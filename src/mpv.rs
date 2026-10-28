//! mpv as a child process, driven over its JSON IPC (named pipe on Windows,
//! unix socket elsewhere). Two connections: one for request/response
//! commands, one that only receives property changes and events — a
//! synchronous Windows pipe handle can't read and write concurrently.

use crate::{app::Msg, config::Config};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

pub enum MpvEvent {
    Prop(String, Value),
    EndFile(String),
    Exited,
}

struct Cmd {
    args: Value,
    reply: Option<Sender<Value>>,
}

pub struct Mpv {
    child: Child,
    tx: Sender<Cmd>,
}

const OBSERVED: &[&str] = &[
    "time-pos",
    "duration",
    "pause",
    "volume",
    "path",
    "idle-active",
    "audio-codec-name",
    "audio-params",
    "audio-out-params",
    "audio-bitrate",
    "current-ao",
    "af",
];

type Conn = (Box<dyn Read + Send>, Box<dyn Write + Send>);

#[cfg(windows)]
fn ipc_path() -> String {
    format!(r"\\.\pipe\lastwave-tui-{}", std::process::id())
}
#[cfg(unix)]
fn ipc_path() -> String {
    std::env::temp_dir().join(format!("lastwave-tui-{}.sock", std::process::id())).to_string_lossy().into_owned()
}

#[cfg(windows)]
fn connect(path: &str) -> std::io::Result<Conn> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let w = f.try_clone()?;
    Ok((Box::new(f), Box::new(w)))
}
#[cfg(unix)]
fn connect(path: &str) -> std::io::Result<Conn> {
    let s = std::os::unix::net::UnixStream::connect(path)?;
    let w = s.try_clone()?;
    Ok((Box::new(s), Box::new(w)))
}

fn connect_retry(path: &str) -> Result<Conn, String> {
    let start = Instant::now();
    loop {
        match connect(path) {
            Ok(c) => return Ok(c),
            Err(e) if start.elapsed() > Duration::from_secs(8) => return Err(format!("mpv IPC: {e}")),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

pub fn args(cfg: &Config, ipc: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "--idle=yes",
        "--no-video",
        "--vid=no",
        "--no-terminal",
        "--force-window=no",
        "--audio-display=no",
        "--ytdl=yes",
        "--prefetch-playlist=yes",
        "--cache=yes",
        "--demuxer-max-bytes=16MiB",
        "--demuxer-max-back-bytes=4MiB",
        "--keep-open=no",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(format!("--input-ipc-server={ipc}"));
    a.push(format!("--ytdl-format={}", cfg.ytdl_format));
    a.push(format!("--volume={}", cfg.volume));
    a.push(format!("--audio-device={}", cfg.audio_device));
    a.push(format!("--gapless-audio={}", if cfg.gapless { "yes" } else { "no" }));
    if cfg.exclusive {
        a.push("--audio-exclusive=yes".into());
        a.push("--replaygain=no".into());
    } else {
        a.push(format!("--replaygain={}", cfg.replaygain));
        let af = cfg.eq.filter();
        if !af.is_empty() {
            a.push(format!("--af={af}"));
        }
    }
    let raw = crate::config::ytdl_raw_options(cfg);
    if !raw.is_empty() {
        a.push(format!("--ytdl-raw-options={raw}"));
    }
    a.extend(cfg.extra_mpv_args.iter().cloned());
    a
}

impl Mpv {
    pub fn spawn(cfg: &Config, events: Sender<Msg>) -> Result<Mpv, String> {
        let ipc = ipc_path();
        let mut cmd = Command::new(&cfg.mpv_path);
        cmd.args(args(cfg, &ipc)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| format!("could not start mpv ({}): {e}", cfg.mpv_path))?;

        let (ev_r, mut ev_w) = match connect_retry(&ipc) {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        for (i, p) in OBSERVED.iter().enumerate() {
            let line = json!({"command": ["observe_property", i + 1, p]}).to_string() + "\n";
            ev_w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        }
        thread::spawn(move || {
            let _keep = ev_w;
            for line in BufReader::new(ev_r).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let ev = match v["event"].as_str() {
                    Some("property-change") => {
                        MpvEvent::Prop(v["name"].as_str().unwrap_or_default().to_string(), v["data"].clone())
                    }
                    Some("end-file") => MpvEvent::EndFile(v["reason"].as_str().unwrap_or_default().to_string()),
                    _ => continue,
                };
                if events.send(Msg::Mpv(ev)).is_err() {
                    return;
                }
            }
            let _ = events.send(Msg::Mpv(MpvEvent::Exited));
        });

        let (cmd_r, mut cmd_w) = connect_retry(&ipc)?;
        let (tx, rx) = mpsc::channel::<Cmd>();
        thread::spawn(move || {
            let mut reader = BufReader::new(cmd_r);
            let mut id: u64 = 0;
            let mut line = String::new();
            let first = Cmd { args: json!(["disable_event", "all"]), reply: None };
            for c in std::iter::once(first).chain(rx) {
                id += 1;
                let msg = json!({"command": c.args, "request_id": id}).to_string() + "\n";
                if cmd_w.write_all(msg.as_bytes()).is_err() {
                    return;
                }
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if v["request_id"].as_u64() == Some(id) {
                        if let Some(r) = &c.reply {
                            let _ = r.send(v);
                        }
                        break;
                    }
                }
            }
        });
        Ok(Mpv { child, tx })
    }

    pub fn cmd(&self, args: Value) {
        let _ = self.tx.send(Cmd { args, reply: None });
    }

    pub fn set(&self, prop: &str, value: impl Into<Value>) {
        self.cmd(json!(["set_property", prop, value.into()]));
    }

    pub fn get(&self, prop: &str) -> Option<Value> {
        let (rtx, rrx) = mpsc::channel();
        self.tx.send(Cmd { args: json!(["get_property", prop]), reply: Some(rtx) }).ok()?;
        let v = rrx.recv_timeout(Duration::from_secs(2)).ok()?;
        (v["error"] == "success").then(|| v["data"].clone())
    }

    pub fn quit(&mut self) {
        self.cmd(json!(["quit"]));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(800) {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
    }
}
