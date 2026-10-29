//! Cover art: fetched/decoded off the UI thread, downscaled, kept in a small
//! LRU, and drawn either as real graphics through ratatui-image (sixel /
//! kitty / iTerm2) or as text: half-block, quadrant or sextant characters
//! with two fitted colours per cell. Text is what survives multiplexers
//! (herdr, Zellij), which only forward a character grid. Half-blocks are the
//! default text mode: they are in practically every font, unlike sextants.
//! Without truecolor, text covers use the xterm 256-colour palette.
//!
//! `kitty+text` combines both for herdr: the text cover is drawn as usual and
//! a Kitty direct placement is laid on top of it. herdr forwards the image
//! only to clients whose terminal speaks Kitty graphics (WezTerm), so those
//! see the real cover while the others (Rio) keep the text one underneath.

use crate::{app::Msg, ytm::agent};
use image::{imageops::FilterType, DynamicImage};
use lofty::prelude::*;
use ratatui::{layout::Rect, style::Color, Frame};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
    Resize, StatefulImage,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Cursor, Read, Write},
    sync::mpsc::Sender,
    thread,
    time::{Duration, Instant},
};

const MAX_CACHED: usize = 24;
const MAX_PX: u32 = 360;
/// A cover that failed to load is fetched again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Render slots: each keeps its own encoding so two sizes of the same cover
/// don't re-encode every frame.
pub const DETAIL: usize = 0;
pub const NOW: usize = 1;

/// `image_protocol` setting values.
pub const PROTOCOLS: &[&str] = &["auto", "kitty+text", "kitty", "iterm2", "sixel", "sextants", "quadrants", "halfblocks"];

/// Kitty image ids for the overlay of each slot.
const OVERLAY_IDS: [u32; 2] = [7701, 7702];
/// Overlay images are sent as PNG at most this many pixels a side.
const OVERLAY_PX: u32 = 320;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Graphics(ProtocolType),
    /// 1x2 pixels per cell ("▀" with fg = top, bg = bottom), in every font.
    HalfBlocks,
    /// 2x2 pixels per cell (U+2596..U+259F), in every common font.
    Quadrants,
    /// 2x3 pixels per cell (U+1FB00..U+1FB3B, "legacy computing").
    Sextants,
    /// Half-blocks with a Kitty image placed over them.
    KittyText,
}

/// One text cell: glyph, foreground, background.
type Cell = (char, Color, Color);

enum Encoded {
    Graphics(StatefulProtocol),
    Text { area: Rect, cells: Vec<Cell> },
}

pub struct Covers {
    picker: Picker,
    /// What `auto` resolves to for this terminal.
    auto: Mode,
    mode: Mode,
    cell: (u16, u16),
    /// Text covers use 24-bit colour; otherwise the 256-colour palette.
    truecolor: bool,
    images: HashMap<String, DynamicImage>,
    order: VecDeque<String>,
    pending: HashSet<String>,
    /// Keys whose last load failed, and when.
    failed: HashMap<String, Instant>,
    slots: [Option<(String, Encoded)>; 2],
    /// Overlays wanted by the frame being drawn / currently on screen.
    frame: [Option<(String, Rect)>; 2],
    placed: [Option<(String, Rect)>; 2],
}

impl Covers {
    pub fn new(picker: Picker, pref: &str) -> Self {
        let auto = auto_mode(picker.protocol_type());
        let mut c = Self {
            auto,
            mode: auto,
            cell: cell_size(&picker),
            truecolor: truecolor(),
            picker,
            images: HashMap::new(),
            order: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            slots: [None, None],
            frame: [None, None],
            placed: [None, None],
        };
        c.set_protocol(pref);
        c
    }

    /// Apply an `image_protocol` setting; covers re-encode on next draw.
    pub fn set_protocol(&mut self, pref: &str) {
        self.mode = match pref {
            "kitty" => Mode::Graphics(ProtocolType::Kitty),
            "iterm2" => Mode::Graphics(ProtocolType::Iterm2),
            "sixel" => Mode::Graphics(ProtocolType::Sixel),
            "halfblocks" => Mode::HalfBlocks,
            "quadrants" => Mode::Quadrants,
            "sextants" => Mode::Sextants,
            "kitty+text" => Mode::KittyText,
            _ => self.auto,
        };
        if let Mode::Graphics(p) = self.mode {
            self.picker.set_protocol_type(p);
        }
        self.slots = [None, None];
    }

