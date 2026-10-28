use crate::{
    app::{App, DItem, Highlight, Input, LibMode, View, SETTINGS},
    art, eq,
    model::{fmt_time, Track},
    ytm::{self, Collection, Results},
};
use ratatui::{
    crossterm::event::KeyCode,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, LineGauge, List, ListItem, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;

const ACCENT: Color = Color::Rgb(255, 208, 176);
const DIM: Color = Color::DarkGray;
const OK: Color = Color::Rgb(140, 220, 150);

fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(title.into())
}

fn hl() -> Style {
    Style::new().bg(Color::Rgb(60, 52, 48)).fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.buttons.clear();
    app.list_area = None;
    app.term_height = f.area().height;
    if let Some(c) = &mut app.covers {
        c.begin_frame();
    }
    let [top, main, player] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(7)]).areas(f.area());
    draw_tabs(f, top, app);
    let has_drawer = matches!(app.view, View::Search | View::Queue | View::Library) && app.cfg.show_details;
    let (main, drawer) = if has_drawer { split_drawer(main) } else { (main, None) };
    if let Some((area, side)) = drawer {
        draw_details(f, area, app, side);
    }
    match app.view {
        View::Search => draw_search(f, main, app),
        View::Queue => draw_queue(f, main, app),
        View::Library => draw_library(f, main, app),
        View::Lyrics => draw_lyrics(f, main, app),
        View::Eq => draw_eq(f, main, app),
        View::Settings => draw_settings(f, main, app),
    }
    draw_player(f, player, app);
    if app.help {
        draw_help(f, f.area());
        // Kitty overlays sit above all text; keep them off the popup.
        if let Some(c) = &mut app.covers {
            c.begin_frame();
        }
    }
}

/// Wide: drawer on the right. Narrow: drawer below. Too small: no drawer.
fn split_drawer(main: Rect) -> (Rect, Option<(Rect, bool)>) {
    if main.width >= 100 {
        let w = (main.width * 32 / 100).clamp(34, 56);
        let [list, side] = Layout::horizontal([Constraint::Min(40), Constraint::Length(w)]).areas(main);
        (list, Some((side, true)))
    } else if main.height >= 18 {
        let h = (main.height * 40 / 100).clamp(9, 14);
        let [list, below] = Layout::vertical([Constraint::Min(6), Constraint::Length(h)]).areas(main);
        (list, Some((below, false)))
    } else {
        (main, None)
    }
}

/// Lay out clickable `[ label ]` buttons left to right, wrapping into rows.
/// Returns the lines to render; registers each button's hit area on `app`.
fn button_rows(app: &mut App, area: Rect, buttons: &[(&str, KeyCode)]) -> Vec<Line<'static>> {
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let (mut x, mut y) = (0u16, 0u16);
    for (label, code) in buttons {
        let span = Span::styled(format!(" {label} "), Style::new().fg(Color::Black).bg(ACCENT).bold());
        let w = span.width() as u16;
        if x > 0 && x + w > area.width {
            lines.push(Vec::new());
            (x, y) = (0, y + 1);
        }
        if y < area.height {
            app.buttons.push((Rect::new(area.x + x, area.y + y, w.min(area.width), 1), *code));
        }
        let row = lines.last_mut().unwrap();
        row.push(span);
        row.push(Span::raw(" "));
        x += w + 1;
    }
    lines.into_iter().map(Line::from).collect()
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &mut App) {
    let mut spans = vec![Span::styled(" LastWave ", Style::new().bg(ACCENT).fg(Color::Black).bold()), Span::raw(" ")];
    let mut x = area.x + 11;
    for (i, v) in View::ALL.iter().enumerate() {
        let style = if *v == app.view { Style::new().fg(ACCENT).bold().underlined() } else { Style::new().fg(DIM) };
        let label = format!("{} {}", i + 1, v.title());
        let w = label.chars().count() as u16;
        app.buttons.push((Rect::new(x, area.y, w, 1), KeyCode::Char((b'1' + i as u8) as char)));
        x += w + 3;
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("   "));
    }
    let hint = if app.cfg.show_details { "v hide details" } else { "v show details" };
    spans.push(Span::styled(format!("· {hint} · ? keys"), Style::new().fg(DIM)));
    f.render_widget(Line::from(spans), area);
}

