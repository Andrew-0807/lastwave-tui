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
    widgets::{Block, BorderType, Borders, Clear, LineGauge, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;
use std::sync::OnceLock;

/// Colours. Only indexed colours from the fixed 16-255 range (themes such as
/// Solarized remap 0-15, where DarkGray is the background itself) and the
/// terminal's own foreground, dimmed, for secondary text and borders.
struct Palette {
    accent: Color,
    /// Text drawn on an accent background.
    on_accent: Color,
    ok: Color,
    like: Color,
    /// Selected row background.
    sel_bg: Color,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Bg {
    Dark,
    Light,
    Unknown,
}

impl Palette {
    fn for_bg(bg: Bg) -> Self {
        match bg {
            // The original peach look.
            Bg::Dark => Self { accent: Color::Indexed(216), on_accent: Color::Indexed(16), ok: Color::Indexed(114), like: Color::Indexed(211), sel_bg: Color::Indexed(237) },
            Bg::Light => Self { accent: Color::Indexed(130), on_accent: Color::Indexed(231), ok: Color::Indexed(28), like: Color::Indexed(161), sel_bg: Color::Indexed(253) },
            // Mid-tones that read on both light and dark backgrounds.
            Bg::Unknown => Self { accent: Color::Indexed(173), on_accent: Color::Indexed(16), ok: Color::Indexed(71), like: Color::Indexed(168), sel_bg: Color::Indexed(237) },
        }
    }
}

/// Background brightness: `LASTWAVE_THEME=light|dark`, else COLORFGBG
/// ("fg;bg", set by rxvt, Konsole, iTerm2 and others).
fn background() -> Bg {
    match std::env::var("LASTWAVE_THEME").unwrap_or_default().to_lowercase().as_str() {
        "light" => return Bg::Light,
        "dark" => return Bg::Dark,
        _ => {}
    }
    let v = std::env::var("COLORFGBG").unwrap_or_default();
    match v.rsplit(';').next().and_then(|s| s.parse::<u8>().ok()) {
        Some(7 | 9..=15) => Bg::Light,
        Some(_) => Bg::Dark,
        None => Bg::Unknown,
    }
}

fn pal() -> &'static Palette {
    static P: OnceLock<Palette> = OnceLock::new();
    P.get_or_init(|| Palette::for_bg(background()))
}

fn accent() -> Style {
    Style::new().fg(pal().accent)
}

/// Secondary text: the terminal's foreground, dimmed. Visible on any theme.
fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(dim()).title(title.into())
}

/// Selected row: background only, so per-column colours and the playing
/// marker still show through.
fn hl() -> Style {
    Style::new().bg(pal().sel_bg)
}

/// Display width of `s` in terminal columns (CJK and emoji count 2).
fn str_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Cut `s` to at most `w` columns, ending in "…" when shortened.
fn trunc(s: &str, w: usize) -> String {
    if str_width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let (mut out, mut used, mut buf) = (String::new(), 0, [0u8; 4]);
    for c in s.chars() {
        let cw = str_width(c.encode_utf8(&mut buf));
        if used + cw > w - 1 {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// `trunc`, padded with spaces to exactly `w` columns.
fn fit(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let pad = w.saturating_sub(str_width(&t));
    t + &" ".repeat(pad)
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
    if app.picker.is_some() {
        draw_picker(f, f.area(), app);
        if let Some(c) = &mut app.covers {
            c.begin_frame();
        }
    }
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

/// Render a bordered list, building only the rows that are on screen. Keeps
/// the selection visible and leaves the scroll offset in `state` (mouse
/// clicks map rows through it), like ratatui's own List does.
fn render_list<'a>(
    f: &mut Frame,
    area: Rect,
    block: Block<'a>,
    state: &mut ListState,
    len: usize,
    item: impl FnMut(usize) -> ListItem<'a>,
) {
    let h = block.inner(area).height as usize;
    if h == 0 || area.width < 3 {
        f.render_widget(block, area);
        return;
    }
    if len == 0 {
        state.select(None);
        f.render_widget(block, area);
        return;
    }
    if state.selected().is_some_and(|s| s >= len) {
        state.select(Some(len - 1));
    }
    let mut off = state.offset().min(len.saturating_sub(h));
    if let Some(s) = state.selected() {
        if s < off {
            off = s;
        } else if s >= off + h {
            off = s + 1 - h;
        }
    }
    let items: Vec<ListItem> = (off..len.min(off + h)).map(item).collect();
    let mut window = ListState::default().with_selected(state.selected().map(|s| s - off));
    f.render_stateful_widget(List::new(items).block(block).highlight_style(hl()), area, &mut window);
    *state.offset_mut() = off;
}

/// Lay out clickable `[ label ]` buttons left to right, wrapping into rows.
/// Returns the lines to render; registers each button's hit area on `app`.
/// Outlined buttons: `[label]` in the accent colour, optionally followed by
/// the dimmed key that does the same thing.
fn button_rows(app: &mut App, area: Rect, buttons: &[(&str, KeyCode)], hints: bool) -> Vec<Line<'static>> {
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let (mut x, mut y) = (0u16, 0u16);
    for (label, code) in buttons {
        let mut spans = vec![Span::styled(format!("[{label}]"), accent().bold())];
        let hint = match code {
            KeyCode::Char(c) if hints => Some(c.to_string()),
            KeyCode::Enter if hints => Some("↵".into()),
            _ => None,
        };
        if let Some(h) = hint.filter(|_| !label.contains(['(', '↵'])) {
            spans.push(Span::styled(format!(" {h}"), dim()));
        }
        let w = spans.iter().map(|s| s.width() as u16).sum::<u16>();
        if x > 0 && x + w > area.width {
            lines.push(Vec::new());
            (x, y) = (0, y + 1);
        }
        if y < area.height {
            app.buttons.push((Rect::new(area.x + x, area.y + y, w.min(area.width), 1), *code));
        }
        let row = lines.last_mut().unwrap();
        row.extend(spans);
        row.push(Span::raw("  "));
        x += w + 2;
    }
    lines.into_iter().map(Line::from).collect()
}

/// Short tab names for narrow terminals.
fn short_title(v: View) -> &'static str {
    match v {
        View::Library => "Lib",
        View::Eq => "EQ",
        View::Settings => "Set",
        v => v.title(),
    }
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &mut App) {
    const BRAND: &str = " LastWave ";
    let full_hint = format!("· {} · ? keys", if app.cfg.show_details { "v hide details" } else { "v show details" });
    // The roomiest layout that fits: (short names, gap, trailing hint).
    let variants = [(false, 3, full_hint.as_str()), (false, 3, "· ? keys"), (false, 2, ""), (true, 2, "")];
    let label = |short: bool, i: usize, v: View| format!("{} {}", i + 1, if short { short_title(v) } else { v.title() });
    let width = |&(short, gap, hint): &(bool, u16, &str)| {
        let tabs: usize = View::ALL.iter().enumerate().map(|(i, v)| str_width(&label(short, i, *v)) + gap as usize).sum();
        BRAND.len() + 1 + tabs + str_width(hint)
    };
    let (short, gap, hint) = *variants.iter().find(|v| width(v) <= area.width as usize).unwrap_or(&variants[3]);

    let mut spans = vec![Span::styled(BRAND, Style::new().bg(pal().accent).fg(pal().on_accent).bold()), Span::raw(" ")];
    let mut x = area.x + BRAND.len() as u16 + 1;
    for (i, v) in View::ALL.iter().enumerate() {
        let style = if *v == app.view { accent().bold().underlined() } else { dim() };
        let label = label(short, i, *v);
        let w = str_width(&label) as u16;
        app.buttons.push((Rect::new(x, area.y, w, 1), KeyCode::Char((b'1' + i as u8) as char)));
        x += w + gap;
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" ".repeat(gap as usize)));
    }
    if !hint.is_empty() {
        spans.push(Span::styled(hint.to_string(), dim()));
    }
    f.render_widget(Line::from(spans), area);
}