    pub fn mode_name(&self) -> String {
        match self.mode {
            Mode::Graphics(p) => format!("{p:?}").to_lowercase(),
            Mode::HalfBlocks => "halfblocks".into(),
            Mode::Quadrants => "quadrants".into(),
            Mode::Sextants => "sextants".into(),
            Mode::KittyText => "kitty+text".into(),
        }
    }

    /// Call before drawing a frame; slots not rendered again drop their overlay.
    pub fn begin_frame(&mut self) {
        self.frame = [None, None];
    }

    /// Bring the Kitty overlays in line with the frame just drawn. Writes
    /// straight to the terminal after ratatui has flushed the frame.
    pub fn flush_overlays(&mut self, out: &mut impl Write) {
        for (slot, id) in OVERLAY_IDS.into_iter().enumerate() {
            if self.frame[slot] == self.placed[slot] {
                continue;
            }
            if self.placed[slot].is_some() {
                let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
            }
            if let Some((key, r)) = &self.frame[slot] {
                if let Some(img) = self.images.get(key) {
                    place_kitty(out, id, img, *r);
                }
            }
            self.placed[slot] = self.frame[slot].clone();
        }
        let _ = out.flush();
    }

    /// Draw the cover for `key` into `area`, starting a fetch if needed.
    /// Returns false while loading or when there is no art (caller draws a
    /// placeholder).
    pub fn render(&mut self, f: &mut Frame, area: Rect, slot: usize, key: &str, tx: &Sender<Msg>) -> bool {
        if key.is_empty() || area.width == 0 || area.height == 0 {
            return false;
        }
        // Covers on screen stay at the young end of the LRU, so scrolling
        // through a long list doesn't evict the one that is playing.
        if let Some(i) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(i).unwrap_or_default();
            self.order.push_back(k);
        }
        let fresh = match (&self.slots[slot], self.mode) {
            (Some((k, Encoded::Graphics(_))), Mode::Graphics(_)) => k != key,
            (Some((k, Encoded::Text { area: a, .. })), m) if !matches!(m, Mode::Graphics(_)) => k != key || *a != area,
            _ => true,
        };
        if fresh {
            let Some(img) = self.images.get(key) else {
                self.fetch(key, tx);
                return false;
            };
            let enc = match self.mode {
                Mode::Graphics(_) => Encoded::Graphics(self.picker.new_resize_protocol(img.clone())),
                text => Encoded::Text { area, cells: text_cells(img, area, text, self.cell, self.truecolor) },
            };
            self.slots[slot] = Some((key.to_string(), enc));
        }
        let (r, cols) = text_rect(area, self.cell);
        match &mut self.slots[slot] {
            // Default is nearest-neighbour, which turns half-block covers
            // into noise; a real filter averages each cell's pixels.
            // Drawn into the centred square: Fit alone pins it top-left.
            Some((_, Encoded::Graphics(p))) => {
                f.render_stateful_widget(StatefulImage::default().resize(Resize::Fit(Some(FilterType::CatmullRom))), r, p)
            }
            Some((_, Encoded::Text { cells, .. })) => {
                let buf = f.buffer_mut();
                for (i, (ch, fg, bg)) in cells.iter().enumerate() {
                    let (x, y) = (r.x + (i as u16 % cols), r.y + (i as u16 / cols));
                    buf[(x, y)].set_char(*ch).set_fg(*fg).set_bg(*bg);
                }
                if self.mode == Mode::KittyText {
                    self.frame[slot] = Some((key.to_string(), r));
                }
            }
            None => return false,
        }
        true
    }

    /// Start loading `key` unless it is already loading or failed recently.
    fn fetch(&mut self, key: &str, tx: &Sender<Msg>) {
        if self.failed.get(key).is_some_and(|t| t.elapsed() < RETRY_AFTER) || !self.pending.insert(key.to_string()) {
            return;
        }
        let (key, tx) = (key.to_string(), tx.clone());
        thread::spawn(move || {
            let img = load(&key);
            let _ = tx.send(Msg::Art(key, img));
        });
    }

    pub fn loaded(&mut self, key: String, img: Option<DynamicImage>) {
        self.pending.remove(&key);
        let Some(img) = img else {
            // Remembered for a while only: a network blip shouldn't leave
            // the placeholder up for good.
            self.failed.insert(key, Instant::now());
            return;
        };
        self.failed.remove(&key);
        self.order.retain(|k| *k != key);
        self.order.push_back(key.clone());
        self.images.insert(key, img);
        while self.order.len() > MAX_CACHED {
            if let Some(old) = self.order.pop_front() {
                self.images.remove(&old);
            }
        }
    }

    /// Columns a visually square cover `rows` cells tall needs.
    pub fn cols_for_rows(&self, rows: u16) -> u16 {
        let (fw, fh) = (self.cell.0.max(1) as u32, self.cell.1.max(1) as u32);
        ((rows as u32 * fh + fw / 2) / fw).min(u16::MAX as u32) as u16
    }

    /// Rows a visually square cover `cols` cells wide needs.
    pub fn rows_for_cols(&self, cols: u16) -> u16 {
        let (fw, fh) = (self.cell.0.max(1) as u32, self.cell.1.max(1) as u32);
        (cols as u32 * fw / fh).min(u16::MAX as u32) as u16
    }
}

