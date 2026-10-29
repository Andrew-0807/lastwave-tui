//! Developer commands, built only with `cargo build --features dev`:
//! `--search <query>`, `--discover`, `--selftest <album>` and `--art-debug [log]`.

use crate::{app, artdebug, config, lyrics, model, mpv, ytm};
use std::{sync::mpsc, time::Duration};

/// Runs the developer command named in `args`; false if there is none.
pub fn run(args: &[String]) -> bool {
    if args.first().map(String::as_str) == Some("--search") {
        cli_search(&args[1..].join(" "));
        return true;
    }
    if args.first().map(String::as_str) == Some("--art-debug") {
        artdebug::run(args.get(1).map(String::as_str).unwrap_or("art-debug.log"));
        return true;
    }
    if args.first().map(String::as_str) == Some("--discover") {
        match ytm::home() {
            Ok(shelves) => {
                for s in shelves {
                    println!("== {} ({} items)", s.title, s.items.len());
                    for e in s.items.iter().take(3) {
                        match e {
                            ytm::Entry::Track(t) => println!("   song  {} — {} [{}]", t.title, t.artist, !t.art.is_empty()),
                            ytm::Entry::Collection(c) => println!("   page  {} · {} [{}] [{}]", c.title, c.subtitle, c.browse_id, !c.art.is_empty()),
                        }
                    }
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
        return true;
    }
    if args.first().map(String::as_str) == Some("--selftest") {
        selftest(&args[1..].join(" "));
        return true;
    }
    false
}

fn selftest(q: &str) {
    let Ok(ytm::Results::Collections(albums)) = ytm::search(q, ytm::Kind::Albums) else { return eprintln!("album search failed") };
    let Some(c) = albums.first() else { return eprintln!("no albums") };
    let mut tracks = ytm::browse(&c.browse_id).unwrap_or_default();
    ytm::fill_from_collection(&mut tracks, c);
    println!("album: {} · {} -> {} tracks", c.title, c.subtitle, tracks.len());
    for t in tracks.iter().take(3) {
        println!("  {} — {} · {} {}", t.title, t.artist, t.album, model::fmt_time(t.duration as f64));
    }
    let Some(t) = tracks.first().cloned() else { return };
    match lyrics::fetch(&t) {
        Some(l) => println!("lyrics: {} synced / {} plain lines; first: {:?}", l.synced.len(), l.plain.len(), l.synced.get(3)),
        None => println!("lyrics: none"),
    }
    let (tx, rx) = mpsc::channel();
    let cfg = config::Config::default();
    let mpv = match mpv::Mpv::spawn(&cfg, tx) {
        Ok(m) => m,
        Err(e) => return eprintln!("mpv: {e}"),
    };
    println!("devices: {:?}", mpv.get("audio-device-list").and_then(|v| v.as_array().map(|a| a.len())));
    mpv.cmd(serde_json::json!(["loadfile", t.url(), "replace"]));
    let start = std::time::Instant::now();
    let mut last_t = -1.0;
    while start.elapsed() < Duration::from_secs(25) {
        if let Ok(app::Msg::Mpv(ev)) = rx.recv_timeout(Duration::from_millis(500)) {
            match ev {
                mpv::MpvEvent::Prop(n, v) if n == "time-pos" => {
                    let p = v.as_f64().unwrap_or(0.0);
                    if p - last_t >= 2.0 {
                        println!("time-pos {p:.1}");
                        last_t = p;
                    }
                }
                mpv::MpvEvent::Prop(n, v) if !v.is_null() => println!("{n} = {v}"),
                mpv::MpvEvent::EndFile(r) => println!("end-file {r}"),
                mpv::MpvEvent::Exited => break,
                _ => {}
            }
        }
        if last_t > 6.0 {
            break;
        }
    }
    let mut mpv = mpv;
    mpv.quit();
}

/// Non-interactive catalogue check: `lastwave-tui --search <query>`.
fn cli_search(q: &str) {
    match ytm::search(q, ytm::Kind::Songs) {
        Ok(ytm::Results::Tracks(t)) => {
            for t in t {
                println!("{:>6}  {} — {} · {}  [{}]", model::fmt_time(t.duration as f64), t.title, t.artist, t.album, t.url());
            }
        }
        Ok(ytm::Results::Collections(c)) => c.iter().for_each(|c| println!("{} · {} [{}]", c.title, c.subtitle, c.browse_id)),
        Err(e) => eprintln!("error: {e}"),
    }
}