fn cover_cols(app: &App, rows: u16) -> u16 {
    app.covers.as_ref().map_or(rows * 2, |c| c.cols_for_rows(rows))
}

fn cover_rows(app: &App, cols: u16) -> u16 {
    app.covers.as_ref().map_or(cols / 2, |c| c.rows_for_cols(cols))
}

fn draw_cover(f: &mut Frame, r: Rect, app: &mut App, slot: usize, key: &str) {
    if r.width == 0 || r.height == 0 {
        return;
    }
    if !app.render_cover(f, r, slot, key) {
        let pad = r.height.saturating_sub(1) / 2;
        let mut lines = vec![Line::raw(""); pad as usize];
        lines.push(Line::raw("♪"));
        let ph = Paragraph::new(lines).alignment(Alignment::Center).style(dim().bg(pal().sel_bg));
        f.render_widget(ph, r);
    }
}

fn draw_details(f: &mut Frame, area: Rect, app: &mut App, side: bool) {
    let b = block(" Details ").title_bottom(Line::styled(" v hide ", dim()).right_aligned());
    let inner = b.inner(area);
    f.render_widget(b, area);
    let Some(h) = app.highlighted() else {
        f.render_widget(Paragraph::new("Highlight a song to see it here").dim().alignment(Alignment::Center), inner);
        return;
    };

    // Covers are square on screen: cells are about twice as tall as wide.
    let (cover, text) = if side {
        let ch = cover_rows(app, inner.width).min(inner.height.saturating_sub(9));
        let cw = cover_cols(app, ch).min(inner.width);
        let cover = Rect::new(inner.x + (inner.width - cw) / 2, inner.y, cw, ch);
        let gap = if ch > 0 { ch + 1 } else { 0 };
        (cover, Rect::new(inner.x, inner.y + gap, inner.width, inner.height.saturating_sub(gap)))
    } else {
        let cw = cover_cols(app, inner.height).min(inner.width / 3);
        let cover = Rect::new(inner.x, inner.y, cw, inner.height);
        let gap = if cw > 0 { cw + 2 } else { 0 };
        (cover, Rect::new(inner.x + gap, inner.y, inner.width.saturating_sub(gap), inner.height))
    };

    let field = |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<8}"), dim()), Span::raw(v)]);
    let (key, mut lines, buttons): (String, Vec<Line>, Vec<(&str, KeyCode)>) = match &h {
        Highlight::Track(t) => {
            let mut l = vec![Line::styled(t.title.clone(), accent().bold()), Line::raw(t.artist.clone()), Line::raw("")];
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
            if is_playing(app, t) {
                l.push(Line::styled("▶ Now playing", Style::new().fg(pal().ok)));
            }
            let mut btn: Vec<(&str, KeyCode)> = if app.view == View::Queue {
                vec![("▶ Play", KeyCode::Enter), ("⤵ Play next", KeyCode::Char('N')), ("✕ Remove", KeyCode::Char('d'))]
            } else {
                vec![("▶ Play", KeyCode::Enter), ("⤵ Play next", KeyCode::Char('N')), ("+ Queue", KeyCode::Char('a'))]
            };
            if t.video_id().is_some() {
                btn.push(("≈ Radio", KeyCode::Char('r')));
            }
            if ytm::signed_in() && t.video_id().is_some() {
                btn.push((if app.is_liked(t) { "♥ Unlike" } else { "♡ Like" }, KeyCode::Char('l')));
                btn.push(("+ Playlist", KeyCode::Char('P')));
            }
            (t.art_key(), l, btn)
        }
        Highlight::Collection(c) => {
            let l = vec![
                Line::styled(c.title.clone(), accent().bold()),
                Line::raw(c.subtitle.clone()),
                Line::raw(""),
                field("Type", coll_kind(c).into()),
            ];
            (c.art.clone(), l, vec![("↵ Open", KeyCode::Enter)])
        }
    };

    let btn_lines = button_rows(app, Rect::new(text.x, text.y, text.width, text.height), &buttons, true);
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