fn draw_cover(f: &mut Frame, r: Rect, app: &mut App, slot: usize, key: &str) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    if !app.render_cover(f, r, slot, key) {
        let pad = r.height.saturating_sub(1) / 2;
        let mut lines = vec![Line::raw(""); pad as usize];
        lines.push(Line::raw("♪"));
        let ph = Paragraph::new(lines).alignment(Alignment::Center).style(Style::new().fg(DIM).bg(Color::Rgb(40, 36, 34)));
        f.render_widget(ph, r);
    }
}

fn draw_details(f: &mut Frame, area: Rect, app: &mut App, side: bool) {
    let b = block(" Details ").title_bottom(Line::styled(" v hide ", Style::new().fg(DIM)).right_aligned());
    let inner = b.inner(area);
    f.render_widget(b, area);
    let Some(h) = app.highlighted() else {
        f.render_widget(Paragraph::new("Highlight a song to see it here").fg(DIM).alignment(Alignment::Center), inner);
        return;
    };

    let (cover, text) = if side {
        let ch = (inner.width / 2).min(inner.height.saturating_sub(9));
        let cw = (ch * 2).min(inner.width);
        let cover = Rect::new(inner.x + (inner.width - cw) / 2, inner.y, cw, ch);
        let gap = if ch > 0 { ch + 1 } else { 0 };
        (cover, Rect::new(inner.x, inner.y + gap, inner.width, inner.height.saturating_sub(gap)))
    } else {
        let cw = (inner.height * 2).min(inner.width / 3);
        let cover = Rect::new(inner.x, inner.y, cw, inner.height);
        let gap = if cw > 0 { cw + 2 } else { 0 };
        (cover, Rect::new(inner.x + gap, inner.y, inner.width.saturating_sub(gap), inner.height))
    };

    let field = |k: &str, v: String| {
        Line::from(vec![Span::styled(format!("{k:<8}"), Style::new().fg(DIM)), Span::raw(v)])
    };
    let (key, mut lines, buttons): (String, Vec<Line>, Vec<(&str, KeyCode)>) = match &h {
        Highlight::Track(t) => {
            let mut l = vec![Line::styled(t.title.clone(), Style::new().fg(ACCENT).bold()), Line::raw(t.artist.clone()), Line::raw("")];
            if !t.album.is_empty() {
                l.push(field("Album", t.album.clone()));
            }
            if t.duration > 0 {
                l.push(field("Length", fmt_time(t.duration as f64)));
            }
            match &t.source {
                crate::model::Source::Yt(id) => l.push(field("Source", format!("YouTube Music · {id}"))),
                crate::model::Source::Local(p) => {
                    l.push(field("Quality", t.quality.clone()));
                    l.push(field("File", p.to_string_lossy().into_owned()));
                }
            }
            if app.current().is_some_and(|c| c.url() == t.url()) {
                l.push(Line::styled("● Now playing", Style::new().fg(OK)));
            }
            let mut btn: Vec<(&str, KeyCode)> = if app.view == View::Queue {
                vec![("▶ Play", KeyCode::Enter), ("⤵ Play next", KeyCode::Char('N')), ("✕ Remove", KeyCode::Char('d'))]
            } else {
                vec![("▶ Play", KeyCode::Enter), ("⤵ Play next", KeyCode::Char('N')), ("+ Queue", KeyCode::Char('a'))]
            };
            if app.view == View::Search && t.video_id().is_some() {
                btn.push(("≈ Radio", KeyCode::Char('r')));
            }
            (t.art_key(), l, btn)
        }
        Highlight::Collection(c) => {
            let kind = match c.browse_id.get(..2) {
                Some("MP") => "Album",
                Some("UC") => "Artist",
                _ => "Playlist",
            };
            let l = vec![
                Line::styled(c.title.clone(), Style::new().fg(ACCENT).bold()),
                Line::raw(c.subtitle.clone()),
                Line::raw(""),
                field("Type", kind.into()),
            ];
            (c.art.clone(), l, vec![("↵ Open", KeyCode::Enter)])
        }
    };

    let btn_lines = button_rows(app, Rect::new(text.x, text.y, text.width, text.height), &buttons);
    // Buttons go at the bottom of the text area: shift their hit areas down.
    let nb = btn_lines.len() as u16;
    let btn_y = text.y + text.height.saturating_sub(nb);
    let first = app.buttons.len() - app.buttons.iter().rev().take_while(|(r, _)| r.y >= text.y && r.x >= text.x && r.y < text.y + nb).count();
    for (r, _) in app.buttons[first..].iter_mut() {
        r.y = btn_y + (r.y - text.y);
    }
    lines.truncate(text.height.saturating_sub(nb + 1) as usize);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), Rect { height: text.height.saturating_sub(nb + 1), ..text });
    f.render_widget(Paragraph::new(btn_lines), Rect::new(text.x, btn_y, text.width, nb.min(text.height)));
    draw_cover(f, cover, app, art::DETAIL, &key);
}