/// Whether the terminal takes 24-bit colour escapes. COLORTERM is the
/// standard signal; Windows Terminal (WT_SESSION) and a few others don't
/// always set it. Multiplexers keep the previous behaviour (truecolor).
pub fn truecolor() -> bool {
    let env = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let term = env("TERM");
    matches!(env("COLORTERM").as_str(), "truecolor" | "24bit")
        || std::env::var_os("WT_SESSION").is_some()
        || matches!(env("TERM_PROGRAM").as_str(), "wezterm" | "iterm.app" | "vscode" | "rio" | "ghostty")
        || term.contains("kitty")
        || term.ends_with("-direct")
        || in_multiplexer()
}

/// Nearest xterm-256 colour: the 6x6x6 cube or the 24-step grey ramp.
fn to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let idx = |v: u8| if v < 48 { 0 } else if v < 115 { 1 } else { ((v - 35) / 40) as usize };
    let (ri, gi, bi) = (idx(r), idx(g), idx(b));
    let dist = |c: (u8, u8, u8)| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let step = if avg > 238 { 23 } else { avg.saturating_sub(3) / 10 } as u8;
    let grey = 8 + 10 * step;
    if dist((grey, grey, grey)) < dist(cube) {
        232 + step
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

/// Kitty direct placement of `img` stretched over the cells of `r`, above
/// the text (z=1), without moving the cursor (C=1). PNG keeps it small.
fn place_kitty(out: &mut impl Write, id: u32, img: &DynamicImage, r: Rect) {
    use base64::Engine;
    let small = if img.width() > OVERLAY_PX { img.resize(OVERLAY_PX, OVERLAY_PX, FilterType::Triangle) } else { img.clone() };
    let mut png = Vec::new();
    if small.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).is_err() {
        return;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
    // Save cursor, jump to the cover's top-left cell, transmit, restore.
    let _ = write!(out, "\x1b7\x1b[{};{}H", r.y + 1, r.x + 1);
    for (i, c) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let c = std::str::from_utf8(c).unwrap_or_default();
        if i == 0 {
            let _ = write!(out, "\x1b_Ga=T,f=100,c={},r={},C=1,z=1,i={id},q=2,m={more};{c}\x1b\\", r.width, r.height);
        } else {
            let _ = write!(out, "\x1b_Gm={more};{c}\x1b\\");
        }
    }
    let _ = write!(out, "\x1b8");
}