fn is_playing(app: &App, t: &Track) -> bool {
    app.current().is_some_and(|c| c.source == t.source)
}

/// Column widths of track rows for a list `width` columns wide (inside the
/// border). Row: marker(2) title [2 artist] [2 album] 1 duration(7).
struct Cols {
    title: usize,
    artist: usize,
    album: usize,
}

impl Cols {
    fn new(width: u16) -> Self {
        let rest = (width as usize).saturating_sub(2 + 8);
        if rest >= 64 {
            let (artist, album) = (rest * 28 / 100, rest * 24 / 100);
            Self { title: rest - artist - album - 4, artist, album }
        } else if rest >= 28 {
            let artist = rest * 38 / 100;
            Self { title: rest - artist - 2, artist, album: 0 }
        } else {
            Self { title: rest, artist: 0, album: 0 }
        }
    }

    fn for_list(area: Rect) -> Self {
        Self::new(area.width.saturating_sub(2))
    }
}

/// One track row, in columns. `playing` adds the ▶ marker and accent title;
/// selection (a background) combines with it.
fn track_item(app: &App, t: &Track, playing: bool, cols: &Cols) -> ListItem<'static> {
    let mut flags = Vec::new();
    if app.is_liked(t) {
        flags.push(Span::styled(" ♥", Style::new().fg(pal().like)));
    }
    if app.is_ai(t) {
        flags.push(Span::styled(" [AI]", dim()));
    }
    let fw: usize = flags.iter().map(Span::width).sum();
    let title = trunc(&t.title, cols.title.saturating_sub(fw));
    let pad = cols.title.saturating_sub(str_width(&title) + fw);
    let mut spans = vec![
        if playing { Span::styled("▶ ", accent().bold()) } else { Span::raw("  ") },
        Span::styled(title, if playing { accent().bold() } else { Style::new().bold() }),
    ];
    spans.extend(flags);
    spans.push(Span::raw(" ".repeat(pad)));
    if cols.artist > 0 {
        spans.push(Span::raw(format!("  {}", fit(&t.artist, cols.artist))));
    }
    if cols.album > 0 {
        spans.push(Span::raw("  "));
        let q = if t.quality.is_empty() { String::new() } else { format!("[{}]", t.quality) };
        let qw = str_width(&q);
        let aw = if t.album.is_empty() { 0 } else { cols.album.saturating_sub(qw + usize::from(qw > 0)) };
        let album = trunc(&t.album, aw);
        let used = str_width(&album);
        spans.push(Span::styled(album, dim()));
        if qw > 0 && used + qw < cols.album {
            spans.push(Span::raw(" ".repeat(cols.album - used - qw)));
            spans.push(Span::styled(q, Style::new().fg(pal().ok)));
        } else {
            spans.push(Span::raw(" ".repeat(cols.album.saturating_sub(used))));
        }
    }
    let dur = if t.duration > 0 { fmt_time(t.duration as f64) } else { String::new() };
    spans.push(Span::styled(format!(" {dur:>7}"), dim()));
    ListItem::new(Line::from(spans))
}

fn coll_kind(c: &Collection) -> &'static str {
    match c.browse_id.get(..2) {
        Some("MP") => "Album",
        Some("UC") => "Artist",
        _ => "Playlist",
    }
}

fn coll_item(c: &Collection, width: u16) -> ListItem<'static> {
    let rest = (width as usize).saturating_sub(11);
    let title = trunc(&c.title, if c.subtitle.is_empty() { rest } else { rest.saturating_sub(2) * 3 / 5 });
    let sub = trunc(&c.subtitle, rest.saturating_sub(str_width(&title) + 2));
    ListItem::new(Line::from(vec![
        Span::styled(format!("  {:<9}", coll_kind(c).to_lowercase()), dim()),
        Span::styled(title, Style::new().bold()),
        Span::styled(format!("  {sub}"), dim()),
    ]))
}

fn input_line(app: &App, target: Input, value: &str, placeholder: &str) -> Line<'static> {
    if app.input == Some(target) {
        Line::from(vec![Span::raw(app.input_buf.clone()), Span::styled("▏", accent())])
    } else if value.is_empty() {
        Line::styled(placeholder.to_string(), dim())
    } else {
        Line::raw(value.to_string())
    }
}

