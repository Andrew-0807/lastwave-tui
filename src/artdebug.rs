//! `lastwave-tui --art-debug <log>`: records what the terminal answers to the
//! image-capability queries and draws one test image per protocol, so cover
//! problems can be diagnosed from outside the terminal.

use crate::art;
use image::{DynamicImage, Rgb, RgbImage};
use ratatui::{
    crossterm::terminal,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Span},
};
use ratatui_image::{picker::ProtocolType, StatefulImage};
use std::{
    fmt::Write as _,
    io::{Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn escape(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x1b => "<ESC>".to_string(),
            0x07 => "<BEL>".to_string(),
            b if (0x20..0x7f).contains(&b) => (b as char).to_string(),
            b => format!("<{b:02x}>"),
        })
        .collect()
}

/// Quadrants of known colours with a white diagonal: easy to judge aspect and colour.
fn test_image() -> DynamicImage {
    let mut img = RgbImage::new(240, 240);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = match (x < 120, y < 120) {
            _ if x.abs_diff(y) < 4 => Rgb([255, 255, 255]),
            (true, true) => Rgb([230, 60, 60]),
            (false, true) => Rgb([60, 200, 90]),
            (true, false) => Rgb([60, 110, 230]),
            (false, false) => Rgb([240, 200, 60]),
        };
    }
    DynamicImage::ImageRgb8(img)
}

pub fn run(log_path: &str) {
    let mut log = String::new();
    for k in ["TERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "COLORTERM", "WT_SESSION", "WEZTERM_EXECUTABLE", "WEZTERM_PANE", "RIO_CONFIG", "LC_TERMINAL"] {
        let _ = writeln!(log, "env {k}={:?}", std::env::var(k).ok());
    }

    // 1. What the app picks (first: a timed-out query thread would steal later replies).
    let t0 = Instant::now();
    let detected = art::detect_picker();
    let _ = writeln!(log, "picker query took {:?}", t0.elapsed());
    let _ = writeln!(log, "picker: protocol={:?} font={:?} caps={:?}", detected.protocol_type(), detected.font_size(), detected.capabilities());
    let _ = writeln!(log, "app uses (auto): {:?}", art::Covers::new(detected.clone(), "auto").mode_name());

    // 2. Raw replies to the same queries ratatui-image sends.
    let _ = terminal::enable_raw_mode();
    let query = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b[14t\x1b[5n";
    let _ = std::io::stdout().write_all(query.as_bytes());
    let _ = std::io::stdout().flush();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 256];
        while let Ok(n) = std::io::stdin().read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut replies = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(100)) {
            replies.extend(b);
        }
    }
    let _ = terminal::disable_raw_mode();
    let _ = writeln!(log, "raw replies ({} bytes): {}", replies.len(), escape(&replies));

    let _ = std::fs::write(log_path, &log);

    let _ = writeln!(log, "crossterm supports_ansi={}", ratatui::crossterm::ansi_support::supports_ansi());

    // 3. One test image per protocol, side by side, for a screenshot.
    let mut term = ratatui::init();
    let protos = [ProtocolType::Halfblocks, ProtocolType::Sixel, ProtocolType::Iterm2, ProtocolType::Kitty];
    let mut states: Vec<_> = protos
        .iter()
        .map(|p| {
            let mut pk = detected.clone();
            pk.set_protocol_type(*p);
            pk.new_resize_protocol(test_image())
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let _ = term.draw(|f| {
            let [top, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(f.area());
            f.render_widget(
                Line::from(vec![
                    Span::styled(" RGB-BG ", Style::new().bg(Color::Rgb(230, 60, 60))),
                    Span::styled(" RGB-FG ", Style::new().fg(Color::Rgb(60, 200, 90))),
                    Span::styled(" IDX-BG ", Style::new().bg(Color::Indexed(33))),
                    Span::styled(" RED-BG ", Style::new().bg(Color::Red)),
                    Span::raw(" ▀▄█ "),
                ]),
                top,
            );
            let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(rest);
            for (i, c) in cols.iter().enumerate() {
                let [label, img] = Layout::vertical([Constraint::Length(1), Constraint::Length(12)]).areas(*c);
                f.render_widget(Line::raw(format!("{:?}", protos[i])), label);
                f.render_stateful_widget(StatefulImage::default(), img, &mut states[i]);
            }
        });
        thread::sleep(Duration::from_millis(100));
    }
    ratatui::restore();
    for (i, s) in states.iter_mut().enumerate() {
        let _ = writeln!(log, "{:?} encode result: {:?}", protos[i], s.last_encoding_result());
    }
    let _ = std::fs::write(log_path, &log);
}