/// Largest visually square rect inside `area` (cells are font_w x font_h
/// pixels), centred. Returns it with its column count.
fn text_rect(area: Rect, (fw, fh): (u16, u16)) -> (Rect, u16) {
    let (fw, fh) = (fw.max(1) as u32, fh.max(1) as u32);
    let side = (area.width as u32 * fw).min(area.height as u32 * fh);
    let cols = ((side / fw) as u16).clamp(1, area.width);
    let rows = ((side / fh) as u16).clamp(1, area.height);
    let r = Rect::new(area.x + (area.width - cols) / 2, area.y + (area.height - rows) / 2, cols, rows);
    (r, cols)
}

/// Encode an image as block characters: each cell's sub-pixels are split
/// into the two-colour partition with the least error.
fn text_cells(img: &DynamicImage, area: Rect, mode: Mode, font: (u16, u16), truecolor: bool) -> Vec<Cell> {
    let mode = if mode == Mode::KittyText { Mode::HalfBlocks } else { mode };
    let (sx, sy) = match mode {
        Mode::Sextants => (2u32, 3u32),
        Mode::HalfBlocks => (1, 2),
        _ => (2, 2),
    };
    let (r, _) = text_rect(area, font);
    let (cols, rows) = (r.width as u32, r.height as u32);
    let px = img.resize_exact(cols * sx, rows * sy, FilterType::CatmullRom).to_rgb8();
    let n = (sx * sy) as usize;
    let mut out = Vec::with_capacity((cols * rows) as usize);
    for cy in 0..rows {
        for cx in 0..cols {
            let mut p = [[0f32; 3]; 6];
            for (i, q) in p.iter_mut().take(n).enumerate() {
                let c = px.get_pixel(cx * sx + i as u32 % sx, cy * sy + i as u32 / sx).0;
                *q = [c[0] as f32, c[1] as f32, c[2] as f32];
            }
            out.push(fit_cell(&p[..n], mode));
        }
    }
    if !truecolor {
        let q = |c: Color| match c {
            Color::Rgb(r, g, b) => Color::Indexed(to_256(r, g, b)),
            c => c,
        };
        out.iter_mut().for_each(|(_, fg, bg)| (*fg, *bg) = (q(*fg), q(*bg)));
    }
    out
}

fn fit_cell(p: &[[f32; 3]], mode: Mode) -> Cell {
    let rgb = |c: [f32; 3]| Color::Rgb(c[0] as u8, c[1] as u8, c[2] as u8);
    if mode == Mode::HalfBlocks {
        // Two pixels, two colours: exact, no fitting needed.
        return ('▀', rgb(p[0]), rgb(p[1]));
    }
    let n = p.len();
    let mean = |mask: u32, set: bool| {
        let (mut s, mut k) = ([0f32; 3], 0f32);
        for (i, c) in p.iter().enumerate() {
            if (mask >> i & 1 == 1) == set {
                (0..3).for_each(|j| s[j] += c[j]);
                k += 1.0;
            }
        }
        if k == 0.0 { s } else { [s[0] / k, s[1] / k, s[2] / k] }
    };
    let err = |mask: u32| {
        let (a, b) = (mean(mask, true), mean(mask, false));
        p.iter()
            .enumerate()
            .map(|(i, c)| {
                let m = if mask >> i & 1 == 1 { a } else { b };
                (0..3).map(|j| (c[j] - m[j]).powi(2)).sum::<f32>()
            })
            .sum::<f32>()
    };
    // The last sub-pixel always stays in the background group: halves the search.
    let best = (0..1u32 << (n - 1)).min_by(|&a, &b| err(a).total_cmp(&err(b))).unwrap_or(0);
    let (fg, bg) = (rgb(mean(best, true)), rgb(mean(best, false)));
    if best == 0 {
        return (' ', bg, bg);
    }
    let ch = match mode {
        Mode::Sextants => match best {
            21 => '▌',
            42 => '▐',
            m => char::from_u32(0x1FB00 + m - 1 - (m > 21) as u32 - (m > 42) as u32).unwrap_or('█'),
        },
        _ => [' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█'][best as usize],
    };
    (ch, fg, bg)
}