fn track_item(t: &Track, playing: bool) -> ListItem<'static> {
    let dur = if t.duration > 0 { fmt_time(t.duration as f64) } else { String::new() };
    let mut spans = vec![
        Span::styled(if playing { "▶ " } else { "  " }, Style::new().fg(ACCENT)),
        Span::styled(format!("{dur:>6}  "), Style::new().fg(DIM)),
        Span::styled(t.title.clone(), if playing { Style::new().fg(ACCENT).bold() } else { Style::new().bold() }),
        Span::raw("  "),
        Span::raw(t.artist.clone()),
    ];
    if !t.album.is_empty() {
        spans.push(Span::styled(format!(" · {}", t.album), Style::new().fg(DIM)));
    }
    if !t.quality.is_empty() {
        spans.push(Span::styled(format!("  [{}]", t.quality), Style::new().fg(OK)));
    }
    ListItem::new(Line::from(spans))
}

fn coll_item(c: &Collection) -> ListItem<'static> {
    let kind = match c.browse_id.get(..2) {
        Some("MP") => "album",
        Some("UC") => "artist",
        _ => "playlist",
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("  {kind:<9}"), Style::new().fg(DIM)),
        Span::styled(c.title.clone(), Style::new().bold()),
        Span::styled(format!("  {}", c.subtitle), Style::new().fg(DIM)),
    ]))
}

fn input_line(app: &App, target: Input, value: &str, placeholder: &str) -> Line<'static> {
    if app.input == Some(target) {
        Line::from(vec![Span::raw(app.input_buf.clone()), Span::styled("▏", Style::new().fg(ACCENT))])
    } else if value.is_empty() {
        Line::styled(placeholder.to_string(), Style::new().fg(DIM))
    } else {
        Line::raw(value.to_string())
    }
}

fn draw_search(f: &mut Frame, area: Rect, app: &mut App) {
    let [inp, list] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
    let title = format!(" Search YouTube Music · {} (f) ", app.kind.label());
    let line = input_line(app, Input::Search, &app.query, "Press / to search songs, artists, albums — or paste a YouTube link. Empty = Discover");
    f.render_widget(Paragraph::new(line).block(block(title)), inp);

    if let Some((c, tracks)) = &app.browse {
        let items: Vec<ListItem> = tracks.iter().map(|t| track_item(t, false)).collect();
        let title = format!(" {} · {} · {} tracks  (Esc back, Enter play from here, A queue all) ", c.title, c.subtitle, tracks.len());
        let w = List::new(items).block(block(title)).highlight_style(hl());
        f.render_stateful_widget(w, list, &mut app.browse_sel);
        app.list_area = Some(list);
        return;
    }
    if app.discover_mode() {
        draw_discover(f, list, app);
        return;
    }
    let busy = if app.searching { " · searching…" } else { "" };
    app.list_area = Some(list);
    match &app.results {
        Some(Results::Tracks(t)) => {
            let items: Vec<ListItem> = t.iter().map(|t| track_item(t, false)).collect();
            let w = List::new(items)
                .block(block(format!(" Songs{busy}  (Enter play, N play next, a queue, r radio, A queue all) ")))
                .highlight_style(hl());
            f.render_stateful_widget(w, list, &mut app.search_sel);
        }
        Some(Results::Collections(cs)) => {
            let items: Vec<ListItem> = cs.iter().map(coll_item).collect();
            let w = List::new(items)
                .block(block(format!(" {}{busy}  (Enter open) ", app.kind.label())))
                .highlight_style(hl());
            f.render_stateful_widget(w, list, &mut app.search_sel);
        }
        None => {
            let msg = if app.searching { "Searching…" } else { "" };
            f.render_widget(Paragraph::new(msg).block(block(" Results ")), list);
        }
    }
}

