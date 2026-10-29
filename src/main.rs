mod aifilter;
mod app;
mod art;
mod artdebug;
mod config;
mod eq;
mod lastfm;
mod library;
mod lyrics;
mod model;
mod mpv;
mod ui;
mod ytm;

use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
};
use std::{
    panic::{self, AssertUnwindSafe},
    sync::mpsc,
    time::{Duration, Instant},
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--search") {
        cli_search(&args[1..].join(" "));
        return;
    }
    if args.first().map(String::as_str) == Some("--art-debug") {
        artdebug::run(args.get(1).map(String::as_str).unwrap_or("art-debug.log"));
        return;
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
        return;
    }
    if args.first().map(String::as_str) == Some("--selftest") {
        selftest(&args[1..].join(" "));
        return;
    }

    let (tx, rx) = mpsc::channel();
    let mut app = app::App::new(tx);
    let mut terminal = ratatui::init();
    let guard = TerminalGuard;
    install_panic_hook();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // A panic in the UI thread is caught so the queue/settings still get
    // saved and mpv stopped below; the hook has already fixed the terminal.
    let run = panic::catch_unwind(AssertUnwindSafe(|| -> std::io::Result<()> {
        app.covers = Some(art::Covers::new(art::detect_picker(), &app.cfg.image_protocol));
        app.start();
        let mut last_draw = Instant::now();
        while !app.quit {
            // Redraw when something changed, and on a tick for the position
            // bar, lyrics sync and anything else that moves with time.
            if app.dirty || last_draw.elapsed() >= TICK {
                app.dirty = false;
                last_draw = Instant::now();
                terminal.draw(|f| ui::draw(f, &mut app))?;
                if let Some(c) = &mut app.covers {
                    c.flush_overlays(&mut std::io::stdout());
                }
            }
            let wait = TICK.saturating_sub(last_draw.elapsed()).min(MAX_WAIT);
            if event::poll(wait)? {
                // Drain whatever arrives with it: without bracketed paste
                // (Windows console) a paste is a burst of key presses.
                let mut batch = vec![event::read()?];
                while batch.len() < MAX_BURST && event::poll(BURST_GAP)? {
                    batch.push(event::read()?);
                }
                // Pointer motion does nothing (drag isn't used either).
                batch.retain(|e| !matches!(e, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_))));
                if !batch.is_empty() {
                    app.dirty = true;
                    dispatch(&mut app, batch);
                }
            }
            while let Ok(m) = rx.try_recv() {
                app.on_msg(m);
            }
        }
        Ok(())
    }));
    let panicked = run.is_err();
    if !panicked {
        if let Some(c) = &mut app.covers {
            c.begin_frame();
            c.flush_overlays(&mut std::io::stdout());
        }
    }
    // Flushes a pending scrobble, saves config + queue and stops mpv.
    let _ = panic::catch_unwind(AssertUnwindSafe(|| app.shutdown()));
    drop(terminal);
    drop(guard);
    if let Ok(Err(e)) = run {
        eprintln!("lastwave-tui: {e}");
    }
    if panicked {
        std::process::exit(101);
    }
}

/// Redraw at least this often (position bar, lyrics).
const TICK: Duration = Duration::from_millis(250);
/// Longest wait for input before checking background messages.
const MAX_WAIT: Duration = Duration::from_millis(100);
/// Upper bound on events gathered into one batch.
const MAX_BURST: usize = 1 << 16;
/// Events closer together than this are treated as one burst.
const BURST_GAP: Duration = Duration::from_millis(2);

/// Puts the terminal back (raw mode, alternate screen, mouse capture,
/// bracketed paste) however main() is left, including by unwinding.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

/// Main-thread panics restore the terminal before the message is printed
/// (ratatui's own hook only undoes raw mode and the alternate screen).
/// Background-thread panics only end that thread, so they're logged to a
/// file instead of being printed over the UI.
fn install_panic_hook() {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore_terminal();
            prev(info);
        } else {
            use std::io::Write;
            let path = config::dir().join("panic.log");
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{:?}: {info}", std::thread::current().name());
            }
        }
    }));
}

/// Hands a batch of terminal events to the app. A run of two or more typed
/// characters that arrived together is a paste the terminal didn't mark as
/// one, so it goes through `on_paste` instead of firing a hotkey per letter
/// (a pasted "queen" would otherwise quit on its `q`).
fn dispatch(app: &mut app::App, batch: Vec<Event>) {
    let mut run: Vec<KeyEvent> = Vec::new();
    for ev in batch {
        match ev {
            // Windows reports releases too; they don't break a run.
            Event::Key(k) if k.kind != KeyEventKind::Press => {}
            Event::Key(k) if is_text(&k) => run.push(k),
            ev => {
                flush_run(app, &mut run);
                handle(app, ev);
            }
        }
    }
    flush_run(app, &mut run);
}

fn handle(app: &mut app::App, ev: Event) {
    match ev {
        // Quits from anywhere, including while a text field has focus.
        Event::Key(k) if k.code == KeyCode::Char('c') && k.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL => {
            app.quit = true
        }
        Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
        Event::Mouse(m) => app.on_mouse(m),
        Event::Paste(p) => app.on_paste(p),
        _ => {}
    }
}

fn flush_run(app: &mut app::App, run: &mut Vec<KeyEvent>) {
    let chars: Vec<char> = run.iter().filter_map(|k| if let KeyCode::Char(c) = k.code { Some(c) } else { None }).collect();
    // Identical letters in a row are key repeat or a backlog of one hotkey.
    if chars.len() >= 2 && chars.iter().any(|&c| c != chars[0]) {
        let text = run
            .drain(..)
            .filter_map(|k| match k.code {
                KeyCode::Char(c) => Some(c),
                KeyCode::Enter => Some('\n'),
                KeyCode::Tab => Some('\t'),
                _ => None,
            })
            .collect();
        handle(app, Event::Paste(text));
    } else {
        for k in run.drain(..) {
            handle(app, Event::Key(k));
        }
    }
}

/// A key that types text: a character (shift and AltGr allowed), Enter or Tab.
fn is_text(k: &KeyEvent) -> bool {
    let m = k.modifiers - KeyModifiers::SHIFT;
    matches!(k.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab)
        && (m.is_empty() || m == KeyModifiers::CONTROL | KeyModifiers::ALT)
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