fn load(key: &str) -> Option<DynamicImage> {
    let bytes = match key.strip_prefix("local:") {
        Some(path) => {
            let f = lofty::read_from_path(path).ok()?;
            let tag = f.primary_tag().or_else(|| f.first_tag())?;
            tag.pictures().first()?.data().to_vec()
        }
        None => {
            let mut buf = Vec::new();
            agent().get(key).call().ok()?.into_reader().take(8 << 20).read_to_end(&mut buf).ok()?;
            buf
        }
    };
    let mut img = image::load_from_memory(&bytes).ok()?;
    // 16:9 video thumbnails: keep the centre square.
    let (w, h) = (img.width(), img.height());
    if w as f32 > h as f32 * 1.15 {
        img = img.crop_imm((w - h) / 2, 0, h, h);
    }
    if img.width() > MAX_PX || img.height() > MAX_PX {
        img = img.resize(MAX_PX, MAX_PX, FilterType::Triangle);
    }
    Some(DynamicImage::ImageRgb8(img.to_rgb8()))
}

/// The picker the app uses for covers.
pub fn detect_picker() -> Picker {
    Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)))
}

fn in_multiplexer() -> bool {
    let env = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    matches!(env("TERM_PROGRAM").as_str(), "herdr" | "zellij") || !env("ZELLIJ").is_empty()
}

/// Cell size in pixels, for keeping covers square. Multiplexers report a
/// made-up 8x16 (herdr) or nothing; real fonts are ~2.4x taller than wide
/// (WezTerm 9x23, Rio 10x22 here), and 2:1 visibly stretched covers.
fn cell_size(picker: &Picker) -> (u16, u16) {
    if in_multiplexer() {
        (10, 24)
    } else {
        picker.font_size()
    }
}

/// The query answer can't always be trusted:
/// - Multiplexers like herdr and Zellij redraw a text grid and drop image
///   escapes, even when the outer terminal would answer that it supports them.
///   (tmux is left alone: ratatui-image wraps images in tmux passthrough.)
/// - On Windows, ConPTY answers the capability query itself, so Rio looks
///   like a plain console although it renders iTerm2 images.
fn auto_mode(queried: ProtocolType) -> Mode {
    let env = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let program = env("TERM_PROGRAM");
    if in_multiplexer() {
        return Mode::KittyText;
    }
    if program == "rio" && queried == ProtocolType::Halfblocks {
        return Mode::Graphics(ProtocolType::Iterm2);
    }
    match queried {
        // No graphics at all: our own half-blocks (256-colour fallback,
        // centred) beat ratatui-image's; sextants are opt-in since many
        // fonts lack them and show empty boxes.
        ProtocolType::Halfblocks => Mode::HalfBlocks,
        p => Mode::Graphics(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_two_colours() {
        let (r, b) = ([255.0, 0.0, 0.0], [0.0, 0.0, 255.0]);
        // Left column red, right column blue.
        let (ch, fg, bg) = fit_cell(&[r, b, r, b], Mode::Quadrants);
        assert_eq!((ch, fg, bg), ('▌', Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255)));
        assert_eq!(fit_cell(&[b, b, b, b, b, b], Mode::Sextants).0, ' ');
        // Top-left sub-pixel only: first sextant glyph.
        assert_eq!(fit_cell(&[r, b, b, b, b, b], Mode::Sextants).0, '\u{1FB00}');
        assert_eq!(fit_cell(&[r, b, r, b, r, b], Mode::Sextants).0, '▌');
        assert_eq!(fit_cell(&[r, b], Mode::HalfBlocks), ('▀', Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn quantises_to_256() {
        assert_eq!(to_256(0, 0, 0), 16);
        assert_eq!(to_256(255, 255, 255), 231);
        assert_eq!(to_256(255, 0, 0), 196);
        assert_eq!(to_256(128, 128, 128), 244);
        assert_eq!(to_256(255, 215, 175), 223);
    }

    #[test]
    fn square_cover_in_cells() {
        let (r, cols) = text_rect(Rect::new(0, 0, 40, 10), (8, 16));
        assert_eq!((r.width, r.height, cols), (20, 10, 20));
        assert_eq!(r.x, 10, "centred horizontally");
    }
}
