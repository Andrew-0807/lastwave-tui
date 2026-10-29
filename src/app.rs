use crate::{
    aifilter,
    art::{self, Covers},
    config::{self, Config, State, REPLAYGAIN, YTDL_FORMATS},
    eq, lastfm, library,
    lyrics::{self, Lyrics},
    model::Track,
    mpv::{Mpv, MpvEvent},
    ytm::{self, AddTarget, Collection, Entry, Kind, Results, Shelf},
};
use image::DynamicImage;
use ratatui::{
    crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::{Position, Rect},
    widgets::ListState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::Sender,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// System clipboard text, via the platform's own tool (no extra crate).
fn read_clipboard() -> Result<String, String> {
    use std::process::Command;
    #[cfg(windows)]
    let out = {
        use std::os::windows::process::CommandExt;
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Get-Clipboard -Raw"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
    };
    #[cfg(target_os = "macos")]
    let out = Command::new("pbpaste").output();
    #[cfg(all(unix, not(target_os = "macos")))]
    let out = Command::new("wl-paste")
        .arg("--no-newline")
        .output()
        .or_else(|_| Command::new("xclip").args(["-selection", "clipboard", "-o"]).output());
    let out = out.map_err(|e| format!("could not read the clipboard: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err("The clipboard is empty — copy your music.youtube.com cookies first".into());
    }
    Ok(text)
}

pub enum Msg {
    Mpv(MpvEvent),
    Search(u64, Result<Results, String>),
    /// (opened from the Library view?, page, tracks)
    Browse(bool, Collection, Result<Vec<Track>, String>),
    Home(Result<Vec<Shelf>, String>),
    YtLogin(Result<(String, String), String>),
    YtLists(Result<Vec<Collection>, String>),
    Radio(Result<Vec<Track>, String>),
    Lyrics(String, Option<Lyrics>),
    Library(Vec<Track>),
    Status(String),
    LfmToken(Result<String, String>),
    LfmSession(Result<(String, String), String>),
    Art(String, Option<DynamicImage>),
    AiList(Result<usize, String>),
    Liked(Result<HashSet<String>, String>),
    /// (video id, liked, result)
    LikeResult(String, bool, Result<(), String>),
    AddTargets(String, Result<Vec<AddTarget>, String>),
    /// (playlist title, browse id of the playlist added to)
    Added(String, Result<String, String>),
}

/// One row of the Discover list.
#[derive(Clone, Serialize, Deserialize)]
pub enum DItem {
    Header(String),
    Track(Track),
    Coll(Collection),
}

/// Last fetched account views, shown instantly at startup while fresh
/// copies load in the background. Only valid for the account in `user`.
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    user: String,
    discover: Vec<DItem>,
    yt_lists: Vec<Collection>,
    /// Tracks of library playlists that were opened, by browse id.
    pages: std::collections::HashMap<String, Vec<Track>>,
    #[serde(default)]
    liked: HashSet<String>,
}

const CACHE_FILE: &str = "cache.json";
const MAX_CACHED_PAGES: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LibMode {
    Local,
    Yt,
}

/// "Add to playlist" popup for one song. Row 0 is "+ New playlist…".
pub struct Picker {
    pub track: Track,
    pub items: Vec<AddTarget>,
    pub sel: ListState,
    pub loading: bool,
}

/// What the details drawer shows.
pub enum Highlight {
    Track(Track),
    Collection(Collection),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Search,
    Queue,
    Library,
    Lyrics,
    Eq,
    Settings,
}

impl View {
    pub const ALL: [View; 6] = [View::Search, View::Queue, View::Library, View::Lyrics, View::Eq, View::Settings];
    pub fn title(self) -> &'static str {
        match self {
            View::Search => "Search",
            View::Queue => "Queue",
            View::Library => "Library",
            View::Lyrics => "Lyrics",
            View::Eq => "Equalizer",
            View::Settings => "Settings",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Search,
    LibFilter,
    LibDir,
    LfmKey,
    LfmSecret,
    YtCookies,
    NewPlaylist,
}

pub const SETTINGS: &[&str] = &[
    "Output device",
    "Bit-perfect (exclusive)",
    "ReplayGain",
    "Gapless",
    "YouTube audio",
    "Radio autoplay",
    "Library folder",
    "Last.fm API key",
    "Last.fm API secret",
    "Last.fm account",
    "YouTube Music account",
    "Use account for playback",
    "Cover images",
    "Skip AI songs",
    "Hide AI songs in Discover",
];

const SET_YT_ACCOUNT: usize = 10;

pub struct App {
    pub cfg: Config,
    tx: Sender<Msg>,
    pub mpv: Option<Mpv>,
    pub view: View,
    pub input: Option<Input>,
    pub input_buf: String,
    pub help: bool,
    pub quit: bool,
    pub status: String,

    pub query: String,
    pub kind: Kind,
    pub results: Option<Results>,
    pub search_sel: ListState,
    search_seq: u64,
    pub searching: bool,
    pub browse: Option<(Collection, Vec<Track>)>,
    pub browse_sel: ListState,
    pub discover: Vec<DItem>,
    pub discover_sel: ListState,
    pub discover_loading: bool,

    pub queue: Vec<Track>,
    pub cur: Option<usize>,
    pub queue_sel: ListState,
    mpv_next: Option<String>,
    radio_busy: bool,

    pub library: Vec<Track>,
    pub lib_filter: String,
    pub lib_sel: ListState,
    pub lib_scanning: bool,
    pub lib_mode: LibMode,
    pub yt_lists: Vec<Collection>,
    pub yt_lists_sel: ListState,
    pub yt_open: Option<(Collection, Vec<Track>)>,
    pub yt_open_sel: ListState,
    pub yt_loading: bool,
    cache: Cache,
    /// Video ids in the account's Liked Music.
    pub liked: HashSet<String>,
    pub picker: Option<Picker>,

    pub pos: f64,
    pub dur: f64,
    pub paused: bool,
    pub volume: f64,
    pub idle: bool,
    pub codec: String,
    pub params: Value,
    pub out_params: Value,
    pub bitrate: f64,
    pub ao: String,
    pub af: String,

    pub lyrics: Option<Lyrics>,
    pub lyrics_loading: bool,
    lyrics_key: String,

    pub eq_band: usize,
    pub set_sel: usize,
    pub devices: Vec<(String, String)>,
    lfm_token: Option<String>,

    pub covers: Option<Covers>,
    /// Clickable regions laid out by the last draw: (area, key it presses).
    pub buttons: Vec<(Rect, KeyCode)>,
    /// The list widget of the current view, for click-to-select / wheel.
    pub list_area: Option<Rect>,
    pub term_height: u16,
    last_click: Option<(usize, std::time::Instant)>,

    played: f64,
    last_pos: f64,
    started_at: u64,
    finished: bool,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn nav(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    state.select(Some((cur + delta).clamp(0, len as isize - 1) as usize));
}

fn list_nav(state: &mut ListState, len: usize, code: KeyCode) -> bool {
    match code {
        KeyCode::Up | KeyCode::Char('k') => nav(state, len, -1),
        KeyCode::Down | KeyCode::Char('j') => nav(state, len, 1),
        KeyCode::PageUp => nav(state, len, -15),
        KeyCode::PageDown => nav(state, len, 15),
        KeyCode::Home | KeyCode::Char('g') => nav(state, len, -(len as isize)),
        KeyCode::End | KeyCode::Char('G') => nav(state, len, len as isize),
        _ => return false,
    }
    true
}

impl App {
    pub fn new(tx: Sender<Msg>) -> Self {
        let cfg = Config::load();
        let state = State::load();
        let volume = cfg.volume;
        let mut queue_sel = ListState::default();
        queue_sel.select(state.cur);
        Self {
            cfg,
            tx,
            mpv: None,
            view: View::Search,
            input: None,
            input_buf: String::new(),
            help: false,
            quit: false,
            status: "Press / to search, ? for keys".into(),
            query: String::new(),
            kind: Kind::Songs,
            results: None,
            search_sel: ListState::default(),
            search_seq: 0,
            searching: false,
            browse: None,
            browse_sel: ListState::default(),
            discover: Vec::new(),
            discover_sel: ListState::default(),
            discover_loading: false,
            cur: state.cur.filter(|&c| c < state.queue.len()),
            queue: state.queue,
            queue_sel,
            mpv_next: None,
            radio_busy: false,
            library: Vec::new(),
            lib_filter: String::new(),
            lib_sel: ListState::default(),
            lib_scanning: false,
            lib_mode: LibMode::Local,
            yt_lists: Vec::new(),
            yt_lists_sel: ListState::default(),
            yt_open: None,
            yt_open_sel: ListState::default(),
            yt_loading: false,
            cache: Cache::default(),
            liked: HashSet::new(),
            picker: None,
            pos: 0.0,
            dur: 0.0,
            paused: false,
            volume,
            idle: true,
            codec: String::new(),
            params: Value::Null,
            out_params: Value::Null,
            bitrate: 0.0,
            ao: String::new(),
            af: String::new(),
            lyrics: None,
            lyrics_loading: false,
            lyrics_key: String::new(),
            eq_band: 0,
            set_sel: 0,
            devices: Vec::new(),
            lfm_token: None,
            covers: None,
            buttons: Vec::new(),
            list_area: None,
            term_height: 0,
            last_click: None,
            played: 0.0,
            last_pos: 0.0,
            started_at: 0,
            finished: true,
        }
    }

    pub fn start(&mut self) {
        match Mpv::spawn(&self.cfg, self.tx.clone()) {
            Ok(m) => {
                self.devices = m
                    .get("audio-device-list")
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .map(|d| {
                        (
                            d["name"].as_str().unwrap_or_default().to_string(),
                            d["description"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect();
                self.mpv = Some(m);
            }
            Err(e) => self.status = e,
        }
        self.rescan();
        // Loaded even with skipping off: the [AI] marks use it too.
        if aifilter::load_cached() {
            self.spawn(|| Msg::AiList(aifilter::refresh()));
        }
        if let Some(c) = self.cur.filter(|&c| self.cfg.skip_ai && self.is_ai(&self.queue[c])) {
            self.cur = self.next_playable(c);
            self.queue_sel.select(self.cur);
        }
        if let Some(c) = config::load_cookies() {
            ytm::set_auth(Some(c));
        }
        // Show the last session's account views immediately; the fetches
        // below replace them when they land.
        let cache: Cache = config::load(CACHE_FILE);
        let who = if ytm::signed_in() { self.cfg.yt_user.clone() } else { String::new() };
        if cache.user == who {
            self.discover = cache.discover.clone();
            self.discover_sel.select((self.discover.len() > 1).then_some(1));
            self.hide_ai_discover();
            if ytm::signed_in() {
                self.yt_lists = cache.yt_lists.clone();
                self.yt_lists_sel.select((!self.yt_lists.is_empty()).then_some(0));
                self.liked = cache.liked.clone();
            }
            self.cache = cache;
        } else {
            self.cache.user = who;
        }
        self.load_yt_lists();
        self.load_liked();
        self.load_home();
    }

    fn save_cache(&mut self) {
        self.cache.user = if ytm::signed_in() { self.cfg.yt_user.clone() } else { String::new() };
        self.cache.discover = self.discover.clone();
        self.cache.yt_lists = self.yt_lists.clone();
        self.cache.liked = self.liked.clone();
        config::save(CACHE_FILE, &self.cache);
    }

    pub fn discover_mode(&self) -> bool {
        self.browse.is_none() && self.query.trim().is_empty()
    }

    fn load_home(&mut self) {
        self.discover_loading = true;
        self.spawn(|| Msg::Home(ytm::home()));
    }

    fn load_yt_lists(&mut self) {
        if !ytm::signed_in() {
            return;
        }
        self.yt_loading = true;
        self.spawn(|| Msg::YtLists(ytm::library_playlists()));
    }

    fn load_liked(&mut self) {
        if ytm::signed_in() {
            self.spawn(|| Msg::Liked(ytm::liked_ids()));
        }
    }

    pub fn is_liked(&self, t: &Track) -> bool {
        t.video_id().is_some_and(|id| self.liked.contains(id))
    }

    /// The highlighted song, else the playing one.
    fn action_track(&self) -> Option<Track> {
        match self.highlighted() {
            Some(Highlight::Track(t)) => Some(t),
            _ => self.current().cloned(),
        }
    }

    /// The song an account action applies to and its video id, or why there is none.
    fn account_target(&mut self, what: &str) -> Option<(Track, String)> {
        if !ytm::signed_in() {
            self.status = format!("Sign in to YouTube Music (Library → y) to {what}");
            return None;
        }
        let t = self.action_track()?;
        match t.video_id().map(str::to_string) {
            Some(id) => Some((t, id)),
            None => {
                self.status = "Local files aren't on YouTube Music".into();
                None
            }
        }
    }

    fn toggle_like(&mut self) {
        let Some((t, id)) = self.account_target("like songs") else { return };
        let liked = !self.liked.contains(&id);
        if liked {
            self.liked.insert(id.clone());
        } else {
            self.liked.remove(&id);
        }
        self.status = format!("{} {}", if liked { "♥ Liked" } else { "Removed like:" }, t.title);
        self.spawn(move || {
            let r = ytm::set_liked(&id, liked);
            Msg::LikeResult(id, liked, r)
        });
    }

    fn open_picker(&mut self) {
        let Some((t, id)) = self.account_target("add to playlists") else { return };
        let mut sel = ListState::default();
        sel.select(Some(0));
        self.picker = Some(Picker { track: t, items: Vec::new(), sel, loading: true });
        self.spawn(move || {
            let r = ytm::add_targets(&id);
            Msg::AddTargets(id, r)
        });
    }

    fn key_picker(&mut self, code: KeyCode) {
        let Some(p) = &mut self.picker else { return };
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('P') => self.picker = None,
            KeyCode::Enter => match p.sel.selected() {
                Some(0) => self.begin_input(Input::NewPlaylist, String::new()),
                Some(i) => {
                    let Some(target) = p.items.get(i - 1).cloned() else { return };
                    if target.has {
                        self.status = format!("Already in {}", target.title);
                        return;
                    }
                    let id = p.track.video_id().unwrap_or_default().to_string();
                    self.picker = None;
                    self.status = format!("Adding to {}…", target.title);
                    self.spawn(move || {
                        let r = ytm::add_to_playlist(&target.playlist_id, &id).map(|_| browse_id_of(&target.playlist_id));
                        Msg::Added(target.title, r)
                    });
                }
                None => {}
            },
            code => {
                let len = p.items.len() + 1;
                list_nav(&mut p.sel, len, code);
            }
        }
    }

    fn create_playlist(&mut self, title: String) {
        let Some(p) = self.picker.take() else { return };
        if title.is_empty() {
            return;
        }
        let id = p.track.video_id().unwrap_or_default().to_string();
        self.status = format!("Creating {title}…");
        self.spawn(move || {
            let r = ytm::create_playlist(&title, &id).map(|pid| browse_id_of(&pid));
            Msg::Added(title, r)
        });
    }

    fn open_collection(&mut self, c: Collection, lib: bool) {
        if lib {
            self.yt_loading = true;
            // Cached tracks first; the fetch below refreshes them.
            if let Some(t) = self.cache.pages.get(&c.browse_id) {
                self.yt_open_sel.select((!t.is_empty()).then_some(0));
                self.yt_open = Some((c.clone(), t.clone()));
            }
        } else {
            self.searching = true;
        }
        self.status = format!("Opening {}…", c.title);
        self.spawn(move || {
            let r = ytm::browse(&c.browse_id);
            Msg::Browse(lib, c, r)
        });
    }

    fn start_yt_login(&mut self) {
        self.view = View::Settings;
        self.set_sel = SET_YT_ACCOUNT;
        self.begin_input(Input::YtCookies, String::new());
        self.status = "Copy your music.youtube.com cookies, then press Enter here (reads the clipboard) — or paste them / a cookies.txt path first".into();
    }

    /// `None` reads the cookies from the system clipboard.
    fn submit_yt_cookies(&mut self, raw: Option<String>) {
        self.status = "Signing in…".into();
        self.spawn(move || {
            let previous = config::load_cookies();
            let r = raw.map_or_else(read_clipboard, Ok).and_then(|raw| ytm::normalize_cookies(&raw)).and_then(|c| {
                ytm::set_auth(Some(c.clone()));
                ytm::account_name().map(|n| (c, n))
            });
            if r.is_err() {
                ytm::set_auth(previous);
            }
            Msg::YtLogin(r)
        });
    }

    fn yt_sign_out(&mut self) {
        ytm::set_auth(None);
        config::save_cookies(None);
        self.cfg.yt_user.clear();
        self.cfg.save();
        self.yt_lists.clear();
        self.yt_open = None;
        self.liked.clear();
        self.picker = None;
        self.cache = Cache::default();
        config::save(CACHE_FILE, &self.cache);
        self.mpv_set("ytdl-raw-options", "");
        self.status = "Signed out of YouTube Music".into();
        self.load_home();
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
                self.lib_filter = self.input_buf.clone();
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.finish_track();
        self.cfg.volume = self.volume;
        self.cfg.save();
        State { queue: self.queue.clone(), cur: self.cur }.save();
        if let Some(m) = &mut self.mpv {
            m.quit();
        }
    }

    fn spawn<F: FnOnce() -> Msg + Send + 'static>(&self, f: F) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
    }

    fn mpv_cmd(&self, args: Value) {
        if let Some(m) = &self.mpv {
            m.cmd(args);
        }
    }

    fn mpv_set(&self, prop: &str, v: impl Into<Value>) {
        if let Some(m) = &self.mpv {
            m.set(prop, v);
        }
    }

    // ---- playback / queue ---------------------------------------------

    pub fn current(&self) -> Option<&Track> {
        self.cur.and_then(|i| self.queue.get(i))
    }

    pub fn is_ai(&self, t: &Track) -> bool {
        aifilter::is_ai(t, &self.cfg.ai_flagged, &self.cfg.ai_allowed)
    }

    /// Albums by an AI artist ("Album • Artist • 2024") and AI artist pages.
    /// Playlists are left alone: their subtitle names the curator.
    pub fn is_ai_coll(&self, c: &Collection) -> bool {
        let ai = |n: &str| aifilter::is_ai_artist(n, &self.cfg.ai_flagged, &self.cfg.ai_allowed);
        match c.browse_id.get(..2) {
            Some("UC") => ai(&c.title),
            Some("MP") => c.subtitle.split(" • ").map(str::trim).any(|p| {
                !p.is_empty() && !ytm::TYPE_WORDS.contains(&p) && !p.bytes().all(|b| b.is_ascii_digit()) && ai(p)
            }),
            _ => false,
        }
    }

    /// First queue index from `from` on that should play: with "Skip AI
    /// songs" on, tracks by AI artists are passed over.
    fn next_playable(&self, from: usize) -> Option<usize> {
        (from..self.queue.len()).find(|&i| !self.cfg.skip_ai || !self.is_ai(&self.queue[i]))
    }

    /// Last queue index before `before` that should play.
    fn prev_playable(&self, before: usize) -> Option<usize> {
        (0..before).rev().find(|&i| !self.cfg.skip_ai || !self.is_ai(&self.queue[i]))
    }

    /// With "Skip AI songs" on, drop AI tracks; returns (kept, dropped count).
    fn drop_ai(&self, tracks: Vec<Track>) -> (Vec<Track>, usize) {
        let n = tracks.len();
        let kept: Vec<Track> = tracks.into_iter().filter(|t| !self.cfg.skip_ai || !self.is_ai(t)).collect();
        let ai = n - kept.len();
        (kept, ai)
    }

    fn note_skipped(&mut self, n: usize) {
        if n > 0 {
            self.status = format!("Skipped {n} AI-generated song{}", if n == 1 { "" } else { "s" });
        }
    }

    fn play_index(&mut self, i: usize) {
        let Some(url) = self.queue.get(i).map(Track::url) else { return };
        self.finish_track();
        self.mpv_cmd(json!(["loadfile", url, "replace"]));
        self.mpv_set("pause", false);
        self.mpv_next = None;
        self.cur = Some(i);
        self.queue_sel.select(Some(i));
        self.on_track_start();
        self.sync_next();
    }

    /// Keep mpv's playlist as [current, next] so gapless + prefetch work.
    fn sync_next(&mut self) {
        let Some(c) = self.cur else { return };
        let want = self.next_playable(c + 1).map(|i| self.queue[i].url());
        if want == self.mpv_next {
            return;
        }
        self.mpv_cmd(json!(["playlist-clear"]));
        if let Some(u) = &want {
            self.mpv_cmd(json!(["loadfile", u, "append"]));
        }
        self.mpv_next = want;
    }

    fn on_track_start(&mut self) {
        self.pos = 0.0;
        self.dur = 0.0;
        self.played = 0.0;
        self.last_pos = 0.0;
        self.started_at = now();
        self.finished = false;
        let Some(t) = self.current().cloned() else { return };

        self.lyrics = None;
        self.lyrics_loading = true;
        self.lyrics_key = t.url();
        let key = self.lyrics_key.clone();
        let lt = t.clone();
        self.spawn(move || Msg::Lyrics(key, lyrics::fetch(&lt)));

        if self.lfm_ready() {
            let (k, s, sk) = self.lfm_creds();
            let nt = t.clone();
            self.spawn(move || match lastfm::now_playing(&k, &s, &sk, &nt) {
                Ok(()) => Msg::Status(String::new()),
                Err(e) => Msg::Status(e),
            });
        }
        self.maybe_radio();
    }

    fn maybe_radio(&mut self) {
        let Some(c) = self.cur else { return };
        if !self.cfg.radio || self.radio_busy || c + 2 < self.queue.len() {
            return;
        }
        let Some(id) = self.queue[c].video_id().map(str::to_string) else { return };
        self.radio_busy = true;
        self.spawn(move || Msg::Radio(ytm::radio(&id)));
    }

    /// Scrobble rule: >30s track, played half or 4 minutes.
    fn finish_track(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(t) = self.current().cloned() else { return };
        let dur = if self.dur > 0.0 { self.dur } else { t.duration as f64 };
        if !self.lfm_ready() || dur < 30.0 || (self.played < dur / 2.0 && self.played < 240.0) {
            return;
        }
        let mut t = t;
        if t.duration == 0 {
            t.duration = dur as u32;
        }
        let (k, s, sk) = self.lfm_creds();
        let ts = self.started_at;
        self.spawn(move || match lastfm::scrobble(&k, &s, &sk, &t, ts) {
            Ok(()) => Msg::Status(format!("Scrobbled: {} — {}", t.artist, t.title)),
            Err(e) => Msg::Status(e),
        });
    }

    fn lfm_ready(&self) -> bool {
        !self.cfg.lastfm_session.is_empty() && !self.cfg.lastfm_api_key.is_empty()
    }

    fn lfm_creds(&self) -> (String, String, String) {
        (self.cfg.lastfm_api_key.clone(), self.cfg.lastfm_api_secret.clone(), self.cfg.lastfm_session.clone())
    }

    pub fn play_now(&mut self, t: Track) {
        let i = match self.cur {
            Some(c) => {
                self.queue.insert(c + 1, t);
                c + 1
            }
            None => {
                self.queue.push(t);
                self.queue.len() - 1
            }
        };
        self.play_index(i);
    }

    pub fn play_list(&mut self, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.finish_track();
        self.queue = tracks;
        self.cur = None;
        self.play_index(start);
    }

    /// Insert right after the current track (or at the end when idle).
    pub fn play_next(&mut self, tracks: Vec<Track>) {
        let (tracks, ai) = self.drop_ai(tracks);
        let n = tracks.len();
        let at = self.cur.map_or(self.queue.len(), |c| c + 1);
        self.queue.splice(at..at, tracks);
        self.sync_next();
        self.status = format!("{n} track{} will play next{}", if n == 1 { "" } else { "s" }, ai_left_out(ai));
    }

    pub fn append(&mut self, tracks: Vec<Track>) {
        let (tracks, ai) = self.drop_ai(tracks);
        let n = tracks.len();
        self.queue.extend(tracks);
        self.sync_next();
        self.status = format!("Added {n} track{} to queue{}", if n == 1 { "" } else { "s" }, ai_left_out(ai));
    }

    fn start_radio(&mut self, t: Track) {
        self.play_list(vec![t], 0);
        self.radio_busy = false;
        if let Some(id) = self.queue[0].video_id().map(str::to_string) {
            self.radio_busy = true;
            self.status = "Starting radio…".into();
            self.spawn(move || Msg::Radio(ytm::radio(&id)));
        }
    }

    /// Radio from the playing song without restarting it: what's queued
    /// after it is replaced by the radio.
    fn radio_from_current(&mut self) {
        let Some(c) = self.cur else {
            self.status = "Nothing is playing".into();
            return;
        };
        let Some(id) = self.queue[c].video_id().map(str::to_string) else {
            self.status = "Radio needs a YouTube song".into();
            return;
        };
        self.queue.truncate(c + 1);
        self.sync_next();
        self.radio_busy = true;
        self.status = "Starting radio…".into();
        self.spawn(move || Msg::Radio(ytm::radio(&id)));
    }

    fn next(&mut self) {
        let from = self.cur.map_or(0, |c| c + 1);
        match self.next_playable(from) {
            Some(i) => {
                self.play_index(i);
                self.note_skipped(i - from);
            }
            None => self.status = "End of queue".into(),
        }
    }

    /// Flag (or unflag) the playing song's artist as AI-generated.
    /// Drop AI tracks (and shelves left empty) from Discover when hiding is on.
    fn hide_ai_discover(&mut self) {
        if !self.cfg.hide_ai {
            return;
        }
        let sel = self.discover_sel.selected().and_then(|i| self.discover.get(i)).map(|d| match d {
            DItem::Track(t) => t.url(),
            DItem::Coll(c) => c.browse_id.clone(),
            DItem::Header(h) => h.clone(),
        });
        let items = std::mem::take(&mut self.discover);
        for d in items {
            if matches!(&d, DItem::Track(t) if self.is_ai(t)) || matches!(&d, DItem::Coll(c) if self.is_ai_coll(c)) {
                continue;
            }
            if matches!(d, DItem::Header(_)) && matches!(self.discover.last(), Some(DItem::Header(_))) {
                self.discover.pop();
            }
            self.discover.push(d);
        }
        if matches!(self.discover.last(), Some(DItem::Header(_))) {
            self.discover.pop();
        }
        let i = sel.and_then(|k| {
            self.discover.iter().position(|d| match d {
                DItem::Track(t) => t.url() == k,
                DItem::Coll(c) => c.browse_id == k,
                DItem::Header(h) => *h == k,
            })
        });
        self.discover_sel.select(i.or((self.discover.len() > 1).then_some(1)));
    }

    fn toggle_ai_flag(&mut self) {
        let Some(t) = self.current().cloned() else { return };
        if self.is_ai(&t) {
            aifilter::unflag(&t.artist, &mut self.cfg.ai_flagged, &mut self.cfg.ai_allowed, aifilter::listed);
            self.cfg.save();
            self.status = format!("{} is no longer treated as AI", t.artist);
            self.sync_next();
            return;
        }
        let name = aifilter::flag(&t.artist, &mut self.cfg.ai_flagged, &mut self.cfg.ai_allowed, aifilter::listed);
        self.cfg.save();
        self.hide_ai_discover();
        if !self.cfg.skip_ai {
            self.status = format!("Flagged {name} — turn on Skip AI songs in Settings to skip them");
            return;
        }
        let removed = self.drop_ai_from_queue();
        self.next();
        if self.status != "End of queue" {
            self.status = format!("Flagged {name} as AI-generated");
        }
        if removed > 0 {
            self.status += &format!("; removed {removed} more AI song{} from the queue", if removed == 1 { "" } else { "s" });
        }
    }

    /// Remove AI tracks from the queue, except the current one.
    fn drop_ai_from_queue(&mut self) -> usize {
        let before = self.queue.len();
        let cur = self.cur;
        let keep: Vec<bool> = self.queue.iter().enumerate().map(|(i, t)| Some(i) == cur || !self.is_ai(t)).collect();
        let mut new_cur = None;
        let mut q = Vec::with_capacity(before);
        for (i, t) in std::mem::take(&mut self.queue).into_iter().enumerate() {
            if keep[i] {
                if Some(i) == cur {
                    new_cur = Some(q.len());
                }
                q.push(t);
            }
        }
        self.queue = q;
        self.cur = new_cur;
        nav(&mut self.queue_sel, self.queue.len(), 0);
        before - self.queue.len()
    }

    fn prev(&mut self) {
        match self.cur.and_then(|c| self.prev_playable(c)) {
            Some(i) if self.pos <= 3.0 => self.play_index(i),
            _ => self.mpv_cmd(json!(["seek", 0, "absolute"])),
        }
    }

    fn toggle_pause(&mut self) {
        if self.idle {
            let from = self.cur.unwrap_or(0);
            match self.next_playable(from) {
                Some(i) => {
                    self.play_index(i);
                    self.note_skipped(i - from);
                }
                None if !self.queue.is_empty() => self.status = "Nothing left to play (the rest is AI-generated)".into(),
                None => {}
            }
        } else {
            self.mpv_cmd(json!(["cycle", "pause"]));
        }
    }

    fn remove_from_queue(&mut self, i: usize) {
        if i >= self.queue.len() {
            return;
        }
        let was_current = self.cur == Some(i);
        self.queue.remove(i);
        match self.cur {
            Some(c) if c > i => self.cur = Some(c - 1),
            _ => {}
        }
        if was_current {
            self.finish_track();
            match self.next_playable(i) {
                Some(n) => {
                    self.play_index(n);
                    self.note_skipped(n - i);
                }
                None => {
                    self.cur = None;
                    self.mpv_cmd(json!(["stop"]));
                }
            }
        } else {
            self.sync_next();
        }
        nav(&mut self.queue_sel, self.queue.len(), 0);
    }

    fn move_in_queue(&mut self, i: usize, up: bool) {
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|&j| j < self.queue.len()) };
        let Some(j) = j else { return };
        self.queue.swap(i, j);
        if self.cur == Some(i) {
            self.cur = Some(j);
        } else if self.cur == Some(j) {
            self.cur = Some(i);
        }
        self.queue_sel.select(Some(j));
        self.sync_next();
    }

    // ---- audio settings --------------------------------------------------

    /// Push output-chain settings into the running mpv.
    fn apply_audio(&mut self, reload_ao: bool) {
        let Some(m) = &self.mpv else { return };
        m.set("audio-device", self.cfg.audio_device.clone());
        m.set("audio-exclusive", self.cfg.exclusive);
        m.set("gapless-audio", if self.cfg.gapless { "yes" } else { "no" });
        m.set("ytdl-format", self.cfg.ytdl_format.clone());
        if self.cfg.exclusive {
            m.set("replaygain", "no");
            m.set("af", "");
        } else {
            m.set("replaygain", self.cfg.replaygain.clone());
            m.set("af", self.cfg.eq.filter());
        }
        if reload_ao {
            m.cmd(json!(["ao-reload"]));
        }
        self.cfg.save();
    }

    fn apply_eq(&mut self) {
        if self.cfg.exclusive && self.cfg.eq.active() {
            self.status = "EQ is bypassed while bit-perfect mode is on".into();
        } else {
            self.mpv_set("af", self.cfg.eq.filter());
        }
        self.cfg.save();
    }

    pub fn bit_perfect(&self) -> bool {
        let rate = |v: &Value| v["samplerate"].as_u64();
        self.cfg.exclusive
            && self.af.is_empty()
            && (self.volume - 100.0).abs() < 0.01
            && rate(&self.params).is_some()
            && rate(&self.params) == rate(&self.out_params)
    }

    // ---- library / search -------------------------------------------------

    fn rescan(&mut self) {
        let dir = PathBuf::from(&self.cfg.library_dir);
        if self.cfg.library_dir.is_empty() || !dir.is_dir() {
            return;
        }
        self.lib_scanning = true;
        self.spawn(move || Msg::Library(library::scan(&dir)));
    }

    pub fn lib_visible(&self) -> Vec<usize> {
        let f = self.lib_filter.to_lowercase();
        let words: Vec<&str> = f.split_whitespace().collect();
        (0..self.library.len())
            .filter(|&i| {
                let t = &self.library[i];
                let hay = format!("{} {} {}", t.title, t.artist, t.album).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect()
    }

    fn run_search(&mut self) {
        if self.query.trim().is_empty() {
            return;
        }
        self.search_seq += 1;
        self.searching = true;
        self.browse = None;
        let (seq, q, kind) = (self.search_seq, self.query.clone(), self.kind);
        self.spawn(move || Msg::Search(seq, ytm::search(&q, kind)));
    }

    fn selected_search_track(&self) -> Option<(Vec<Track>, usize)> {
        if let Some((_, tracks)) = &self.browse {
            let i = self.browse_sel.selected()?;
            return (i < tracks.len()).then(|| (tracks.clone(), i));
        }
        if self.discover_mode() {
            return match self.discover_sel.selected().and_then(|i| self.discover.get(i)) {
                Some(DItem::Track(t)) => Some((vec![t.clone()], 0)),
                _ => None,
            };
        }
        if let Some(Results::Tracks(t)) = &self.results {
            let i = self.search_sel.selected()?;
            return (i < t.len()).then(|| (t.clone(), i));
        }
        None
    }

    fn selected_search_collection(&self) -> Option<Collection> {
        if self.browse.is_some() {
            return None;
        }
        if self.discover_mode() {
            return match self.discover_sel.selected().and_then(|i| self.discover.get(i)) {
                Some(DItem::Coll(c)) => Some(c.clone()),
                _ => None,
            };
        }
        match &self.results {
            Some(Results::Collections(cs)) => self.search_sel.selected().and_then(|i| cs.get(i)).cloned(),
            _ => None,
        }
    }

    /// Discover rows include shelf headers; never leave the cursor on one.
    fn skip_headers(&mut self, before: usize) {
        let Some(mut i) = self.discover_sel.selected() else { return };
        let len = self.discover.len() as isize;
        let mut dir: isize = if i >= before { 1 } else { -1 };
        let mut flipped = false;
        while matches!(self.discover.get(i), Some(DItem::Header(_))) {
            let n = i as isize + dir;
            if n < 0 || n >= len {
                if flipped {
                    break;
                }
                flipped = true;
                dir = -dir;
                continue;
            }
            i = n as usize;
        }
        self.discover_sel.select(Some(i));
    }

    // ---- events ---------------------------------------------------------------

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Mpv(ev) => self.on_mpv(ev),
            Msg::Search(seq, r) if seq == self.search_seq => {
                self.searching = false;
                match r {
                    Ok(r) => {
                        let n = match &r {
                            Results::Tracks(t) => t.len(),
                            Results::Collections(c) => c.len(),
                        };
                        self.status = format!("{n} results");
                        self.search_sel.select((n > 0).then_some(0));
                        self.results = Some(r);
                    }
                    Err(e) => self.status = format!("Search failed: {e}"),
                }
            }
            Msg::Search(..) => {}
            Msg::Browse(lib, c, r) => {
                self.searching = false;
                self.yt_loading = false;
                match r {
                    Ok(mut t) => {
                        ytm::fill_from_collection(&mut t, &c);
                        self.status = format!("{}: {} tracks", c.title, t.len());
                        let first = (!t.is_empty()).then_some(0);
                        if lib {
                            // A refresh of what's already on screen keeps the cursor.
                            let same = self.yt_open.as_ref().is_some_and(|(o, _)| o.browse_id == c.browse_id);
                            if !same || self.yt_open_sel.selected().is_none_or(|i| i >= t.len()) {
                                self.yt_open_sel.select(first);
                            }
                            if self.cache.pages.len() >= MAX_CACHED_PAGES && !self.cache.pages.contains_key(&c.browse_id) {
                                if let Some(k) = self.cache.pages.keys().next().cloned() {
                                    self.cache.pages.remove(&k);
                                }
                            }
                            self.cache.pages.insert(c.browse_id.clone(), t.clone());
                            // Only replace the view if the user is still on it (or on the list).
                            if same || self.yt_open.is_none() {
                                self.yt_open = Some((c, t));
                            }
                            self.save_cache();
                        } else {
                            self.browse_sel.select(first);
                            self.browse = Some((c, t));
                        }
                    }
                    Err(e) => self.status = format!("Could not open {}: {e}", c.title),
                }
            }
            Msg::Home(r) => {
                self.discover_loading = false;
                match r {
                    Ok(shelves) => {
                        self.discover.clear();
                        for sh in shelves {
                            self.discover.push(DItem::Header(sh.title));
                            self.discover.extend(sh.items.into_iter().map(|e| match e {
                                Entry::Track(t) => DItem::Track(t),
                                Entry::Collection(c) => DItem::Coll(c),
                            }));
                        }
                        self.hide_ai_discover();
                        let keep = self.discover_sel.selected().filter(|&i| matches!(self.discover.get(i), Some(DItem::Track(_) | DItem::Coll(_))));
                        self.discover_sel.select(keep.or((self.discover.len() > 1).then_some(1)));
                        self.save_cache();
                    }
                    Err(e) => self.status = format!("Discover failed: {e}"),
                }
            }
            Msg::YtLogin(r) => match r {
                Ok((cookies, name)) => {
                    config::save_cookies(Some(&cookies));
                    self.status = format!("Signed in to YouTube Music as {name}");
                    self.cfg.yt_user = name;
                    self.cfg.save();
                    self.mpv_set("ytdl-raw-options", config::ytdl_raw_options(&self.cfg));
                    self.load_home();
                    self.load_yt_lists();
                    self.load_liked();
                }
                Err(e) => self.status = e,
            },
            Msg::YtLists(r) => {
                self.yt_loading = false;
                match r {
                    Ok(l) => {
                        if self.yt_lists_sel.selected().is_none_or(|i| i >= l.len()) {
                            self.yt_lists_sel.select((!l.is_empty()).then_some(0));
                        }
                        self.yt_lists = l;
                        self.save_cache();
                    }
                    Err(e) => self.status = format!("Could not load playlists: {e}"),
                }
            }
            Msg::Radio(r) => {
                self.radio_busy = false;
                match r {
                    Ok(tracks) => {
                        let have: HashSet<String> = self.queue.iter().map(Track::url).collect();
                        let fresh: Vec<Track> = tracks.into_iter().filter(|t| !have.contains(&t.url())).collect();
                        let total = fresh.len();
                        let fresh: Vec<Track> = fresh.into_iter().filter(|t| !self.cfg.skip_ai || !self.is_ai(t)).collect();
                        let n = fresh.len();
                        self.queue.extend(fresh);
                        self.sync_next();
                        self.status = match total - n {
                            0 => format!("Radio: +{n} tracks"),
                            ai => format!("Radio: +{n} tracks ({ai} AI-generated left out)"),
                        };
                    }
                    Err(e) => self.status = format!("Radio failed: {e}"),
                }
            }
            Msg::Lyrics(key, l) => {
                if key == self.lyrics_key {
                    self.lyrics_loading = false;
                    self.lyrics = l;
                }
            }
            Msg::Library(l) => {
                self.lib_scanning = false;
                self.status = format!("Library: {} tracks", l.len());
                self.library = l;
                nav(&mut self.lib_sel, self.library.len(), 0);
            }
            Msg::Status(s) => {
                if !s.is_empty() {
                    self.status = s;
                }
            }
            Msg::LfmToken(r) => match r {
                Ok(tok) => {
                    lastfm::open_url(&lastfm::auth_url(&self.cfg.lastfm_api_key, &tok));
                    self.lfm_token = Some(tok);
                    self.status = "Approve LastWave in your browser, then press Enter on Last.fm account again".into();
                }
                Err(e) => self.status = e,
            },
            Msg::Art(key, img) => {
                if let Some(c) = &mut self.covers {
                    c.loaded(key, img);
                }
            }
            Msg::AiList(r) => match r {
                Ok(n) => {
                    if self.view == View::Settings {
                        self.status = format!("AI artist list updated: {n} artists");
                    }
                    self.hide_ai_discover();
                    self.sync_next();
                }
                Err(e) => self.status = e,
            },
            Msg::Liked(r) => {
                if let Ok(l) = r {
                    self.liked = l;
                    self.save_cache();
                }
            }
            Msg::LikeResult(id, liked, r) => match r {
                // Liked Music changed: refetch it next time it's opened.
                Ok(()) => {
                    self.cache.pages.remove("VLLM");
                    self.save_cache();
                }
                Err(e) => {
                    if liked {
                        self.liked.remove(&id);
                    } else {
                        self.liked.insert(id);
                    }
                    self.status = format!("Could not update like: {e}");
                }
            },
            Msg::AddTargets(id, r) => {
                let Some(p) = self.picker.as_mut().filter(|p| p.track.video_id() == Some(id.as_str())) else { return };
                match r {
                    Ok(items) => {
                        p.items = items;
                        p.loading = false;
                    }
                    Err(e) => {
                        self.picker = None;
                        self.status = format!("Could not load playlists: {e}");
                    }
                }
            }
            Msg::Added(title, r) => match r {
                Ok(bid) => {
                    self.status = format!("Added to {title}");
                    self.cache.pages.remove(&bid);
                    if let Some(c) = self.yt_open.as_ref().map(|(c, _)| c.clone()).filter(|c| c.browse_id == bid) {
                        self.open_collection(c, true);
                    }
                    self.save_cache();
                    self.load_yt_lists();
                }
                Err(e) => self.status = format!("Could not add to {title}: {e}"),
            },
            Msg::LfmSession(r) => match r {
                Ok((sk, name)) => {
                    self.cfg.lastfm_session = sk;
                    self.status = format!("Last.fm connected as {name}");
                    self.cfg.lastfm_user = name;
                    self.lfm_token = None;
                    self.cfg.save();
                }
                Err(e) => self.status = e,
            },
        }
    }

    fn on_mpv(&mut self, ev: MpvEvent) {
        match ev {
            MpvEvent::Prop(name, v) => match name.as_str() {
                "time-pos" => {
                    let p = v.as_f64().unwrap_or(0.0);
                    let d = p - self.last_pos;
                    if !self.paused && d > 0.0 && d < 2.0 {
                        self.played += d;
                    }
                    self.last_pos = p;
                    self.pos = p;
                }
                "duration" => self.dur = v.as_f64().unwrap_or(0.0),
                "pause" => self.paused = v.as_bool().unwrap_or(false),
                "volume" => self.volume = v.as_f64().unwrap_or(self.volume),
                "idle-active" => self.idle = v.as_bool().unwrap_or(false),
                "audio-codec-name" => self.codec = v.as_str().unwrap_or_default().to_string(),
                "audio-params" => self.params = v,
                "audio-out-params" => self.out_params = v,
                "audio-bitrate" => self.bitrate = v.as_f64().unwrap_or(0.0),
                "current-ao" => self.ao = v.as_str().unwrap_or_default().to_string(),
                "af" => self.af = v.as_array().map(|a| if a.is_empty() { "" } else { "on" }).unwrap_or("").into(),
                "path" => {
                    let Some(p) = v.as_str() else { return };
                    let Some(c) = self.cur else { return };
                    if self.queue.get(c).is_some_and(|t| t.url() == p) {
                        return;
                    }
                    // mpv moved on to the prefetched next entry (gapless advance).
                    if let Some(n) = self.next_playable(c + 1).filter(|&n| self.queue[n].url() == p) {
                        self.finish_track();
                        self.note_skipped(n - c - 1);
                        self.cur = Some(n);
                        self.queue_sel.select(self.cur);
                        self.mpv_cmd(json!(["playlist-remove", 0]));
                        self.mpv_next = None;
                        self.on_track_start();
                        self.sync_next();
                    }
                }
                _ => {}
            },
            MpvEvent::EndFile(reason) => match reason.as_str() {
                "error" => {
                    let what = self.current().map(|t| t.title.clone()).unwrap_or_default();
                    self.status = format!("Could not play \"{what}\" (is yt-dlp installed and up to date?)");
                }
                "eof" if self.mpv_next.is_none() => self.finish_track(),
                _ => {}
            },
            MpvEvent::Exited => {
                self.mpv = None;
                self.idle = true;
                self.status = "mpv exited — restart LastWave TUI".into();
            }
        }
    }

    // ---- keys -------------------------------------------------------------------

    pub fn on_key(&mut self, k: KeyEvent) {
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
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(c @ '1'..='6') => self.view = View::ALL[(c as u8 - b'1') as usize],
            KeyCode::Tab => self.cycle_view(1),
            KeyCode::BackTab => self.cycle_view(View::ALL.len() - 1),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('n') => self.next(),
            KeyCode::Char('p') => self.prev(),
            KeyCode::Char(',') => self.mpv_cmd(json!(["seek", -10, "relative"])),
            KeyCode::Char('.') => self.mpv_cmd(json!(["seek", 10, "relative"])),
            KeyCode::Left if self.view != View::Eq && self.view != View::Settings => {
                self.mpv_cmd(json!(["seek", -5, "relative"]))
            }
            KeyCode::Right if self.view != View::Eq && self.view != View::Settings => {
                self.mpv_cmd(json!(["seek", 5, "relative"]))
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.mpv_cmd(json!(["add", "volume", 5])),
            KeyCode::Char('-') => self.mpv_cmd(json!(["add", "volume", -5])),
            KeyCode::Char('/') => {
                self.view = View::Search;
                self.begin_input(Input::Search, self.query.clone());
            }
            KeyCode::Char('v') => {
                self.cfg.show_details = !self.cfg.show_details;
                self.cfg.save();
            }
            KeyCode::Char('R') => {
                self.cfg.radio = !self.cfg.radio;
                self.status = format!("Radio autoplay {}", if self.cfg.radio { "on" } else { "off" });
                self.cfg.save();
                self.maybe_radio();
            }
            KeyCode::Char('B') => self.toggle_ai_flag(),
            KeyCode::Char('l') if !matches!(self.view, View::Eq | View::Settings) => self.toggle_like(),
            KeyCode::Char('P') => self.open_picker(),
            KeyCode::Char('w') => self.radio_from_current(),
            code => match self.view {
                View::Search => self.key_search(code),
                View::Queue => self.key_queue(code),
                View::Library => self.key_library(code),
                View::Lyrics => {}
                View::Eq => self.key_eq(code),
                View::Settings => self.key_settings(code),
            },
        }
    }

    fn cycle_view(&mut self, by: usize) {
        let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
        self.view = View::ALL[(i + by) % View::ALL.len()];
    }

    fn begin_input(&mut self, target: Input, initial: String) {
        self.input = Some(target);
        self.input_buf = initial;
    }

    fn on_input(&mut self, target: Input, k: KeyEvent) {
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
                        self.lib_filter = v;
                        self.lib_sel.select(Some(0));
                    }
                    Input::LibDir => {
                        self.cfg.library_dir = v;
                        self.cfg.save();
                        self.rescan();
                    }
                    Input::LfmKey => {
                        self.cfg.lastfm_api_key = v;
                        self.cfg.save();
                    }
                    Input::LfmSecret => {
                        self.cfg.lastfm_api_secret = v;
                        self.cfg.save();
                    }
                    // Nothing typed: read the system clipboard instead.
                    Input::YtCookies => self.submit_yt_cookies(Some(v).filter(|v| !v.is_empty())),
                    Input::NewPlaylist => self.create_playlist(v),
                }
            }
            KeyCode::Backspace => {
                self.input_buf.pop();
                if target == Input::LibFilter {
                    self.lib_filter = self.input_buf.clone();
                }
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => self.input_buf.clear(),
            KeyCode::Char(c) => {
                self.input_buf.push(c);
                if target == Input::LibFilter {
                    self.lib_filter = self.input_buf.clone();
                    self.lib_sel.select(Some(0));
                }
            }
            _ => {}
        }
    }

    fn key_search(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('i') => self.begin_input(Input::Search, self.query.clone()),
            KeyCode::Char('f') => {
                self.kind = self.kind.next();
                self.run_search();
            }
            KeyCode::Esc | KeyCode::Backspace if self.browse.is_some() => self.browse = None,
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.results = None;
            }
            KeyCode::Char('S') if self.discover_mode() => self.load_home(),
            KeyCode::Enter => {
                if let Some((tracks, i)) = self.selected_search_track() {
                    if self.browse.is_some() {
                        self.play_list(tracks, i);
                    } else {
                        self.play_now(tracks[i].clone());
                    }
                } else if let Some(c) = self.selected_search_collection() {
                    self.open_collection(c, false);
                }
            }
            KeyCode::Char('a') => {
                if let Some((tracks, i)) = self.selected_search_track() {
                    self.append(vec![tracks[i].clone()]);
                }
            }
            KeyCode::Char('N') => {
                if let Some((tracks, i)) = self.selected_search_track() {
                    self.play_next(vec![tracks[i].clone()]);
                }
            }
            KeyCode::Char('A') => {
                if let Some((_, t)) = &self.browse {
                    let t = t.clone();
                    self.append(t);
                } else if let (false, Some(Results::Tracks(t))) = (self.discover_mode(), &self.results) {
                    let t = t.clone();
                    self.append(t);
                }
            }
            KeyCode::Char('r') => {
                if let Some((tracks, i)) = self.selected_search_track() {
                    self.start_radio(tracks[i].clone());
                }
            }
            code => {
                if let Some((_, t)) = &self.browse {
                    let len = t.len();
                    list_nav(&mut self.browse_sel, len, code);
                } else if self.discover_mode() {
                    let before = self.discover_sel.selected().unwrap_or(0);
                    if list_nav(&mut self.discover_sel, self.discover.len(), code) {
                        self.skip_headers(before);
                    }
                } else {
                    let len = match &self.results {
                        Some(Results::Tracks(t)) => t.len(),
                        Some(Results::Collections(c)) => c.len(),
                        None => 0,
                    };
                    list_nav(&mut self.search_sel, len, code);
                }
            }
        }
    }

    fn key_queue(&mut self, code: KeyCode) {
        let sel = self.queue_sel.selected();
        match code {
            KeyCode::Enter => {
                if let Some(i) = sel {
                    self.play_index(i);
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(i) = sel {
                    self.remove_from_queue(i);
                }
            }
            KeyCode::Char('N') => {
                if let (Some(i), Some(c)) = (sel, self.cur) {
                    if i != c {
                        let t = self.queue.remove(i);
                        let c = if i < c { c - 1 } else { c };
                        self.cur = Some(c);
                        self.queue.insert(c + 1, t);
                        self.queue_sel.select(Some(c + 1));
                        self.sync_next();
                        self.status = "Moved to play next".into();
                    }
                }
            }
            KeyCode::Char('K') => {
                if let Some(i) = sel {
                    self.move_in_queue(i, true);
                }
            }
            KeyCode::Char('J') => {
                if let Some(i) = sel {
                    self.move_in_queue(i, false);
                }
            }
            KeyCode::Char('c') => self.queue_sel.select(self.cur),
            KeyCode::Char('X') => {
                match self.cur.and_then(|c| self.queue.get(c).cloned()) {
                    Some(t) => {
                        self.queue = vec![t];
                        self.cur = Some(0);
                    }
                    None => self.queue.clear(),
                }
                self.queue_sel.select(self.cur);
                self.sync_next();
                self.status = "Queue cleared".into();
            }
            code => {
                list_nav(&mut self.queue_sel, self.queue.len(), code);
            }
        }
    }

    fn key_library(&mut self, code: KeyCode) {
        let mode = match code {
            KeyCode::Char('y') => Some(if self.lib_mode == LibMode::Local { LibMode::Yt } else { LibMode::Local }),
            KeyCode::F(11) => Some(LibMode::Local),
            KeyCode::F(12) => Some(LibMode::Yt),
            _ => None,
        };
        if let Some(m) = mode {
            self.lib_mode = m;
            if m == LibMode::Yt && self.yt_lists.is_empty() && !self.yt_loading {
                self.load_yt_lists();
            }
            return;
        }
        if self.lib_mode == LibMode::Yt {
            self.key_library_yt(code);
            return;
        }
        let vis = self.lib_visible();
        let sel = self.lib_sel.selected().filter(|&i| i < vis.len());
        match code {
            KeyCode::Char('f') | KeyCode::Char('i') => self.begin_input(Input::LibFilter, self.lib_filter.clone()),
            KeyCode::Esc => self.lib_filter.clear(),
            KeyCode::Char('o') => self.begin_input(Input::LibDir, self.cfg.library_dir.clone()),
            KeyCode::Char('S') => self.rescan(),
            KeyCode::Enter => {
                if let Some(i) = sel {
                    let tracks = vis.iter().map(|&j| self.library[j].clone()).collect();
                    self.play_list(tracks, i);
                }
            }
            KeyCode::Char('a') => {
                if let Some(i) = sel {
                    self.append(vec![self.library[vis[i]].clone()]);
                }
            }
            KeyCode::Char('N') => {
                if let Some(i) = sel {
                    self.play_next(vec![self.library[vis[i]].clone()]);
                }
            }
            KeyCode::Char('A') => {
                let tracks = vis.iter().map(|&j| self.library[j].clone()).collect();
                self.append(tracks);
            }
            code => {
                list_nav(&mut self.lib_sel, vis.len(), code);
            }
        }
    }

    fn key_library_yt(&mut self, code: KeyCode) {
        let open_sel = self.yt_open.as_ref().and_then(|(_, t)| self.yt_open_sel.selected().filter(|&i| i < t.len()));
        let open_track = |app: &Self| open_sel.and_then(|i| app.yt_open.as_ref().map(|(_, t)| t[i].clone()));
        match code {
            KeyCode::Esc | KeyCode::Backspace if self.yt_open.is_some() => self.yt_open = None,
            KeyCode::Char('S') => self.load_yt_lists(),
            KeyCode::Char('L') if !ytm::signed_in() => self.start_yt_login(),
            KeyCode::Enter => {
                if let (Some((_, t)), Some(i)) = (&self.yt_open, open_sel) {
                    let t = t.clone();
                    self.play_list(t, i);
                } else if self.yt_open.is_none() {
                    if let Some(c) = self.yt_lists_sel.selected().and_then(|i| self.yt_lists.get(i)).cloned() {
                        self.open_collection(c, true);
                    }
                }
            }
            KeyCode::Char('a') => {
                if let Some(t) = open_track(self) {
                    self.append(vec![t]);
                }
            }
            KeyCode::Char('N') => {
                if let Some(t) = open_track(self) {
                    self.play_next(vec![t]);
                }
            }
            KeyCode::Char('r') => {
                if let Some(t) = open_track(self) {
                    self.start_radio(t);
                }
            }
            KeyCode::Char('A') => {
                if let Some((_, t)) = &self.yt_open {
                    let t = t.clone();
                    self.append(t);
                }
            }
            code => {
                if let Some((_, t)) = &self.yt_open {
                    let len = t.len();
                    list_nav(&mut self.yt_open_sel, len, code);
                } else {
                    list_nav(&mut self.yt_lists_sel, self.yt_lists.len(), code);
                }
            }
        }
    }

    fn key_eq(&mut self, code: KeyCode) {
        let b = self.eq_band;
        match code {
            KeyCode::Left | KeyCode::Char('h') => self.eq_band = b.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => self.eq_band = (b + 1).min(eq::BANDS.len() - 1),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                let d = if matches!(code, KeyCode::Up | KeyCode::Char('k')) { 0.5 } else { -0.5 };
                let g = &mut self.cfg.eq.gains[b];
                *g = (*g + d).clamp(-eq::MAX_GAIN, eq::MAX_GAIN);
                self.cfg.eq.enabled = true;
                self.cfg.eq.preset = "Custom".into();
                self.apply_eq();
            }
            KeyCode::Char('e') => {
                self.cfg.eq.enabled = !self.cfg.eq.enabled;
                self.apply_eq();
            }
            KeyCode::Char(']') | KeyCode::Char('[') => {
                let n = eq::PRESETS.len();
                let i = self.cfg.eq.preset_index();
                let next = if code == KeyCode::Char(']') { (i + 1) % n } else { (i + n - 1) % n };
                self.cfg.eq.apply_preset(next);
                self.apply_eq();
            }
            KeyCode::Char('c') => {
                self.cfg.eq.clarity = !self.cfg.eq.clarity;
                self.apply_eq();
            }
            KeyCode::Char('C') => {
                self.cfg.eq.clarity_preset = (self.cfg.eq.clarity_preset + 1) % eq::CLARITY.len();
                self.cfg.eq.clarity = true;
                self.apply_eq();
            }
            KeyCode::Char('z') => {
                self.cfg.eq.apply_preset(0);
                self.apply_eq();
            }
            _ => {}
        }
    }

    pub fn setting_value(&self, i: usize) -> String {
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        match i {
            0 => self
                .devices
                .iter()
                .find(|(n, _)| *n == self.cfg.audio_device)
                .map(|(n, d)| if d.is_empty() { n.clone() } else { d.clone() })
                .unwrap_or_else(|| self.cfg.audio_device.clone()),
            1 => on(self.cfg.exclusive),
            2 => self.cfg.replaygain.clone(),
            3 => on(self.cfg.gapless),
            4 => YTDL_FORMATS
                .iter()
                .find(|(f, _)| *f == self.cfg.ytdl_format)
                .map(|(_, l)| l.to_string())
                .unwrap_or_else(|| self.cfg.ytdl_format.clone()),
            5 => on(self.cfg.radio),
            6 => self.cfg.library_dir.clone(),
            7 => mask(&self.cfg.lastfm_api_key),
            8 => mask(&self.cfg.lastfm_api_secret),
            10 => {
                if ytm::signed_in() {
                    format!("signed in as {} (Enter to sign out)", self.cfg.yt_user)
                } else {
                    "not signed in (copy cookies, then Enter twice)".into()
                }
            }
            11 => on(self.cfg.yt_account_playback),
            12 => {
                let using = self.covers.as_ref().map(|c| c.mode_name()).unwrap_or_default();
                if self.cfg.image_protocol == "auto" {
                    format!("auto (using {using})")
                } else {
                    self.cfg.image_protocol.clone()
                }
            }
            13 => {
                let n = aifilter::count();
                let listed = if n > 0 { format!("{n} known AI artists") } else { "list not downloaded yet".into() };
                let mine = self.cfg.ai_flagged.len();
                let mine = if mine > 0 { format!(", {mine} flagged by you") } else { String::new() };
                format!("{} ({listed}{mine}; B flags the playing artist)", on(self.cfg.skip_ai))
            }
            14 => format!("{} (Quick picks and other Discover shelves)", on(self.cfg.hide_ai)),
            9 => {
                if !self.cfg.lastfm_user.is_empty() && self.lfm_ready() {
                    format!("connected as {} (Enter to disconnect)", self.cfg.lastfm_user)
                } else if self.lfm_token.is_some() {
                    "waiting for browser approval — press Enter".into()
                } else {
                    "not connected (Enter to connect)".into()
                }
            }
            _ => String::new(),
        }
    }

    fn key_settings(&mut self, code: KeyCode) {
        let back = matches!(code, KeyCode::Left | KeyCode::Char('h'));
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.set_sel = self.set_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.set_sel = (self.set_sel + 1).min(SETTINGS.len() - 1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Left | KeyCode::Char('l') | KeyCode::Char('h') => {
                self.change_setting(back)
            }
            _ => {}
        }
    }

    fn change_setting(&mut self, back: bool) {
        fn cycle(cur: usize, len: usize, back: bool) -> usize {
            if back { (cur + len - 1) % len } else { (cur + 1) % len }
        }
        match self.set_sel {
            0 => {
                if self.devices.is_empty() {
                    return;
                }
                let i = self.devices.iter().position(|(n, _)| *n == self.cfg.audio_device).unwrap_or(0);
                self.cfg.audio_device = self.devices[cycle(i, self.devices.len(), back)].0.clone();
                self.apply_audio(true);
            }
            1 => {
                self.cfg.exclusive = !self.cfg.exclusive;
                if self.cfg.exclusive {
                    self.mpv_set("volume", 100);
                    self.status = "Bit-perfect: EQ/ReplayGain bypassed, volume locked to 100% — use your DAC/amp volume".into();
                }
                self.apply_audio(true);
            }
            2 => {
                let i = REPLAYGAIN.iter().position(|r| *r == self.cfg.replaygain).unwrap_or(0);
                self.cfg.replaygain = REPLAYGAIN[cycle(i, REPLAYGAIN.len(), back)].into();
                self.apply_audio(false);
            }
            3 => {
                self.cfg.gapless = !self.cfg.gapless;
                self.apply_audio(false);
            }
            4 => {
                let i = YTDL_FORMATS.iter().position(|(f, _)| *f == self.cfg.ytdl_format).unwrap_or(0);
                self.cfg.ytdl_format = YTDL_FORMATS[cycle(i, YTDL_FORMATS.len(), back)].0.into();
                self.apply_audio(false);
            }
            5 => {
                self.cfg.radio = !self.cfg.radio;
                self.cfg.save();
                self.maybe_radio();
            }
            6 => self.begin_input(Input::LibDir, self.cfg.library_dir.clone()),
            7 => self.begin_input(Input::LfmKey, self.cfg.lastfm_api_key.clone()),
            8 => self.begin_input(Input::LfmSecret, self.cfg.lastfm_api_secret.clone()),
            9 => self.lastfm_step(),
            10 => {
                if ytm::signed_in() {
                    self.yt_sign_out();
                } else {
                    self.start_yt_login();
                }
            }
            11 => {
                self.cfg.yt_account_playback = !self.cfg.yt_account_playback;
                self.cfg.save();
                self.mpv_set("ytdl-raw-options", config::ytdl_raw_options(&self.cfg));
                if self.cfg.yt_account_playback && !ytm::signed_in() {
                    self.status = "Sign in to YouTube Music first; until then playback stays anonymous".into();
                }
            }
            12 => {
                let i = art::PROTOCOLS.iter().position(|p| *p == self.cfg.image_protocol).unwrap_or(0);
                self.cfg.image_protocol = art::PROTOCOLS[cycle(i, art::PROTOCOLS.len(), back)].into();
                self.cfg.save();
                if let Some(c) = &mut self.covers {
                    c.set_protocol(&self.cfg.image_protocol);
                }
            }
            13 => {
                self.cfg.skip_ai = !self.cfg.skip_ai;
                self.cfg.save();
                if self.cfg.skip_ai {
                    self.status = "Downloading the AI artist list…".into();
                    self.spawn(|| Msg::AiList(aifilter::refresh()));
                }
                self.sync_next();
            }
            14 => {
                self.cfg.hide_ai = !self.cfg.hide_ai;
                self.cfg.save();
                if self.cfg.hide_ai {
                    self.hide_ai_discover();
                    self.save_cache();
                    if aifilter::count() == 0 {
                        self.spawn(|| Msg::AiList(aifilter::refresh()));
                    }
                } else {
                    // Hidden songs are gone from the list; fetch it again.
                    self.load_home();
                }
            }
            _ => {}
        }
    }

    fn lastfm_step(&mut self) {
        if self.lfm_ready() {
            self.cfg.lastfm_session.clear();
            self.cfg.lastfm_user.clear();
            self.cfg.save();
            self.status = "Last.fm disconnected".into();
            return;
        }
        if self.cfg.lastfm_api_key.is_empty() || self.cfg.lastfm_api_secret.is_empty() {
            self.status = "Enter a Last.fm API key + secret first (last.fm/api/account/create)".into();
            return;
        }
        let (k, s) = (self.cfg.lastfm_api_key.clone(), self.cfg.lastfm_api_secret.clone());
        match self.lfm_token.clone() {
            None => self.spawn(move || Msg::LfmToken(lastfm::get_token(&k, &s))),
            Some(tok) => self.spawn(move || Msg::LfmSession(lastfm::get_session(&k, &s, &tok))),
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

    /// Draw a cover; false while it loads or when there is none.
    pub fn render_cover(&mut self, f: &mut ratatui::Frame, area: Rect, slot: usize, key: &str) -> bool {
        let tx = self.tx.clone();
        self.covers.as_mut().is_some_and(|c| c.render(f, area, slot, key, &tx))
    }

    fn active_list(&mut self) -> Option<(&mut ListState, usize)> {
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
                if self.input.is_some() || self.help || self.picker.is_some() {
                    if self.input.take().is_none() {
                        self.picker = None;
                    }
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
                    if let Some((state, len)) = self.active_list() {
                        nav(state, len, if up { -3 } else { 3 });
                    }
                } else if m.row + 7 >= self.term_height {
                    // Wheel over the player bar changes volume.
                    self.mpv_cmd(json!(["add", "volume", if up { 5 } else { -5 }]));
                }
            }
            _ => {}
        }
    }

    pub fn config_path(&self) -> String {
        config::config_path().to_string_lossy().into_owned()
    }
}

/// "PLxxx" / "VLPLxxx" -> the browse id library pages are keyed by.
fn browse_id_of(playlist_id: &str) -> String {
    if playlist_id.starts_with("VL") { playlist_id.to_string() } else { format!("VL{playlist_id}") }
}

fn ai_left_out(n: usize) -> String {
    if n == 0 { String::new() } else { format!(" ({n} AI-generated left out)") }
}

fn mask(s: &str) -> String {
    if s.is_empty() {
        "(not set)".into()
    } else {
        format!("{}…", s.chars().take(6).collect::<String>())
    }
}
