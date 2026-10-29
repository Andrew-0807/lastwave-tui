//! Keys and mouse: `global_act` / `view_act` turn keys into actions, `run`
//! carries them out; text fields, paste and clicks.

use super::{nav, App, Highlight, Input, LibMode, View, SETTINGS};
use crate::{
    eq,
    model::Track,
    ytm::{self, Results},
};
use ratatui::{
    crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::Position,
    widgets::ListState,
};
use serde_json::json;
use std::time::Duration;

/// What a key does. Keys become actions in one place: `global_act` (same
/// meaning in every view), else `view_act` (keys only one view uses). A view
/// never rebinds a global key; see KEYS.md and the `keymap_*` tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    // App
    Quit,
    Help,
    ShowView(usize),
    /// true: backwards
    CycleView(bool),
    Search,
    Details,
    ShowPlaying,
    SignInYt,
    Undo,
    // Player: always the playing song
    PlayPause,
    Next,
    Prev,
    Seek(i32),
    Volume(i32),
    RadioPlaying,
    FlagAi,
    Autoplay,
    // The highlighted row (else the playing song)
    Activate,
    PlayNext,
    AddToQueue,
    AddAll,
    Radio,
    Like,
    AddToPlaylist,
    // Lists
    Back,
    Refresh,
    Move(isize),
    // Search
    SearchType,
    // Queue
    Remove,
    /// true: up
    Shift(bool),
    ClearQueue,
    // Library
    LibSource(Option<LibMode>),
    Filter,
    Folder,
    // Equalizer
    EqBand(i8),
    EqGain(i8),
    EqToggle,
    /// true: next
    EqPreset(bool),
    EqReset,
    Clarity,
    /// true: next
    ClarityProfile(bool),
    // Settings: true = backwards
    Change(bool),
}


/// Far enough to reach either end of any list.
const FAR: isize = 1 << 30;


/// Keys with one meaning in every view (outside text fields and pop-ups).
pub fn global_act(code: KeyCode) -> Option<Act> {
    Some(match code {
        KeyCode::Char('q') => Act::Quit,
        KeyCode::Char('?') => Act::Help,
        KeyCode::Char(c @ '1'..='6') => Act::ShowView((c as u8 - b'1') as usize),
        KeyCode::Tab => Act::CycleView(false),
        KeyCode::BackTab => Act::CycleView(true),
        KeyCode::Char('/') => Act::Search,
        KeyCode::Char('v') => Act::Details,
        KeyCode::Char('c') => Act::ShowPlaying,
        KeyCode::Char('L') => Act::SignInYt,
        KeyCode::Char('u') => Act::Undo,
        KeyCode::Char(' ') => Act::PlayPause,
        KeyCode::Char('n') => Act::Next,
        KeyCode::Char('p') => Act::Prev,
        KeyCode::Char(',') => Act::Seek(-10),
        KeyCode::Char('.') => Act::Seek(10),
        KeyCode::Char('+') | KeyCode::Char('=') => Act::Volume(5),
        KeyCode::Char('-') => Act::Volume(-5),
        KeyCode::Char('w') => Act::RadioPlaying,
        KeyCode::Char('B') => Act::FlagAi,
        KeyCode::Char('R') => Act::Autoplay,
        KeyCode::Enter => Act::Activate,
        KeyCode::Esc | KeyCode::Backspace => Act::Back,
        KeyCode::Char('S') => Act::Refresh,
        KeyCode::Char('N') => Act::PlayNext,
        KeyCode::Char('a') => Act::AddToQueue,
        KeyCode::Char('A') => Act::AddAll,
        KeyCode::Char('r') => Act::Radio,
        KeyCode::Char('l') => Act::Like,
        KeyCode::Char('P') => Act::AddToPlaylist,
        _ => return None,
    })
}