fn draw_search(f: &mut Frame, area: Rect, app: &mut App) {
    // The search box is one unboxed line above the results.
    let [inp, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let kind = format!(" {} · t ", app.kind.label());
    let [field, kind_area] = Layout::horizontal([Constraint::Min(1), Constraint::Length(str_width(&kind) as u16)]).areas(inp);
    let placeholder = if field.width >= 70 {
        "/ to search songs, artists, albums — or paste a YouTube link. Empty = Discover"
    } else {
        "/ to search · empty = Discover"
    };
    let mut line = input_line(app, Input::Search, &app.query, placeholder);
    line.spans.insert(0, Span::styled(" ⌕ ", accent().bold()));
    f.render_widget(line, field);
    f.render_widget(Line::styled(kind, dim()).right_aligned(), kind_area);

    let cols = Cols::for_list(list);
    if app.browse.is_some() {
        let mut st = std::mem::take(&mut app.browse_sel);
        if let Some((c, tracks)) = &app.browse {
            let title = format!(" {} · {} · {} tracks  (Esc back, Enter play from here, A queue all, l like, P +playlist) ", c.title, c.subtitle, tracks.len());
            render_list(f, list, block(title), &mut st, tracks.len(), |i| track_item(app, &tracks[i], is_playing(app, &tracks[i]), &cols));
        }
        app.browse_sel = st;
        app.list_area = Some(list);
        return;
    }
    if app.discover_mode() {
        draw_discover(f, list, app);
        return;
    }
    let busy = if app.searching { " · searching…" } else { "" };
    app.list_area = Some(list);
    let mut st = std::mem::take(&mut app.search_sel);
    match &app.results {
        Some(Results::Tracks(t)) => {
            let b = block(format!(" Songs{busy}  (Enter play from here, N play next, a queue, r radio, A queue all, l like, P +playlist) "));
            render_list(f, list, b, &mut st, t.len(), |i| track_item(app, &t[i], is_playing(app, &t[i]), &cols));
        }
        Some(Results::Collections(cs)) => {
            let b = block(format!(" {}{busy}  (Enter open) ", app.kind.label()));
            render_list(f, list, b, &mut st, cs.len(), |i| coll_item(&cs[i], list.width.saturating_sub(2)));
        }
        None => {
            let msg = if app.searching { "Searching…" } else { "" };
            f.render_widget(Paragraph::new(msg).block(block(" Results ")), list);
        }
    }
    app.search_sel = st;
}

fn draw_discover(f: &mut Frame, area: Rect, app: &mut App) {
    let who = if ytm::signed_in() {
        format!("for {}", app.cfg.yt_user)
    } else {
        "· YouTube Music home (sign in for your own picks)".into()
    };
    let busy = if app.discover_loading { " · loading…" } else { "" };
    let title = format!(" Discover {who}{busy}  (Enter play/open, N next, a queue, r radio, l like, P +playlist, S refresh) ");
    if app.discover.is_empty() {
        let msg = if app.discover_loading { "Loading recommendations…" } else { "Nothing to show — press S to retry" };
        f.render_widget(Paragraph::new(msg).dim().block(block(title)), area);
        return;
    }
    let cols = Cols::for_list(area);
    let mut st = std::mem::take(&mut app.discover_sel);
    render_list(f, area, block(title), &mut st, app.discover.len(), |i| match &app.discover[i] {
        DItem::Header(t) => ListItem::new(Line::styled(format!("── {t} "), accent().bold())),
        DItem::Track(t) => track_item(app, t, is_playing(app, t), &cols),
        DItem::Coll(c) => coll_item(c, area.width.saturating_sub(2)),
    });
    app.discover_sel = st;
    app.list_area = Some(area);
}

fn draw_queue(f: &mut Frame, area: Rect, app: &mut App) {
    let title = format!(
        " Queue · {} tracks · radio {}  (Enter play, N play next, d remove, J/K move, c current, X clear, l like, P +playlist) ",
        app.queue.len(),
        if app.cfg.radio { "on" } else { "off" }
    );
    let cols = Cols::for_list(area);
    let mut st = std::mem::take(&mut app.queue_sel);
    render_list(f, area, block(title), &mut st, app.queue.len(), |i| track_item(app, &app.queue[i], app.cur == Some(i), &cols));
    app.queue_sel = st;
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
        let style = if app.lib_mode == mode { Style::new().fg(pal().on_accent).bg(pal().accent).bold() } else { dim() };
        let w = label.chars().count() as u16;
        app.buttons.push((Rect::new(x, inner.y, w, 1), code));
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("  "));
        x += w + 2;
    }
    if yt {
        let who = if ytm::signed_in() { format!("signed in as {}", app.cfg.yt_user) } else { "not signed in".into() };
        spans.push(Span::styled(who, dim()));
    } else if app.input == Some(Input::LibDir) {
        spans.push(Span::raw(format!("folder: {}▏", app.input_buf)));
    } else {
        let mut l = input_line(app, Input::LibFilter, app.lib_filter(), "f to filter");
        l.spans.insert(0, Span::styled("filter: ", dim()));
        spans.extend(l.spans);
    }
    f.render_widget(Line::from(spans), inner);

    if yt {
        draw_library_yt(f, list, app);
        return;
    }
    // The filtered indices are cached in App; rows are only built for the visible window.
    let mut st = std::mem::take(&mut app.lib_sel);
    let vis = app.lib_visible();
    let scanning = if app.lib_scanning { " · scanning…" } else { "" };
    let dir = if app.cfg.library_dir.is_empty() { "No music folder" } else { app.cfg.library_dir.as_str() };
    let title = format!(
        " {dir} · {} of {}{scanning}  (Enter play from here, N play next, a queue, A queue all) ",
        vis.len(),
        app.library.len()
    );
    if vis.is_empty() {
        app.lib_sel = st;
        let lines = library_hint(app);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block(title)), list);
        return;
    }
    let cols = Cols::for_list(list);
    render_list(f, list, block(title), &mut st, vis.len(), |k| {
        let t = &app.library[vis[k]];
        track_item(app, t, is_playing(app, t), &cols)
    });
    app.lib_sel = st;
    app.list_area = Some(list);
}