fn draw_discover(f: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .discover
        .iter()
        .map(|d| match d {
            DItem::Header(t) => ListItem::new(Line::styled(format!("── {t} "), Style::new().fg(ACCENT).bold())),
            DItem::Track(t) => track_item(t, false),
            DItem::Coll(c) => coll_item(c),
        })
        .collect();
    let who = if ytm::signed_in() {
        format!("for {}", app.cfg.yt_user)
    } else {
        "· YouTube Music home (sign in for your own picks)".into()
    };
    let busy = if app.discover_loading { " · loading…" } else { "" };
    let title = format!(" Discover {who}{busy}  (Enter play/open, N next, a queue, r radio, S refresh) ");
    if items.is_empty() {
        let msg = if app.discover_loading { "Loading recommendations…" } else { "Nothing to show — press S to retry" };
        f.render_widget(Paragraph::new(msg).fg(DIM).block(block(title)), area);
        return;
    }
    let w = List::new(items).block(block(title)).highlight_style(hl());
    f.render_stateful_widget(w, area, &mut app.discover_sel);
    app.list_area = Some(area);
}

fn draw_queue(f: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app.queue.iter().enumerate().map(|(i, t)| track_item(t, app.cur == Some(i))).collect();
    let title = format!(
        " Queue · {} tracks · radio {}  (Enter play, N play next, d remove, J/K move, c current, X clear) ",
        app.queue.len(),
        if app.cfg.radio { "on" } else { "off" }
    );
    let w = List::new(items).block(block(title)).highlight_style(hl());
    f.render_stateful_widget(w, area, &mut app.queue_sel);
    app.list_area = Some(area);
}

fn draw_library(f: &mut Frame, area: Rect, app: &mut App) {
    let [top, list] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
    let yt = app.lib_mode == LibMode::Yt;
    let title = if yt { " Library · y switch · S refresh " } else { " Library · y switch · f filter · o folder · S rescan " };
    let b = block(title);
    let inner = b.inner(top);
    f.render_widget(b, top);

    // Source tabs (clickable).
    let mut spans = Vec::new();
    let mut x = inner.x;
    for (label, mode, code) in [(" Local files ", LibMode::Local, KeyCode::F(11)), (" YouTube Music ", LibMode::Yt, KeyCode::F(12))] {
        let style = if app.lib_mode == mode { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().fg(DIM) };
        let w = label.chars().count() as u16;
        app.buttons.push((Rect::new(x, inner.y, w, 1), code));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("  "));
        x += w + 2;
    }
    if yt {
        let who = if ytm::signed_in() { format!("signed in as {}", app.cfg.yt_user) } else { "not signed in".into() };
        spans.push(Span::styled(who, Style::new().fg(DIM)));
    } else if app.input == Some(Input::LibDir) {
        spans.push(Span::raw(format!("folder: {}▏", app.input_buf)));
    } else {
        let mut l = input_line(app, Input::LibFilter, &app.lib_filter, "f to filter");
        l.spans.insert(0, Span::styled("filter: ", Style::new().fg(DIM)));
        spans.extend(l.spans);
    }
    f.render_widget(Line::from(spans), inner);

    if yt {
        draw_library_yt(f, list, app);
        return;
    }
    let vis = app.lib_visible();
    let items: Vec<ListItem> = vis.iter().map(|&i| track_item(&app.library[i], false)).collect();
    let scanning = if app.lib_scanning { " · scanning…" } else { "" };
    let title = format!(
        " {} · {} of {}{scanning}  (Enter play, N play next, a queue, A queue all) ",
        app.cfg.library_dir,
        vis.len(),
        app.library.len()
    );
    let w = List::new(items).block(block(title)).highlight_style(hl());
    f.render_stateful_widget(w, list, &mut app.lib_sel);
    app.list_area = Some(list);
}