/// Keys only some views use: moving around, and each view's own commands.
pub fn view_act(view: View, code: KeyCode) -> Option<Act> {
    let list = matches!(view, View::Search | View::Queue | View::Library | View::Settings);
    let moved = match code {
        KeyCode::Up | KeyCode::Char('k') => Some(-1),
        KeyCode::Down | KeyCode::Char('j') => Some(1),
        KeyCode::PageUp => Some(-15),
        KeyCode::PageDown => Some(15),
        KeyCode::Home | KeyCode::Char('g') => Some(-FAR),
        KeyCode::End | KeyCode::Char('G') => Some(FAR),
        _ => None,
    };
    if let (true, Some(d)) = (list, moved) {
        return Some(Act::Move(d));
    }
    Some(match (view, code) {
        (View::Search | View::Queue | View::Library | View::Lyrics, KeyCode::Left) => Act::Seek(-5),
        (View::Search | View::Queue | View::Library | View::Lyrics, KeyCode::Right) => Act::Seek(5),
        (View::Search, KeyCode::Char('t')) => Act::SearchType,
        (View::Queue, KeyCode::Char('d') | KeyCode::Delete) => Act::Remove,
        (View::Queue, KeyCode::Char('K')) => Act::Shift(true),
        (View::Queue, KeyCode::Char('J')) => Act::Shift(false),
        (View::Queue, KeyCode::Char('X')) => Act::ClearQueue,
        (View::Library, KeyCode::Char('y')) => Act::LibSource(None),
        // The clickable source tabs.
        (View::Library, KeyCode::F(11)) => Act::LibSource(Some(LibMode::Local)),
        (View::Library, KeyCode::F(12)) => Act::LibSource(Some(LibMode::Yt)),
        (View::Library, KeyCode::Char('f')) => Act::Filter,
        (View::Library, KeyCode::Char('o')) => Act::Folder,
        (View::Eq, KeyCode::Left) => Act::EqBand(-1),
        (View::Eq, KeyCode::Right) => Act::EqBand(1),
        (View::Eq, KeyCode::Up | KeyCode::Char('k')) => Act::EqGain(1),
        (View::Eq, KeyCode::Down | KeyCode::Char('j')) => Act::EqGain(-1),
        (View::Eq, KeyCode::Char('e')) => Act::EqToggle,
        (View::Eq, KeyCode::Char('[')) => Act::EqPreset(false),
        (View::Eq, KeyCode::Char(']')) => Act::EqPreset(true),
        (View::Eq, KeyCode::Char('z')) => Act::EqReset,
        (View::Eq, KeyCode::Char('s')) => Act::Clarity,
        (View::Eq, KeyCode::Char('{')) => Act::ClarityProfile(false),
        (View::Eq, KeyCode::Char('}')) => Act::ClarityProfile(true),
        (View::Settings, KeyCode::Left) => Act::Change(true),
        (View::Settings, KeyCode::Right) => Act::Change(false),
        _ => return None,
    })
}

impl App {
    /// What song actions apply to: the highlighted row of the view's list,
    /// else (no list, or nothing in it) the playing song.
    pub(super) fn target(&self) -> Option<Highlight> {
        self.highlighted().or_else(|| self.current().cloned().map(Highlight::Track))
    }

    /// The song `target` points at; says why not when it's an album/playlist or nothing.
    pub(super) fn target_track(&mut self, verb: &str) -> Option<Track> {
        match self.target() {
            Some(Highlight::Track(t)) => Some(t),
            Some(Highlight::Collection(c)) => {
                self.status = format!("Open {} (Enter) and pick a song to {verb}", c.title);
                None
            }
            None => {
                self.status = "Nothing highlighted or playing".into();
                None
            }
        }
    }

    /// The song an account action applies to and its video id, or why there is none.
    pub(super) fn account_target(&mut self, what: &str, verb: &str) -> Option<(Track, String)> {
        if !ytm::signed_in() {
            self.status = format!("Sign in to YouTube Music (L) to {what}");
            return None;
        }
        let t = self.target_track(verb)?;
        match t.video_id().map(str::to_string) {
            Some(id) => Some((t, id)),
            None => {
                self.status = "Local files aren't on YouTube Music".into();
                None
            }
        }
    }