/// What the local Library shows instead of an empty list: how to get songs into it.
fn library_hint(app: &App) -> Vec<Line<'static>> {
    let dir = &app.cfg.library_dir;
    let key = |k: &str| Span::styled(k.to_string(), accent().bold());
    let mut lines = if app.lib_scanning {
        vec![Line::raw(format!("Scanning {dir}…"))]
    } else if dir.is_empty() {
        vec![
            Line::styled("No music folder set yet.", Style::new().bold()),
            Line::raw(""),
            Line::from(vec![Span::raw("Press "), key("o"), Span::raw(" and type the path of the folder with your music files (also in Settings → Library folder).")]),
            Line::styled("FLAC, ALAC, WAV, DSD, MP3, Opus and more are found in all its subfolders and sorted by their tags.", dim()),
        ]
    } else if let Some(e) = &app.lib_error {
        vec![
            Line::styled(e.clone(), Style::new().bold()),
            Line::raw(""),
            Line::from(vec![Span::raw("Press "), key("o"), Span::raw(" to pick another folder, or "), key("S"), Span::raw(" to scan again.")]),
        ]
    } else if !app.library.is_empty() {
        vec![Line::from(vec![Span::raw(format!("No songs match “{}”. ", app.lib_filter())), key("Esc"), Span::raw(" clears the filter.")])]
    } else {
        vec![
            Line::styled(format!("No audio files found in {dir}."), Style::new().bold()),
            Line::raw(""),
            Line::from(vec![Span::raw("Press "), key("o"), Span::raw(" to pick another folder, or "), key("S"), Span::raw(" to scan again.")]),
        ]
    };
    if app.library.is_empty() && !app.lib_scanning {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![key("y"), Span::styled(" switches to your YouTube Music library; / searches YouTube Music.", dim())]));
    }
    lines
}