fn draw_library_yt(f: &mut Frame, area: Rect, app: &mut App) {
    if !ytm::signed_in() {
        let b = block(" YouTube Music library ");
        let inner = b.inner(area);
        f.render_widget(b, area);
        let mut lines = vec![
            Line::raw("Sign in to see Liked Music and your playlists, and get a personal Discover feed."),
            Line::raw(""),
            Line::styled("How: open music.youtube.com signed in → F12 → Network → click any request", Style::new().fg(DIM)),
            Line::styled("→ copy the whole `cookie` request header → paste it here (or a cookies.txt path).", Style::new().fg(DIM)),
            Line::raw(""),
        ];
        let y = inner.y + lines.len() as u16;
        let btn = button_rows(app, Rect::new(inner.x, y, inner.width, 1), &[("Sign in (L)", KeyCode::Char('L'))]);
        lines.extend(btn);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        return;
    }
    let busy = if app.yt_loading { " · loading…" } else { "" };
    if let Some((c, tracks)) = &app.yt_open {
        let items: Vec<ListItem> = tracks.iter().map(|t| track_item(t, false)).collect();
        let title = format!(" {} · {} tracks{busy}  (Esc back, Enter play from here, N next, a queue, A all, r radio) ", c.title, tracks.len());
        let w = List::new(items).block(block(title)).highlight_style(hl());
        f.render_stateful_widget(w, area, &mut app.yt_open_sel);
    } else {
        let items: Vec<ListItem> = app.yt_lists.iter().map(coll_item).collect();
        let title = format!(" Your playlists · {}{busy}  (Enter open) ", app.yt_lists.len());
        let w = List::new(items).block(block(title)).highlight_style(hl());
        f.render_stateful_widget(w, area, &mut app.yt_lists_sel);
    }
    app.list_area = Some(area);
}