    pub fn on_paste(&mut self, text: String) {
        let Some(target) = self.input else {
            if self.view == View::Search || self.view == View::Library {
                self.view = View::Search;
                self.begin_input(Input::Search, text.trim().to_string());
            }
            return;
        };
        if target == Input::YtCookies {
            self.input_buf.push_str(&text);
        } else {
            self.input_buf.push_str(&text.replace(['\r', '\n'], " "));
            if target == Input::LibFilter {
                self.set_lib_filter(self.input_buf.clone());
            }
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        let armed = self.confirm_signout;
        self.handle_key(k);
        // Any key but a second Enter on that row cancels a pending sign-out.
        if armed.is_some() && self.confirm_signout == armed {
            self.confirm_signout = None;
            if self.status.starts_with("Press Enter again") {
                self.status = "Sign-out cancelled".into();
            }
        }
        self.keep_sticky();
    }

    pub(super) fn handle_key(&mut self, k: KeyEvent) {
        if let Some(target) = self.input {
            self.on_input(target, k);
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.picker.is_some() {
            self.key_picker(k.code);
            return;
        }
        // main.rs quits on Ctrl+C first; never let it act as plain c.
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if let Some(a) = global_act(k.code).or_else(|| view_act(self.view, k.code)) {
            self.run(a);
        }
    }

    pub(super) fn run(&mut self, a: Act) {
        match a {
            Act::Quit => self.quit = true,
            Act::Help => self.help = true,
            Act::ShowView(i) => self.view = View::ALL[i],
            Act::CycleView(back) => self.cycle_view(if back { View::ALL.len() - 1 } else { 1 }),
            Act::Search => {
                self.view = View::Search;
                self.begin_input(Input::Search, self.query.clone());
            }
            Act::Details => {
                self.cfg.show_details = !self.cfg.show_details;
                self.cfg.save();
            }
            Act::ShowPlaying => match self.cur {
                Some(c) => {
                    self.view = View::Queue;
                    self.queue_sel.select(Some(c));
                }
                None => self.status = "Nothing is playing".into(),
            },
            Act::SignInYt if ytm::signed_in() => {
                self.status = format!("Signed in to YouTube Music as {} (sign out in Settings)", self.cfg.yt_user)
            }
            Act::SignInYt => self.start_yt_login(),
            Act::Undo => self.undo_queue(),
            Act::PlayPause => self.toggle_pause(),
            Act::Next => self.next(),
            Act::Prev => self.prev(),
            Act::Seek(s) => self.mpv_cmd(json!(["seek", s, "relative"])),
            Act::Volume(d) => self.mpv_cmd(json!(["add", "volume", d])),
            Act::RadioPlaying => self.radio_from_current(),
            Act::FlagAi => self.toggle_ai_flag(),
            Act::Autoplay => {
                self.cfg.radio = !self.cfg.radio;
                self.status = format!("Radio autoplay {}", if self.cfg.radio { "on" } else { "off" });
                self.cfg.save();
                self.maybe_radio();
            }
            Act::Activate => self.activate(),
            Act::Back => self.back(),
            Act::Refresh => self.refresh(),
            Act::PlayNext => self.play_next_target(),
            Act::AddToQueue if self.view == View::Queue && self.highlighted().is_some() => {
                self.status = "Already in the queue (N moves it to play next)".into()
            }
            Act::AddToQueue => {
                if let Some(t) = self.target_track("add it to the queue") {
                    self.append(vec![t]);
                }
            }
            Act::AddAll if self.view == View::Queue => self.status = "Already in the queue".into(),
            Act::AddAll => match self.list_context() {
                Some((tracks, _)) => self.append(tracks),
                None => self.status = "No song list here to add".into(),
            },
            Act::Radio => self.radio_target(),
            Act::Like => self.toggle_like(),
            Act::AddToPlaylist => self.open_picker(),
            Act::Move(d) => self.move_cursor(d),
            Act::SearchType => {
                self.kind = self.kind.next();
                self.run_search();
            }
            Act::Remove => {
                if let Some(i) = self.queue_sel.selected() {
                    self.remove_from_queue(i);
                }
            }
            Act::Shift(up) => {
                if let Some(i) = self.queue_sel.selected().filter(|&i| i < self.queue.len()) {
                    self.move_in_queue(i, up);
                }
            }
            Act::ClearQueue => {
                self.snapshot_queue();
                match self.cur.and_then(|c| self.queue.get(c).cloned()) {
                    Some(t) => {
                        self.queue = vec![t];
                        self.cur = Some(0);
                    }
                    None => self.queue.clear(),
                }
                self.queue_sel.select(self.cur);
                self.sync_next();
                self.status = "Queue cleared — u to undo".into();
            }
            Act::LibSource(m) => {
                let m = m.unwrap_or(if self.lib_mode == LibMode::Local { LibMode::Yt } else { LibMode::Local });
                self.lib_mode = m;
                if m == LibMode::Yt && self.yt_lists.is_empty() && !self.yt_loading {
                    self.load_yt_lists();
                }
            }
            // Both work on local files, so they switch to them.
            Act::Filter => {
                self.lib_mode = LibMode::Local;
                self.begin_input(Input::LibFilter, self.lib_filter.clone());
            }
            Act::Folder => {
                self.lib_mode = LibMode::Local;
                self.begin_input(Input::LibDir, self.cfg.library_dir.clone());
            }
            Act::EqBand(d) => self.eq_band = (self.eq_band as isize + d as isize).clamp(0, eq::BANDS.len() as isize - 1) as usize,
            Act::EqGain(d) => {
                let g = &mut self.cfg.eq.gains[self.eq_band];
                *g = (*g + d as f64 * 0.5).clamp(-eq::MAX_GAIN, eq::MAX_GAIN);
                self.cfg.eq.enabled = true;
                self.cfg.eq.preset = "Custom".into();
                self.apply_eq();
            }
            Act::EqToggle => {
                self.cfg.eq.enabled = !self.cfg.eq.enabled;
                self.apply_eq();
            }
            Act::EqPreset(next) => {
                let n = eq::PRESETS.len();
                let i = self.cfg.eq.preset_index();
                self.cfg.eq.apply_preset(if next { (i + 1) % n } else { (i + n - 1) % n });
                self.apply_eq();
            }
            Act::EqReset => {
                self.cfg.eq.apply_preset(0);
                self.apply_eq();
            }
            Act::Clarity => {
                self.cfg.eq.clarity = !self.cfg.eq.clarity;
                self.apply_eq();
            }
            Act::ClarityProfile(next) => {
                let n = eq::CLARITY.len();
                let i = self.cfg.eq.clarity_preset;
                self.cfg.eq.clarity_preset = if next { (i + 1) % n } else { (i + n - 1) % n };
                self.cfg.eq.clarity = true;
                self.apply_eq();
            }
            // Signed-in account rows change only through Enter (twice).
            Act::Change(back) => {
                if !self.signed_in_row(self.setting_row()) {
                    self.change_setting(back);
                }
            }
        }
    }

    /// Enter: play the highlighted song, open the highlighted
    /// album/playlist/artist, or change the highlighted setting. Search
    /// results and Discover insert the song after the current one; album,
    /// playlist and Library lists become the queue from the song on.
    pub(super) fn activate(&mut self) {
        match self.view {
            View::Settings => self.settings_enter(),
            View::Search | View::Queue | View::Library => {
                if let Some((tracks, i)) = self.list_context() {
                    if self.view == View::Queue {
                        self.play_index(i);
                    } else if self.view == View::Search && self.browse.is_none() {
                        self.play_now(tracks[i].clone());
                    } else {
                        self.play_list(tracks, i);
                    }
                } else if let Some(Highlight::Collection(c)) = self.highlighted() {
                    self.open_collection(c, self.view == View::Library);
                }
            }
            View::Lyrics | View::Eq => {}
        }
    }

    /// Esc / Backspace: leave an opened page, else clear the search or filter.
    pub(super) fn back(&mut self) {
        match self.view {
            View::Search if self.browse.is_some() => self.browse = None,
            View::Search if !self.query.is_empty() => {
                self.query.clear();
                self.results = None;
            }
            View::Library if self.lib_mode == LibMode::Yt => self.yt_open = None,
            View::Library => self.set_lib_filter(String::new()),
            View::Settings if self.lfm_auth.is_some() || self.lfm_busy => self.lfm_cancel(),
            _ => {}
        }
    }

    /// S: load what the view shows again.
    pub(super) fn refresh(&mut self) {
        match self.view {
            View::Search => match self.browse.as_ref().map(|(c, _)| c.clone()) {
                Some(c) => self.open_collection(c, false),
                None if self.discover_mode() => self.load_home(),
                None => self.run_search(),
            },
            View::Library if self.lib_mode == LibMode::Yt => match self.yt_open.as_ref().map(|(c, _)| c.clone()) {
                Some(c) => self.open_collection(c, true),
                None => self.load_yt_lists(),
            },
            View::Library => self.rescan(),
            _ => {}
        }
    }

    pub(super) fn move_cursor(&mut self, d: isize) {
        if self.view == View::Settings {
            self.set_sel = (self.set_sel as isize).saturating_add(d).clamp(0, SETTINGS.len() as isize - 1) as usize;
            return;
        }
        let discover = self.view == View::Search && self.discover_mode();
        let before = self.discover_sel.selected().unwrap_or(0);
        if let Some((state, len)) = self.active_list() {
            nav(state, len, d);
        }
        if discover {
            self.skip_headers(before);
        }
    }

    /// The highlighted song and the list it is in, for "play from here" / "add all".
    pub(super) fn list_context(&self) -> Option<(Vec<Track>, usize)> {
        match self.view {
            View::Search => self.selected_search_track(),
            View::Queue => self.queue_sel.selected().filter(|&i| i < self.queue.len()).map(|i| (self.queue.clone(), i)),
            View::Library if self.lib_mode == LibMode::Yt => {
                let (_, t) = self.yt_open.as_ref()?;
                self.yt_open_sel.selected().filter(|&i| i < t.len()).map(|i| (t.clone(), i))
            }
            View::Library => {
                let i = self.lib_sel.selected().filter(|&i| i < self.lib_vis.len())?;
                Some((self.lib_vis.iter().map(|&j| self.library[j].clone()).collect(), i))
            }
            _ => None,
        }
    }

    /// N: in the Queue the highlighted song moves up to play next; elsewhere it is inserted there.
    pub(super) fn play_next_target(&mut self) {
        if self.view == View::Queue {
            if let Some(i) = self.queue_sel.selected().filter(|&i| i < self.queue.len()) {
                match self.cur {
                    Some(c) if c == i => self.status = "That song is playing now".into(),
                    Some(c) => {
                        let t = self.queue.remove(i);
                        let c = if i < c { c - 1 } else { c };
                        self.cur = Some(c);
                        self.queue.insert(c + 1, t);
                        self.queue_sel.select(Some(c + 1));
                        self.sync_next();
                        self.status = "Moved to play next".into();
                    }
                    None => self.status = "Nothing is playing".into(),
                }
                return;
            }
        }
        if let Some(t) = self.target_track("play it next") {
            self.play_next(vec![t]);
        }
    }

    /// r: radio from the highlighted song (from the playing one without restarting it).
    pub(super) fn radio_target(&mut self) {
        let Some(t) = self.target_track("start a radio from it") else { return };
        if t.video_id().is_none() {
            self.status = "Radio needs a YouTube song".into();
        } else if self.current().is_some_and(|c| c.source == t.source) {
            self.radio_from_current();
        } else {
            self.start_radio(t);
        }
    }

    pub(super) fn cycle_view(&mut self, by: usize) {
        let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
        self.view = View::ALL[(i + by) % View::ALL.len()];
    }

    pub(super) fn begin_input(&mut self, target: Input, initial: String) {
        self.input = Some(target);
        self.input_buf = initial;
    }

    pub(super) fn on_input(&mut self, target: Input, k: KeyEvent) {
        // Terminals without bracketed paste (e.g. Rio/WezTerm through ConPTY)
        // deliver a pasted cookies.txt as keystrokes: its newlines arrive as
        // Enter and its column separators as Tab. An Enter with more input
        // already queued is part of the paste, not a submit.
        let pasting = || event::poll(Duration::ZERO).unwrap_or(false);
        match k.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter if target == Input::YtCookies && pasting() => self.input_buf.push('\n'),
            KeyCode::Tab if target == Input::YtCookies => self.input_buf.push('\t'),
            KeyCode::Char('v') if target == Input::YtCookies && k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input = None;
                self.input_buf.clear();
                self.submit_yt_cookies(None);
            }
            KeyCode::Enter => {
                self.input = None;
                let v = std::mem::take(&mut self.input_buf).trim().to_string();
                match target {
                    Input::Search => {
                        self.query = v;
                        self.run_search();
                    }
                    Input::LibFilter => {
                        self.set_lib_filter(v);
                        self.lib_sel.select(Some(0));
                    }
                    Input::LibDir => {
                        self.cfg.library_dir = v;
                        self.cfg.save();
                        self.rescan();
                    }
                    // A pending approval belongs to the old key; start over with the new one.
                    Input::LfmKey | Input::LfmSecret => {
                        if target == Input::LfmKey {
                            self.cfg.lastfm_api_key = v;
                        } else {
                            self.cfg.lastfm_api_secret = v;
                        }
                        self.cfg.save();
                        if self.lfm_auth.is_some() || self.lfm_busy {
                            self.lfm_cancel();
                        }
                    }
                    // Nothing typed: read the system clipboard instead.
                    Input::YtCookies => self.submit_yt_cookies(Some(v).filter(|v| !v.is_empty())),
                    Input::NewPlaylist => self.create_playlist(v),
                }
            }
            KeyCode::Backspace => {
                self.input_buf.pop();
                if target == Input::LibFilter {
                    self.set_lib_filter(self.input_buf.clone());
                }
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => self.input_buf.clear(),
            KeyCode::Char(c) => {
                self.input_buf.push(c);
                if target == Input::LibFilter {
                    self.set_lib_filter(self.input_buf.clone());
                    self.lib_sel.select(Some(0));
                }
            }
            _ => {}
        }
    }

    pub fn highlighted(&self) -> Option<Highlight> {
        match self.view {
            View::Search => match self.selected_search_track() {
                Some((tracks, i)) => Some(Highlight::Track(tracks[i].clone())),
                None => self.selected_search_collection().map(Highlight::Collection),
            },
            View::Library if self.lib_mode == LibMode::Yt => match &self.yt_open {
                Some((_, t)) => self.yt_open_sel.selected().and_then(|i| t.get(i)).cloned().map(Highlight::Track),
                None => self.yt_lists_sel.selected().and_then(|i| self.yt_lists.get(i)).cloned().map(Highlight::Collection),
            },
            View::Queue => self.queue_sel.selected().and_then(|i| self.queue.get(i)).cloned().map(Highlight::Track),
            View::Library => {
                let vis = self.lib_visible();
                self.lib_sel.selected().and_then(|i| vis.get(i)).map(|&j| Highlight::Track(self.library[j].clone()))
            }
            _ => None,
        }
    }

    pub(super) fn active_list(&mut self) -> Option<(&mut ListState, usize)> {
        match self.view {
            View::Search if self.discover_mode() => Some((&mut self.discover_sel, self.discover.len())),
            View::Library if self.lib_mode == LibMode::Yt => match &self.yt_open {
                Some((_, t)) => Some((&mut self.yt_open_sel, t.len())),
                None => Some((&mut self.yt_lists_sel, self.yt_lists.len())),
            },
            View::Search => match (&self.browse, &self.results) {
                (Some((_, t)), _) => Some((&mut self.browse_sel, t.len())),
                (None, Some(Results::Tracks(t))) => Some((&mut self.search_sel, t.len())),
                (None, Some(Results::Collections(c))) => Some((&mut self.search_sel, c.len())),
                _ => None,
            },
            View::Queue => Some((&mut self.queue_sel, self.queue.len())),
            View::Library => {
                let n = self.lib_visible().len();
                Some((&mut self.lib_sel, n))
            }
            _ => None,
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A click never throws typed (or pasted) text away: only Enter
                // or Esc end a text field.
                if self.input.is_some() {
                    self.status = "Still typing — Enter to confirm, Esc to cancel".into();
                    return;
                }
                if self.help || self.picker.is_some() {
                    self.picker = None;
                    self.help = false;
                    return;
                }
                if let Some(&(_, code)) = self.buttons.iter().find(|(r, _)| r.contains(pos)) {
                    self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
                    return;
                }
                let Some(area) = self.list_area.filter(|a| a.contains(pos)) else { return };
                if pos.y == area.y || pos.y + 1 >= area.y + area.height {
                    return; // border
                }
                let Some((state, len)) = self.active_list() else { return };
                let row = state.offset() + (pos.y - area.y - 1) as usize;
                if row >= len {
                    return;
                }
                state.select(Some(row));
                // A second click on the same row within 400ms plays it.
                let now = std::time::Instant::now();
                let double = self.last_click.is_some_and(|(r, t)| r == row && now.duration_since(t).as_millis() < 400);
                self.last_click = if double { None } else { Some((row, now)) };
                if double {
                    self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if self.list_area.is_some_and(|a| a.contains(pos)) {
                    self.move_cursor(if up { -3 } else { 3 });
                } else if m.row + 7 >= self.term_height {
                    // Wheel over the player bar changes volume.
                    self.mpv_cmd(json!(["add", "volume", if up { 5 } else { -5 }]));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testutil::{ids, t};
    use crate::{app::DItem, ytm::Collection};
    use std::{collections::HashSet, sync::mpsc};
    /// Every key the app could see: printable ASCII and the named keys.
    fn all_keys() -> Vec<KeyCode> {
        let mut keys: Vec<KeyCode> = (' '..='~').map(KeyCode::Char).collect();
        keys.extend([
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Delete,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ]);
        keys.extend((1..=12).map(KeyCode::F));
        keys
    }

    #[test]
    fn keymap_globals_not_shadowed() {
        // Global keys are looked up first, so a view binding for one would be
        // dead code and a second meaning for the key: there must be none.
        for view in View::ALL {
            for k in all_keys() {
                if let (Some(g), Some(v)) = (global_act(k), view_act(view, k)) {
                    panic!("{k:?} is global ({g:?}) but {view:?} binds it to {v:?}");
                }
            }
        }
        // Keys that used to change meaning by view now have one.
        for k in ['l', 'c', 'f', 'S', 'n', 'p'] {
            let k = KeyCode::Char(k);
            let acts: HashSet<String> = View::ALL.iter().filter_map(|&v| global_act(k).or(view_act(v, k))).map(|a| format!("{a:?}")).collect();
            assert!(acts.len() <= 1, "{k:?} means {acts:?}");
        }
    }

    #[test]
    fn keymap_keys_are_in_help() {
        let help = crate::ui::HELP;
        let key_tokens: HashSet<&str> = help.iter().flat_map(|(_, rows)| rows.iter().flat_map(|(k, _)| k.split_whitespace())).collect();
        let text: String = help.iter().flat_map(|(_, rows)| rows.iter().map(|(k, d)| format!("{k} {d} "))).collect();
        // Aliases left out of the help to keep it on one 80x24 screen; listed in KEYS.md.
        let aliases = [KeyCode::Char('='), KeyCode::Home, KeyCode::End, KeyCode::F(11), KeyCode::F(12)];
        for k in all_keys() {
            if aliases.contains(&k) || (global_act(k).is_none() && View::ALL.iter().all(|&v| view_act(v, k).is_none())) {
                continue;
            }
            let documented = match k {
                KeyCode::Char('1'..='6') => key_tokens.contains("1-6"),
                KeyCode::Char(' ') => key_tokens.contains("Space"),
                KeyCode::Char(c) => key_tokens.contains(c.to_string().as_str()),
                KeyCode::Up | KeyCode::Down => text.contains("↑↓"),
                KeyCode::Left => text.contains('←'),
                KeyCode::Right => text.contains('→'),
                KeyCode::Backspace => text.contains('⌫'),
                KeyCode::BackTab => text.contains("⇧Tab"),
                KeyCode::Delete => key_tokens.contains("Del"),
                KeyCode::PageUp => key_tokens.contains("PgUp"),
                KeyCode::PageDown => key_tokens.contains("PgDn"),
                k => key_tokens.contains(k.to_string().as_str()),
            };
            assert!(documented, "{k:?} is bound but missing from the help screen");
        }
    }

    #[test]
    fn song_keys_act_on_the_highlighted_row() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.queue = vec![t("a"), t("b")];
        app.cur = Some(0);
        app.view = View::Queue;
        app.queue_sel.select(Some(1));
        assert!(matches!(app.target(), Some(Highlight::Track(x)) if x.title == "b"));
        // No list in the view: the playing song.
        app.view = View::Lyrics;
        assert!(matches!(app.target(), Some(Highlight::Track(x)) if x.title == "a"));
        // An album highlighted: song keys say so instead of using the playing song.
        app.view = View::Search;
        app.query = "x".into();
        let album = Collection { browse_id: "MPREb_x".into(), title: "Album".into(), subtitle: String::new(), art: String::new() };
        app.results = Some(Results::Collections(vec![album]));
        app.search_sel.select(Some(0));
        assert!(app.target_track("like it").is_none());
        assert!(app.status.contains("Open Album"), "{}", app.status);
    }

    #[test]
    fn enter_in_search_inserts_and_on_a_page_plays_the_list() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.queue = vec![t("a"), t("b")];
        app.cur = Some(0);
        app.view = View::Search;
        // Search results: the song plays now, after the current one; the rest stays.
        app.query = "x".into();
        app.results = Some(Results::Tracks(vec![t("x"), t("y")]));
        app.search_sel.select(Some(1));
        app.run(Act::Activate);
        assert_eq!((ids(&app.queue), app.cur), (vec!["a".to_string(), "y".into(), "b".into()], Some(1)));
        // Discover: the same.
        app.query.clear();
        app.discover = vec![DItem::Header("one".into()), DItem::Track(t("d")), DItem::Track(t("e"))];
        app.discover_sel.select(Some(2));
        app.run(Act::Activate);
        assert_eq!((ids(&app.queue), app.cur), (vec!["a".to_string(), "y".into(), "e".into(), "b".into()], Some(2)));
        // An opened album: its songs from the highlighted one become the queue.
        let album = Collection { browse_id: "MPREb_x".into(), title: "Album".into(), subtitle: String::new(), art: String::new() };
        app.browse = Some((album, vec![t("p"), t("q")]));
        app.browse_sel.select(Some(1));
        app.run(Act::Activate);
        assert_eq!((ids(&app.queue), app.cur), (vec!["p".to_string(), "q".into()], Some(1)));
    }

    #[test]
    fn discover_list_is_the_shelf() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.view = View::Search;
        app.query.clear();
        app.browse = None;
        app.discover = vec![
            DItem::Header("one".into()),
            DItem::Track(t("a")),
            DItem::Header("two".into()),
            DItem::Track(t("b")),
            DItem::Track(t("c")),
        ];
        app.discover_sel.select(Some(4));
        let (tracks, i) = app.list_context().unwrap();
        assert_eq!((ids(&tracks), i), (vec!["b".to_string(), "c".to_string()], 1));
    }

    #[test]
    fn click_keeps_typed_text() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        app.begin_input(Input::YtCookies, String::new());
        app.on_paste("SID=abc; HSID=def".into());
        let click = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: 3, modifiers: KeyModifiers::NONE };
        app.on_mouse(click);
        assert_eq!(app.input, Some(Input::YtCookies));
        assert_eq!(app.input_buf, "SID=abc; HSID=def");
    }
}