fn draw_library_yt(f: &mut Frame, area: Rect, app: &mut App) {
    if !ytm::signed_in() {
        let b = block(" YouTube Music library ");
        let inner = b.inner(area);
        f.render_widget(b, area);
        let mut lines = vec![
            Line::raw("Sign in to see Liked Music and your playlists, and get a personal Discover feed."),
            Line::raw(""),
            Line::styled("How: open music.youtube.com signed in → F12 → Network → click any request", dim()),
            Line::styled("→ copy the whole `cookie` request header → paste it here (or a cookies.txt path).", dim()),
            Line::raw(""),
        ];
        let y = inner.y + lines.len() as u16;
        let btn = button_rows(app, Rect::new(inner.x, y, inner.width, 1), &[("Sign in (L)", KeyCode::Char('L'))], false);
        lines.extend(btn);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        return;
    }
    let busy = if app.yt_loading { " · loading…" } else { "" };
    if app.yt_open.is_some() {
        let cols = Cols::for_list(area);
        let mut st = std::mem::take(&mut app.yt_open_sel);
        if let Some((c, tracks)) = &app.yt_open {
            let title = format!(" {} · {} tracks{busy}  (Esc back, Enter play from here, N next, a queue, A all, r radio, l like, P +playlist) ", c.title, tracks.len());
            render_list(f, area, block(title), &mut st, tracks.len(), |i| track_item(app, &tracks[i], is_playing(app, &tracks[i]), &cols));
        }
        app.yt_open_sel = st;
    } else {
        let title = format!(" Your playlists · {}{busy}  (Enter open) ", app.yt_lists.len());
        let mut st = std::mem::take(&mut app.yt_lists_sel);
        render_list(f, area, block(title), &mut st, app.yt_lists.len(), |i| coll_item(&app.yt_lists[i], area.width.saturating_sub(2)));
        app.yt_lists_sel = st;
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
        f.render_widget(Paragraph::new(msg).dim().alignment(Alignment::Center), inner);
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
                Some(c) if c == i => accent().bold(),
                Some(c) if i < c => dim(),
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
    let clarity = if e.clarity { "on" } else { "off" };
    let profile = eq::CLARITY[e.clarity_preset.min(eq::CLARITY.len() - 1)].0;
    let mut title = format!(" Equalizer · {state} · preset {} · Studio Clarity {clarity} ({profile}) ", e.preset);
    let mut keys = " ←/→ band  ↑/↓ gain  e on/off  [ ] preset  s clarity  { } clarity profile  z reset ";
    if str_width(&title) + 2 > area.width as usize {
        title = format!(" EQ · {state} · {} · Clarity {clarity} ", e.preset);
    }
    if str_width(keys) + 2 > area.width as usize {
        keys = " ←→ band  ↑↓ gain  e on/off  [ ] preset  s { } clarity  z reset ";
    }
    let b = block(title).title_bottom(Line::from(keys).right_aligned());
    let inner = b.inner(area);
    f.render_widget(b, area);

    let combined = e.combined();
    // Columns shrink to fit; on short screens every other dB step is shown.
    const LABEL: usize = 7;
    let col = ((inner.width as usize).saturating_sub(LABEL) / combined.len().max(1)).clamp(3, 7);
    let bar = col.saturating_sub(2).max(1);
    let step = if inner.height >= 19 { 1 } else { 2 };
    let mut lines = Vec::new();
    for level in (-8..=8).rev().step_by(step) {
        let level = level as f64;
        let reach = step as f64 - 0.75;
        let mut spans = vec![Span::styled(format!("{:+3} dB ", level as i32), dim())];
        for (i, g) in combined.iter().enumerate() {
            let filled = (level > 0.0 && *g >= level - reach) || (level < 0.0 && *g <= level + reach);
            let cell = if filled {
                "█".repeat(bar)
            } else if level == 0.0 {
                "─".repeat(bar)
            } else {
                " ".repeat(bar)
            };
            let style = if i == app.eq_band { accent() } else if filled { Style::new() } else { dim() };
            spans.push(Span::styled(format!("{cell:^col$}"), style));
        }
        lines.push(Line::from(spans));
    }
    let labels = eq::BANDS.iter().enumerate().map(|(i, hz)| {
        let s = format!("{:^col$}", eq::band_label(*hz));
        if i == app.eq_band { Span::styled(s, accent().bold()) } else { Span::raw(s) }
    });
    lines.push(Line::from(std::iter::once(Span::raw(" ".repeat(LABEL))).chain(labels).collect::<Vec<_>>()));
    let gains = combined.iter().map(|g| Span::styled(format!("{:^col$}", format!("{g:+.1}")), dim()));
    lines.push(Line::from(std::iter::once(Span::raw(" ".repeat(LABEL))).chain(gains).collect::<Vec<_>>()));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_settings(f: &mut Frame, area: Rect, app: &App) {
    // A pending Last.fm link gets room in the notes so it can be read (and copied) in full.
    let notes_h = if app.lfm_auth.is_some() { 6 } else { 3 };
    let [list, info] = Layout::vertical([Constraint::Length(SETTINGS.len() as u16 + 2), Constraint::Min(notes_h)]).areas(area);
    let name_w = if list.width >= 60 { 26 } else { 20 };
    let row = |i: usize| {
        let name = SETTINGS[i];
        let editing = match (i, app.input) {
            (6, Some(Input::LibDir)) | (7, Some(Input::LfmKey)) | (8, Some(Input::LfmSecret)) => Some(format!("{}▏", app.input_buf)),
            (10, Some(Input::YtCookies)) => Some(if app.input_buf.is_empty() {
                "Enter = sign in with clipboard · or paste first · Esc cancel▏".into()
            } else {
                format!("{} characters pasted — Enter to sign in▏", app.input_buf.chars().count())
            }),
            _ => None,
        };
        let value = editing.unwrap_or_else(|| app.setting_value(i));
        let marker = if i == app.set_sel { Span::styled("›", accent().bold()) } else { Span::raw(" ") };
        ListItem::new(Line::from(vec![
            marker,
            Span::raw(fit(name, name_w)),
            Span::styled(format!(" {value}"), accent()),
        ]))
    };
    // Scrolls so the selected setting is always on screen.
    let mut st = ListState::default().with_selected(Some(app.set_sel));
    let visible = list.height.saturating_sub(2) as usize;
    let mut b = block(" Settings  (↑/↓ select, Enter/←/→ change) ");
    if visible < SETTINGS.len() {
        b = b.title_bottom(Line::styled(format!(" {}/{} ", app.set_sel + 1, SETTINGS.len()), dim()).right_aligned());
    }
    render_list(f, list, b, &mut st, SETTINGS.len(), row);
    let mut text = Vec::new();
    if let Some((_, url)) = &app.lfm_auth {
        text.push(Line::styled("Last.fm: open this link, approve LastWave, then press Enter on \"Last.fm account\" (Esc cancels):", accent().bold()));
        text.push(Line::raw(url.clone()));
        text.push(Line::raw(""));
    }
    text.extend([
        Line::raw("Bit-perfect sends the decoded stream to the device in WASAPI exclusive mode (CoreAudio hog / ALSA on other OSes):"),
        Line::raw("no mixer, no resampling, no EQ, no ReplayGain, volume fixed at 100%. Best with local FLAC/ALAC."),
        Line::raw("YouTube streams top out at ~160 kbps Opus / 128-256 kbps AAC; they are resolved by mpv's yt-dlp hook (keep yt-dlp updated)."),
        Line::raw(""),
        Line::raw("YouTube Music sign-in: open music.youtube.com signed in → F12 → Network → any request → copy the `cookie` request header and paste it on \"YouTube Music account\" (a cookies.txt path works too). Cookies are stored in plain text next to the config file; signing out of that browser session invalidates them. \"Use account for playback\" passes them to yt-dlp for Premium formats and age-restricted songs."),
        Line::raw(""),
        Line::styled(format!("Config file: {}", app.config_path()), dim()),
    ]);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block(" Notes ")), info);
}

fn fmt_params(v: &Value) -> String {
    let Some(rate) = v["samplerate"].as_u64() else { return String::new() };
    let khz = format!("{:.1}", rate as f64 / 1000.0);
    let khz = khz.trim_end_matches(".0");
    format!("{} {khz}kHz {}ch", v["format"].as_str().unwrap_or("?"), v["channel-count"].as_u64().unwrap_or(0))
}