fn draw_lyrics(f: &mut Frame, area: Rect, app: &App) {
    let title = app.current().map(|t| format!(" {} — {} ", t.title, t.artist)).unwrap_or(" Lyrics ".into());
    let b = block(title).title_bottom(Line::from(" lyrics: LRCLIB ").right_aligned());
    let inner = b.inner(area);
    f.render_widget(b, area);
    let Some(l) = &app.lyrics else {
        let msg = if app.lyrics_loading { "Loading lyrics…" } else { "No lyrics found" };
        f.render_widget(Paragraph::new(msg).fg(DIM).alignment(Alignment::Center), inner);
        return;
    };
    let h = inner.height as usize;
    if l.synced.is_empty() {
        let lines: Vec<Line> = l.plain.iter().map(|s| Line::raw(s.clone())).collect();
        let scroll = if app.dur > 0.0 {
            ((app.pos / app.dur) * lines.len().saturating_sub(h) as f64) as u16
        } else {
            0
        };
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true }).scroll((scroll, 0)), inner);
        return;
    }
    let cur = l.synced.iter().rposition(|(t, _)| *t <= app.pos + 0.15);
    let center = cur.unwrap_or(0);
    let start = center.saturating_sub(h / 2);
    let lines: Vec<Line> = l.synced[start..]
        .iter()
        .take(h)
        .enumerate()
        .map(|(k, (_, s))| {
            let i = start + k;
            let text = if s.is_empty() { "♪".to_string() } else { s.clone() };
            let style = match cur {
                Some(c) if c == i => Style::new().fg(ACCENT).bold(),
                Some(c) if i < c => Style::new().fg(DIM),
                _ => Style::new(),
            };
            Line::styled(text, style)
        })
        .collect();
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn draw_eq(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.cfg.eq;
    let state = if app.cfg.exclusive && e.active() {
        "bypassed (bit-perfect on)".to_string()
    } else if e.active() {
        "on".into()
    } else {
        "off".into()
    };
    let title = format!(
        " Equalizer · {state} · preset {} · Studio Clarity {} ({}) ",
        e.preset,
        if e.clarity { "on" } else { "off" },
        eq::CLARITY[e.clarity_preset.min(eq::CLARITY.len() - 1)].0
    );
    let b = block(title).title_bottom(
        Line::from(" ←/→ band  ↑/↓ gain  e toggle  [ ] preset  c clarity  C clarity profile  z reset ").right_aligned(),
    );
    let inner = b.inner(area);
    f.render_widget(b, area);

    let combined = e.combined();
    let col = 7usize;
    let mut lines = Vec::new();
    for level in (-8..=8).rev() {
        let level = level as f64;
        let mut spans = vec![Span::styled(format!("{:+3} dB ", level as i32), Style::new().fg(DIM))];
        for (i, g) in combined.iter().enumerate() {
            let filled = (level > 0.0 && *g >= level - 0.25) || (level < 0.0 && *g <= level + 0.25);
            let cell = if filled {
                "█████"
            } else if level == 0.0 {
                "─────"
            } else {
                "     "
            };
            let color = if i == app.eq_band { ACCENT } else if filled { Color::Rgb(170, 150, 140) } else { DIM };
            spans.push(Span::styled(format!("{cell:^col$}"), Style::new().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    let labels = eq::BANDS.iter().enumerate().map(|(i, hz)| {
        let s = format!("{:^col$}", eq::band_label(*hz));
        if i == app.eq_band { Span::styled(s, Style::new().fg(ACCENT).bold()) } else { Span::raw(s) }
    });
    lines.push(Line::from(std::iter::once(Span::raw("       ")).chain(labels).collect::<Vec<_>>()));
    let gains = combined.iter().map(|g| Span::styled(format!("{:^col$}", format!("{g:+.1}")), Style::new().fg(DIM)));
    lines.push(Line::from(std::iter::once(Span::raw("       ")).chain(gains).collect::<Vec<_>>()));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_settings(f: &mut Frame, area: Rect, app: &App) {
    let [list, info] = Layout::vertical([Constraint::Length(SETTINGS.len() as u16 + 2), Constraint::Min(3)]).areas(area);
    let lines: Vec<Line> = SETTINGS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let editing = match (i, app.input) {
                (6, Some(Input::LibDir)) | (7, Some(Input::LfmKey)) | (8, Some(Input::LfmSecret)) => {
                    Some(format!("{}▏", app.input_buf))
                }
                (10, Some(Input::YtCookies)) => Some(if app.input_buf.is_empty() {
                    "Enter = sign in with clipboard · or paste first · Esc cancel▏".into()
                } else {
                    format!("{} characters pasted — Enter to sign in▏", app.input_buf.chars().count())
                }),
                _ => None,
            };
            let value = editing.unwrap_or_else(|| app.setting_value(i));
            let style = if i == app.set_sel { hl() } else { Style::new() };
            Line::from(vec![
                Span::styled(format!(" {name:<26}"), style),
                Span::styled(format!(" {value}"), if i == app.set_sel { style } else { Style::new().fg(ACCENT) }),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block(" Settings  (↑/↓ select, Enter/←/→ change) ")), list);
    let text = vec![
        Line::raw("Bit-perfect sends the decoded stream to the device in WASAPI exclusive mode (CoreAudio hog / ALSA on other OSes):"),
        Line::raw("no mixer, no resampling, no EQ, no ReplayGain, volume fixed at 100%. Best with local FLAC/ALAC."),
        Line::raw("YouTube streams top out at ~160 kbps Opus / 128-256 kbps AAC; they are resolved by mpv's yt-dlp hook (keep yt-dlp updated)."),
        Line::raw(""),
        Line::raw("YouTube Music sign-in: open music.youtube.com signed in → F12 → Network → any request → copy the `cookie` request header and paste it on \"YouTube Music account\" (a cookies.txt path works too). Cookies are stored in plain text next to the config file; signing out of that browser session invalidates them. \"Use account for playback\" passes them to yt-dlp for Premium formats and age-restricted songs."),
        Line::raw(""),
        Line::styled(format!("Config file: {}", app.config_path()), Style::new().fg(DIM)),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block(" Notes ")), info);
}

fn fmt_params(v: &Value) -> String {
    let Some(rate) = v["samplerate"].as_u64() else { return String::new() };
    let khz = format!("{:.1}", rate as f64 / 1000.0);
    let khz = khz.trim_end_matches(".0");
    format!("{} {khz}kHz {}ch", v["format"].as_str().unwrap_or("?"), v["channel-count"].as_u64().unwrap_or(0))
}

fn draw_player(f: &mut Frame, area: Rect, app: &mut App) {
    let status = format!(" {} ", app.status);
    let b = block(Line::from(" Now playing ")).title_bottom(Line::styled(status, Style::new().fg(DIM)));
    let inner = b.inner(area);
    f.render_widget(b, area);
    // Cover is roughly square: cells are about twice as tall as wide.
    let cw = if inner.width >= 60 { inner.height * 2 } else { 0 };
    let [cover, _, body] =
        Layout::horizontal([Constraint::Length(cw), Constraint::Length(if cw > 0 { 2 } else { 0 }), Constraint::Min(10)])
            .areas(inner);
    let [l1, l2, l3, l4, l5] = Layout::vertical([Constraint::Length(1); 5]).areas(body);

    if let Some(key) = app.current().map(Track::art_key) {
        draw_cover(f, cover, app, art::NOW, &key);
    } else if cw > 0 {
        draw_cover(f, cover, app, art::NOW, "");
    }

    let icon = if app.idle { "■" } else if app.paused { "‖" } else { "▶" };
    match app.current() {
        Some(t) => {
            f.render_widget(
                Line::from(vec![Span::styled(format!("{icon} "), Style::new().fg(ACCENT)), Span::styled(t.title.clone(), Style::new().bold())]),
                l1,
            );
            let mut sub = vec![Span::raw(format!("  {}", t.artist))];
            if !t.album.is_empty() {
                sub.push(Span::styled(format!(" · {}", t.album), Style::new().fg(DIM)));
            }
            f.render_widget(Line::from(sub), l2);
        }
        None => f.render_widget(Line::styled("■ Nothing playing", Style::new().fg(DIM)), l1),
    }

    let ratio = if app.dur > 0.0 { (app.pos / app.dur).clamp(0.0, 1.0) } else { 0.0 };
    let gauge = LineGauge::default()
        .filled_style(Style::new().fg(ACCENT))
        .unfilled_style(Style::new().fg(DIM))
        .label(format!("{} / {} ", fmt_time(app.pos), fmt_time(app.dur)))
        .ratio(ratio);
    f.render_widget(gauge, l3);

    let mut spans = Vec::new();
    if !app.codec.is_empty() && !app.idle {
        spans.push(Span::styled(format!(" {} ", app.codec.to_uppercase()), Style::new().fg(Color::Black).bg(ACCENT)));
        spans.push(Span::raw(format!(" {}", fmt_params(&app.params))));
        if app.bitrate > 0.0 {
            spans.push(Span::raw(format!(" {:.0}kbps", app.bitrate / 1000.0)));
        }
        let out = fmt_params(&app.out_params);
        if !out.is_empty() {
            spans.push(Span::styled(format!("  → {} {}", app.ao, out), Style::new().fg(DIM)));
        }
    } else {
        spans.push(Span::styled("idle", Style::new().fg(DIM)));
    }
    if app.cfg.exclusive {
        if app.bit_perfect() {
            spans.push(Span::styled("  BIT-PERFECT", Style::new().fg(OK).bold()));
        } else {
            spans.push(Span::styled("  exclusive", Style::new().fg(ACCENT)));
        }
    } else if !app.af.is_empty() {
        spans.push(Span::styled("  EQ", Style::new().fg(ACCENT)));
    }
    f.render_widget(Line::from(spans), l4);

    let play = if app.paused || app.idle { "▶ Play" } else { "‖ Pause" };
    let controls = [
        ("◀◀", KeyCode::Char('p')),
        (play, KeyCode::Char(' ')),
        ("▶▶", KeyCode::Char('n')),
        ("−", KeyCode::Char('-')),
        ("+", KeyCode::Char('+')),
    ];
    let mut row = button_rows(app, l5, &controls).into_iter().next().unwrap_or_default();
    row.spans.push(Span::styled(
        format!(" vol {:.0}%{}", app.volume, if app.cfg.radio { " · radio" } else { "" }),
        Style::new().fg(DIM),
    ));
    f.render_widget(row, l5);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows = [
        ("Global", ""),
        ("1-6 / Tab", "switch view"),
        ("Space", "play / pause"),
        ("n / p", "next / previous"),
        ("← → , .", "seek 5s / 10s"),
        ("+ / -", "volume"),
        ("R", "toggle radio autoplay"),
        ("/", "search (from any view; empty = Discover)"),
        ("v", "show / hide details drawer"),
        ("mouse", "click buttons/tabs, double-click plays, wheel scrolls"),
        ("q", "quit"),
        ("", ""),
        ("Lists", ""),
        ("↑↓ j k PgUp PgDn g G", "move"),
        ("Enter", "play / open"),
        ("a / A", "add selected / all to queue"),
        ("N", "play next (after current song)"),
        ("r", "start radio from song (Search)"),
        ("Esc", "back / clear search → Discover"),
        ("S", "refresh Discover / playlists / library"),
        ("y", "Library: Local files ↔ YouTube Music"),
        ("f", "Search: Songs/Albums/…  Library: filter"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            if d.is_empty() {
                Line::styled(k.to_string(), Style::new().fg(ACCENT).bold())
            } else {
                Line::from(vec![Span::styled(format!("  {k:<22}"), Style::new().bold()), Span::raw(d.to_string())])
            }
        })
        .collect();
    let w = 60.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines).block(block(" Keys (any key closes) ")), r);
}
