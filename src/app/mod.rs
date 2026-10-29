//! App state and the shared helpers. The rest of `impl App` is spread over
//! the submodules: key dispatch, queue and playback, background messages,
//! scrobbling, settings, and search / Discover / Library lists.

mod browse;
mod keys;
mod msg;
mod queue;
mod scrobble;
mod settings;

pub use msg::Msg;
pub use settings::{SettingRow, SETTINGS};

use crate::{
    aifilter,
    art::Covers,
    config::{self, Config, State},
    lyrics::Lyrics,
    model::Track,
    mpv::Mpv,
    ytm::{self, AddTarget, Collection, Kind, Results},
};
use ratatui::{crossterm::event::KeyCode, layout::Rect, widgets::ListState};
use scrobble::{submit_scrobbles, MAX_PENDING_SCROBBLES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    Search,
    LibFilter,
    LibDir,
    LfmKey,
    LfmSecret,
    YtCookies,
    NewPlaylist,
}


pub struct App {
    pub cfg: Config,
    tx: Sender<Msg>,
    pub mpv: Option<Mpv>,
    pub view: View,
    pub input: Option<Input>,
    pub input_buf: String,
    pub help: bool,
    pub quit: bool,
    /// Something visible changed since the last draw.
    pub dirty: bool,
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
    /// Set through `set_lib_filter` so `lib_vis` stays in step.
    lib_filter: String,
    /// "title artist album" of each library track, lowercased once at load.
    lib_search: Vec<String>,
    /// Indices into `library` that match `lib_filter`.
    lib_vis: Vec<usize>,
    pub lib_sel: ListState,
    pub lib_scanning: bool,
    /// Why the library folder can't be scanned (shown in the empty Library view).
    pub lib_error: Option<String>,
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
    /// Last.fm connect in progress: (token, approval URL) once Last.fm handed out a token.
    pub lfm_auth: Option<(String, String)>,
    /// A Last.fm connect request is on its way.
    pub lfm_busy: bool,
    /// Bumped when a connect attempt starts over or is cancelled; stale replies are dropped.
    lfm_seq: u64,

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
    /// The song whose scrobble is pending (taken when it finishes).
    playing: Option<Track>,
    /// Length mpv reported for the playing song (0 = unknown).
    track_len: f64,
    /// mpv reached the end of the playing song; its stats are frozen.
    ended: bool,
    /// The queue ran out: the last song ended with nothing after it.
    at_end: bool,
    /// The gapless advance was seen via `path` first; the old file's eof is still to come.
    skip_eof: bool,
    fail_streak: u32,
    scrobble_queue: Vec<(Track, u64)>,
    /// Scrobbles being sent: the thread (returns what it couldn't send) and
    /// a copy of what it was given, kept in case it doesn't finish by exit.
    scrobble_job: Option<(thread::JoinHandle<config::PendingScrobbles>, config::PendingScrobbles)>,
    lfm_bad_session: bool,

    queue_undo: Vec<(Vec<Track>, Option<usize>)>,
    /// Settings row whose sign-out waits for a second Enter.
    confirm_signout: Option<SettingRow>,
    /// mpv could not start / exited; kept at the front of the status line.
    mpv_error: Option<String>,
    mpv_starting: bool,
    /// Play `cur` once mpv is up.
    pending_play: bool,
    browse_seq: u64,
    radio_seq: u64,
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
            dirty: true,
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
            lib_search: Vec::new(),
            lib_vis: Vec::new(),
            lib_sel: ListState::default(),
            lib_scanning: false,
            lib_error: None,
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
            lfm_auth: None,
            lfm_busy: false,
            lfm_seq: 0,
            covers: None,
            buttons: Vec::new(),
            list_area: None,
            term_height: 0,
            last_click: None,
            played: 0.0,
            last_pos: 0.0,
            started_at: 0,
            playing: None,
            track_len: 0.0,
            ended: false,
            at_end: false,
            skip_eof: false,
            fail_streak: 0,
            scrobble_queue: Vec::new(),
            scrobble_job: None,
            lfm_bad_session: false,
            queue_undo: Vec::new(),
            confirm_signout: None,
            mpv_error: None,
            mpv_starting: false,
            pending_play: false,
            browse_seq: 0,
            radio_seq: 0,
        }
    }

    pub fn start(&mut self) {
        // mpv can take seconds to come up; start it off the UI thread so the
        // first frame draws right away.
        self.mpv_starting = true;
        let (cfg, tx) = (self.cfg.clone(), self.tx.clone());
        self.spawn(move || {
            Msg::MpvReady(Mpv::spawn(&cfg, tx).map(|m| {
                let devices = m
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
                (m, devices)
            }))
        });
        self.rescan();
        // Scrobbles a previous run couldn't send.
        self.scrobble_queue = config::load_scrobbles(MAX_PENDING_SCROBBLES);
        self.flush_scrobbles();
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

    /// Save state, stop mpv and send pending scrobbles (waiting up to ~3s).
    pub fn shutdown(&mut self) {
        self.queue_scrobble();
        self.cfg.volume = self.volume;
        self.cfg.save();
        State { queue: self.queue.clone(), cur: self.cur }.save();
        if let Some(m) = &mut self.mpv {
            m.quit();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        // Scrobbles already on their way: let them land.
        if let Some((job, sent)) = self.scrobble_job.take() {
            while !job.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            // Its report never reaches on_scrobbled now; take what it left.
            let unsent = if job.is_finished() { job.join().unwrap_or(sent) } else { sent };
            self.scrobble_queue.splice(0..0, unsent);
        }
        if !self.scrobble_queue.is_empty() && self.lfm_ready() && !self.lfm_bad_session {
            let items = std::mem::take(&mut self.scrobble_queue);
            let sent = items.clone();
            let (k, s, sk) = self.lfm_creds();
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(submit_scrobbles(&k, &s, &sk, items));
            });
            let left = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
            // Not answered in time: keep them all (a repeat beats a lost scrobble).
            self.scrobble_queue = left.map(|r| r.unsent).unwrap_or(sent);
        }
        // Whatever is still unsent is retried next run.
        let extra = self.scrobble_queue.len().saturating_sub(MAX_PENDING_SCROBBLES);
        self.scrobble_queue.drain(..extra);
        config::save_scrobbles(&self.scrobble_queue);
    }

    /// Keep an mpv failure visible: routine messages go after it.
    fn keep_sticky(&mut self) {
        let Some(e) = &self.mpv_error else { return };
        if !self.status.starts_with(e.as_str()) {
            self.status = if self.status.is_empty() { e.clone() } else { format!("{e} · {}", self.status) };
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

    pub fn current(&self) -> Option<&Track> {
        self.cur.and_then(|i| self.queue.get(i))
    }

    /// Draw a cover; false while it loads or when there is none.
    pub fn render_cover(&mut self, f: &mut ratatui::Frame, area: Rect, slot: usize, key: &str) -> bool {
        let tx = self.tx.clone();
        self.covers.as_mut().is_some_and(|c| c.render(f, area, slot, key, &tx))
    }

    pub fn config_path(&self) -> String {
        config::config_path().to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod testutil {
    use crate::model::{Source, Track};

    pub fn t(id: &str) -> Track {
        Track {
            source: Source::Yt(id.into()),
            title: id.into(),
            artist: String::new(),
            album: String::new(),
            duration: 0,
            quality: String::new(),
            art: String::new(),
        }
    }

    pub fn ids(q: &[Track]) -> Vec<String> {
        q.iter().map(|t| t.title.clone()).collect()
    }
}