/// Gauge label: no total for live streams / unknown durations.
fn time_label(pos: f64, dur: f64) -> String {
    if dur > 0.0 && dur.is_finite() {
        format!("{} / {} ", fmt_time(pos), fmt_time(dur))
    } else {
        format!("{} ", fmt_time(pos))
    }
}

fn draw_player(f: &mut Frame, area: Rect, app: &mut App) {
    let status = format!(" {} ", app.status);
    let b = block(Line::from(" Now playing ")).title_bottom(Line::styled(status, dim()));
    let inner = b.inner(area);
    f.render_widget(b, area);
    let cw = if inner.width >= 60 { cover_cols(app, inner.height) } else { 0 };
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
            let mut title = vec![Span::styled(format!("{icon} "), accent()), Span::styled(t.title.clone(), Style::new().bold())];
            if app.is_liked(t) {
                title.push(Span::styled(" ♥", Style::new().fg(pal().like)));
            }
            f.render_widget(Line::from(title), l1);
            let mut sub = vec![Span::raw(format!("  {}", t.artist))];
            if !t.album.is_empty() {
                sub.push(Span::styled(format!(" · {}", t.album), dim()));
            }
            f.render_widget(Line::from(sub), l2);
        }
        None => f.render_widget(Line::styled("■ Nothing playing", dim()), l1),
    }

    let ratio = if app.dur > 0.0 { (app.pos / app.dur).clamp(0.0, 1.0) } else { 0.0 };
    let gauge = LineGauge::default()
        .filled_style(accent())
        .unfilled_style(dim())
        .label(time_label(app.pos, app.dur))
        .ratio(ratio);
    f.render_widget(gauge, l3);

    // Most important first: narrow terminals cut from the right.
    let mut spans = Vec::new();
    if !app.codec.is_empty() && !app.idle {
        spans.push(Span::styled(format!(" {} ", app.codec.to_uppercase()), Style::new().fg(pal().on_accent).bg(pal().accent)));
    } else {
        spans.push(Span::styled("idle", dim()));
    }
    if app.cfg.exclusive {
        if app.bit_perfect() {
            spans.push(Span::styled(" BIT-PERFECT", Style::new().fg(pal().ok).bold()));
        } else {
            spans.push(Span::styled(" exclusive", accent()));
        }
    } else if !app.af.is_empty() {
        spans.push(Span::styled(" EQ", accent()));
    }
    if !app.codec.is_empty() && !app.idle {
        spans.push(Span::raw(format!(" {}", fmt_params(&app.params))));
        if app.bitrate > 0.0 {
            spans.push(Span::raw(format!(" {:.0}kbps", app.bitrate / 1000.0)));
        }
        let out = fmt_params(&app.out_params);
        if !out.is_empty() {
            spans.push(Span::styled(format!("  → {} {}", app.ao, out), dim()));
        }
    }
    f.render_widget(Line::from(spans), l4);

    let play = if app.paused || app.idle { "▶ Play" } else { "‖ Pause" };
    let controls = [
        ("◀◀", KeyCode::Char('p')),
        (play, KeyCode::Char(' ')),
        ("▶▶", KeyCode::Char('n')),
        ("−", KeyCode::Char('-')),
        ("+", KeyCode::Char('+')),
        ("≈ Radio", KeyCode::Char('w')),
    ];
    let mut row = button_rows(app, l5, &controls, false).into_iter().next().unwrap_or_default();
    row.spans.push(Span::styled(format!(" vol {:.0}%{}", app.volume, if app.cfg.radio { " · radio" } else { "" }), dim()));
    f.render_widget(row, l5);
}

fn draw_picker(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(p) = &mut app.picker else { return };
    let new_row = if app.input == Some(Input::NewPlaylist) {
        Line::from(vec![Span::styled("  + ", accent()), Span::raw(app.input_buf.clone()), Span::styled("▏", accent())])
    } else {
        Line::styled("  + New playlist…", accent())
    };
    let mut items = vec![ListItem::new(new_row)];
    items.extend(p.items.iter().map(|t| {
        if t.has {
            ListItem::new(Line::styled(format!("✓ {}", t.title), dim()))
        } else {
            ListItem::new(Line::raw(format!("  {}", t.title)))
        }
    }));
    if p.loading {
        items.push(ListItem::new(Line::styled("  loading playlists…", dim())));
    }
    let name = trunc(&p.track.title, 28);
    let w = 56.min(area.width);
    let h = (items.len() as u16 + 2).min(area.height).min(22);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let w = List::new(items).block(block(format!(" Add “{name}” to… (Enter add, Esc close) "))).highlight_style(hl());
    f.render_stateful_widget(w, r, &mut p.sel);
}

/// Every key, grouped (KEYS.md has the same list). Sized so that all of it
/// fits in two columns on an 80x24 terminal.
pub(crate) const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Player (always the playing song)",
        &[
            ("Space", "play / pause"),
            ("n p", "next / previous"),
            ("← → , .", "seek 5s / 10s"),
            ("+ -", "volume"),
            ("w", "radio from playing song"),
            ("B", "flag playing artist as AI"),
            ("R", "radio autoplay on/off"),
        ],
    ),
    (
        "App",
        &[
            ("1-6 Tab", "switch view (⇧Tab back)"),
            ("/", "search YouTube Music"),
            ("c", "playing song in Queue"),
            ("v", "details drawer"),
            ("L", "sign in to YouTube Music"),
            ("u", "undo queue change"),
            ("? q Ctrl+C", "this help / quit"),
        ],
    ),
    (
        "Song (highlighted, else playing)",
        &[
            ("Enter", "play from here / open album"),
            ("N a", "play next / add to queue"),
            ("A", "add the whole list to queue"),
            ("r", "radio from the song"),
            ("l P", "like / add to playlist"),
        ],
    ),
    (
        "Lists",
        &[
            ("↑↓ j k", "move"),
            ("PgUp PgDn g G", "page / top / bottom"),
            ("Esc ⌫", "back / clear"),
            ("S", "refresh / rescan"),
            ("mouse", "click / double-click"),
        ],
    ),
    (
        "Views",
        &[
            ("t", "Search: result type"),
            ("d Del", "Queue: remove"),
            ("J K X", "Queue: move / clear"),
            ("y", "Library: local ↔ YouTube"),
            ("f o", "Library: filter / folder"),
            ("← → ↑↓", "EQ: band / gain"),
            ("e z", "EQ: on/off / reset"),
            ("[ ]", "EQ: preset"),
            ("s { }", "EQ: Clarity / profile"),
            ("← → Enter", "Settings: change"),
            ("Enter ×2", "Settings: sign out"),
        ],
    ),
    (
        "Typing",
        &[
            ("Enter Esc", "confirm / cancel"),
            ("Ctrl+U Ctrl+V", "clear / paste cookies"),
            ("click", "keeps the text"),
        ],
    ),
];

fn help_lines(title: &'static str, rows: &[(&str, &str)], width: usize) -> Vec<Line<'static>> {
    let kw = rows.iter().map(|(k, _)| str_width(k)).max().unwrap_or(0) + 2;
    let mut lines = vec![Line::styled(title, accent().bold())];
    lines.extend(rows.iter().map(|(k, d)| {
        Line::from(vec![
            Span::styled(format!("  {}", fit(k, kw)), Style::new().bold()),
            Span::raw(trunc(d, width.saturating_sub(kw + 2))),
        ])
    }));
    lines
}

/// Key help: one column when it fits, else two side by side (sections flow
/// into the second column; what still doesn't fit is cut off at the bottom).
fn draw_help(f: &mut Frame, area: Rect) {
    let total: usize = HELP.iter().map(|(_, rows)| rows.len() + 1).sum();
    // One column has a blank line between sections.
    let one_col_h = (total + HELP.len() - 1 + 2) as u16;
    let two_cols = area.height < one_col_h && area.width >= 60;
    let (w, h) = if two_cols { (area.width.min(130), area.height) } else { (area.width.min(64), one_col_h.min(area.height)) };
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let b = block(" Keys (any key closes) ");
    let inner = b.inner(r);
    f.render_widget(b, r);
    if two_cols {
        let [left, _, right] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Fill(1)]).areas(inner);
        let (mut l, mut rt) = (Vec::new(), Vec::new());
        for (title, rows) in HELP {
            // Fill the left column first; a section never splits across columns.
            let col = if rt.is_empty() && l.len() + rows.len() < inner.height as usize { &mut l } else { &mut rt };
            col.extend(help_lines(title, rows, left.width as usize));
        }
        f.render_widget(Paragraph::new(l), left);
        f.render_widget(Paragraph::new(rt), right);
    } else {
        let mut lines = Vec::new();
        for (i, (title, rows)) in HELP.iter().enumerate() {
            if i > 0 {
                lines.push(Line::raw(""));
            }
            lines.extend(help_lines(title, rows, inner.width as usize));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(trunc("hello", 10), "hello");
        assert_eq!(trunc("hello world", 6), "hello…");
        // CJK is two columns wide: never split a glyph past the limit.
        assert_eq!(trunc("日本語の歌", 6), "日本…");
        assert_eq!(str_width(&fit("日本語の歌", 5)), 5);
        assert_eq!(str_width(&fit("🎵🎵🎵🎵", 5)), 5);
        assert_eq!(trunc("abc", 0), "");
    }

    #[test]
    fn columns_fill_the_row() {
        for w in [20u16, 40, 78, 120, 200] {
            let c = Cols::new(w);
            let used = 2 + c.title + if c.artist > 0 { 2 + c.artist } else { 0 } + if c.album > 0 { 2 + c.album } else { 0 } + 8;
            assert!(used <= w as usize, "width {w}: {used}");
        }
    }

    #[test]
    fn live_streams_show_elapsed_only() {
        assert_eq!(time_label(42.0, 0.0), "0:42 ");
        assert_eq!(time_label(42.0, 180.0), "0:42 / 3:00 ");
    }

    #[test]
    fn settings_keep_selection_visible_at_80x24() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(tx);
        app.view = View::Settings;
        app.set_sel = SETTINGS.len() - 1;
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(screen.contains(SETTINGS[SETTINGS.len() - 1]), "last setting not on screen:\n{screen}");
        // The tab bar fits: the last tab is not cut off.
        assert!(screen.lines().next().unwrap().contains("6 "), "{screen}");
    }

    #[test]
    fn help_fits_80x24() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw_help(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..24).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect();
        // Both sections, down to their last rows, are on screen.
        assert!(screen.contains("quit") && screen.contains("Library: "), "{screen}");
        // Every section and every row, keys and descriptions uncut.
        for (title, rows) in HELP {
            assert!(screen.contains(title), "{title} missing:\n{screen}");
            for (k, d) in *rows {
                assert!(screen.contains(k) && screen.contains(d), "{k} / {d} missing or cut:\n{screen}");
            }
        }
    }
}
