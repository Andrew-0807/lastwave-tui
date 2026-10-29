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
    sync::mpsc::{self, Sender},
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
    /// (request id, opened from the Library view?, page, tracks)
    Browse(u64, bool, Collection, Result<Vec<Track>, String>),
    Home(Result<Vec<Shelf>, String>),
    YtLogin(Result<(String, String), String>),
    YtLists(Result<Vec<Collection>, String>),
    /// (request id, tracks)
    Radio(u64, Result<Vec<Track>, String>),
    /// mpv finished starting in the background: (player, audio devices).
    MpvReady(Result<(Mpv, Vec<(String, String)>), String>),
    Scrobbled(ScrobbleReport),
    Lyrics(String, Option<Lyrics>),
    Library(Result<Vec<Track>, String>),
    Status(String),
    /// (connect attempt, token)
    LfmToken(u64, Result<String, String>),
    /// (connect attempt, (session key, user name))
    LfmSession(u64, Result<(String, String), lastfm::LfmError>),
    Art(String, Option<DynamicImage>),
    AiList(Result<usize, String>),
    Liked(Result<HashSet<String>, String>),
    /// (video id, liked, result)
    LikeResult(String, bool, Result<(), String>),
    AddTargets(String, Result<Vec<AddTarget>, String>),
    /// (playlist title, browse id of the playlist added to)
    Added(String, Result<String, String>),
}

/// Outcome of sending queued scrobbles, in order.
#[derive(Default)]
pub struct ScrobbleReport {
    accepted: Vec<Track>,
    /// Taken by Last.fm but not counted, or refused for good: (track, why).
    ignored: Vec<(Track, String)>,
    /// Not sent because of a temporary error; retried later.
    unsent: Vec<(Track, u64)>,
    error: Option<lastfm::LfmError>,
}

/// Send scrobbles oldest first, stopping at the first temporary failure.
fn submit_scrobbles(k: &str, s: &str, sk: &str, items: Vec<(Track, u64)>) -> ScrobbleReport {
    let mut r = ScrobbleReport::default();
    let mut it = items.into_iter();
    for (t, ts) in it.by_ref() {
        match lastfm::scrobble(k, s, sk, &t, ts) {
            Ok(None) => r.accepted.push(t),
            Ok(Some(why)) => r.ignored.push((t, why)),
            Err(e) if e.retryable() => {
                r.unsent.push((t, ts));
                r.error = Some(e);
                break;
            }
            Err(e) => r.ignored.push((t, e.to_string())),
        }
    }
    r.unsent.extend(it);
    r
}

/// Last.fm's rule: the track is longer than 30s and was played for half
/// its length or 4 minutes. `length` is 0 when unknown; a track that played
/// to its end then counts as long as what was heard.
pub(crate) fn scrobble_eligible(length: f64, played: f64, ended: bool) -> bool {
    let length = if length > 0.0 { length } else if ended { played } else { 0.0 };
    if length <= 0.0 {
        return played >= 240.0;
    }
    length > 30.0 && (played >= length / 2.0 || played >= 240.0)
}

/// The queue to go back to on undo, kept consistent with what is playing:
/// the playing song stays current, put back after the old current one if
/// the old queue doesn't hold it.
fn restore_queue(mut q: Vec<Track>, old_cur: Option<usize>, playing: Option<&Track>) -> (Vec<Track>, Option<usize>) {
    let Some(p) = playing else { return (q, None) };
    let url = p.url();
    if let Some(c) = old_cur.filter(|&c| q.get(c).is_some_and(|t| t.url() == url)) {
        return (q, Some(c));
    }
    if let Some(i) = q.iter().position(|t| t.url() == url) {
        return (q, Some(i));
    }
    let at = old_cur.map_or(q.len(), |c| (c + 1).min(q.len()));
    q.insert(at, p.clone());
    (q, Some(at))
}

const UNDO_DEPTH: usize = 10;
const MAX_PENDING_SCROBBLES: usize = 1000;
/// Songs in a row that may fail to load before playback stops.
const MAX_FAILS: u32 = 5;

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

const SET_LFM_ACCOUNT: usize = 9;
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
    confirm_signout: Option<usize>,
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

    /// What song actions apply to: the highlighted row of the view's list,
    /// else (no list, or nothing in it) the playing song.
    fn target(&self) -> Option<Highlight> {
        self.highlighted().or_else(|| self.current().cloned().map(Highlight::Track))
    }

    /// The song `target` points at; says why not when it's an album/playlist or nothing.
    fn target_track(&mut self, verb: &str) -> Option<Track> {
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
    fn account_target(&mut self, what: &str, verb: &str) -> Option<(Track, String)> {
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

    fn toggle_like(&mut self) {
        let Some((t, id)) = self.account_target("like songs", "like it") else { return };
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
        let Some((t, id)) = self.account_target("add to playlists", "add it to a playlist") else { return };
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
        self.browse_seq += 1;
        let seq = self.browse_seq;
        self.spawn(move || {
            let r = ytm::browse(&c.browse_id);
            Msg::Browse(seq, lib, c, r)
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
                self.set_lib_filter(self.input_buf.clone());
            }
        }
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
        // The cursor follows playback only if it was on the playing song.
        if self.cur.is_none() || self.queue_sel.selected().is_none() || self.queue_sel.selected() == self.cur {
            self.queue_sel.select(Some(i));
        }
        self.cur = Some(i);
        self.at_end = false;
        self.skip_eof = false;
        if self.mpv.is_none() {
            self.pending_play = self.mpv_starting;
            self.status = if self.mpv_starting { "Starting mpv…".into() } else { "mpv is not running".into() };
            return;
        }
        self.mpv_cmd(json!(["loadfile", url, "replace"]));
        self.mpv_set("pause", false);
        self.mpv_next = None;
        self.on_track_start();
        self.sync_next();
    }

    /// mpv moved on to the prefetched entry `n` (gapless advance).
    fn advance_to(&mut self, n: usize) {
        let Some(c) = self.cur else { return };
        self.finish_track();
        self.note_skipped(n.saturating_sub(c + 1));
        if self.queue_sel.selected() == Some(c) {
            self.queue_sel.select(Some(n));
        }
        self.cur = Some(n);
        self.mpv_cmd(json!(["playlist-remove", 0]));
        self.mpv_next = None;
        self.on_track_start();
        self.sync_next();
    }

    /// After the queue ran out, start whatever has been added since.
    fn resume_if_ended(&mut self) {
        if self.at_end && self.idle && self.mpv.is_some() && self.cur.and_then(|c| self.next_playable(c + 1)).is_some() {
            self.next();
        }
    }

    /// Remember the queue before a change that replaces or shortens it.
    fn snapshot_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.queue_undo.len() >= UNDO_DEPTH {
            self.queue_undo.remove(0);
        }
        self.queue_undo.push((self.queue.clone(), self.cur));
    }

    fn undo_queue(&mut self) {
        let Some((q, old_cur)) = self.queue_undo.pop() else {
            self.status = "Nothing to undo".into();
            return;
        };
        let playing = self.current().cloned();
        let (q, cur) = restore_queue(q, old_cur, playing.as_ref());
        self.queue = q;
        self.cur = cur;
        self.queue_sel.select(cur.or((!self.queue.is_empty()).then_some(0)));
        // Radio asked for the replaced queue no longer applies.
        self.radio_seq += 1;
        self.radio_busy = false;
        self.sync_next();
        self.status = format!("Queue restored ({} tracks)", self.queue.len());
        self.resume_if_ended();
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
        self.track_len = 0.0;
        self.ended = false;
        self.at_end = false;
        self.started_at = now();
        self.playing = self.current().cloned();
        let Some(t) = self.current().cloned() else { return };
        self.flush_scrobbles();

        self.lyrics = None;
        self.lyrics_loading = true;
        self.lyrics_key = t.url();
        let key = self.lyrics_key.clone();
        let lt = t.clone();
        self.spawn(move || Msg::Lyrics(key, lyrics::fetch(&lt)));

        if self.lfm_ready() && !self.lfm_bad_session {
            let (k, s, sk) = self.lfm_creds();
            let nt = t.clone();
            self.spawn(move || match lastfm::now_playing(&k, &s, &sk, &nt) {
                Ok(()) => Msg::Status(String::new()),
                Err(e) => Msg::Status(e.to_string()),
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
        self.request_radio(id);
    }

    fn request_radio(&mut self, id: String) {
        self.radio_busy = true;
        self.radio_seq += 1;
        let seq = self.radio_seq;
        self.spawn(move || Msg::Radio(seq, ytm::radio(&id)));
    }

    fn finish_track(&mut self) {
        self.queue_scrobble();
        self.flush_scrobbles();
    }

    /// Queue the song that was playing for scrobbling if it qualifies.
    fn queue_scrobble(&mut self) {
        let Some(mut t) = self.playing.take() else { return };
        let len = if self.track_len > 0.0 { self.track_len } else { t.duration as f64 };
        if !self.lfm_ready() || !scrobble_eligible(len, self.played, self.ended) {
            return;
        }
        if t.duration == 0 {
            t.duration = if len > 0.0 { len } else { self.played } as u32;
        }
        if self.scrobble_queue.len() >= MAX_PENDING_SCROBBLES {
            self.scrobble_queue.remove(0);
        }
        self.scrobble_queue.push((t, self.started_at));
    }

    /// Send queued scrobbles (the new one plus any that failed before).
    fn flush_scrobbles(&mut self) {
        if self.scrobble_job.as_ref().is_some_and(|(j, _)| !j.is_finished())
            || self.scrobble_queue.is_empty()
            || !self.lfm_ready()
            || self.lfm_bad_session
        {
            return;
        }
        let items = std::mem::take(&mut self.scrobble_queue);
        let sent = items.clone();
        let (k, s, sk) = self.lfm_creds();
        let tx = self.tx.clone();
        let job = thread::spawn(move || {
            let r = submit_scrobbles(&k, &s, &sk, items);
            let unsent = r.unsent.clone();
            let _ = tx.send(Msg::Scrobbled(r));
            unsent
        });
        self.scrobble_job = Some((job, sent));
    }

    fn on_scrobbled(&mut self, r: ScrobbleReport) {
        self.scrobble_job = None;
        let n_unsent = r.unsent.len();
        let mut q = r.unsent;
        q.append(&mut self.scrobble_queue);
        self.scrobble_queue = q;
        let waiting = |n: usize| if n == 1 { "1 scrobble waiting".to_string() } else { format!("{n} scrobbles waiting") };
        match &r.error {
            Some(e) if e.bad_session() => {
                self.lfm_bad_session = true;
                self.status = format!("{e} ({})", waiting(self.scrobble_queue.len()));
                return;
            }
            Some(e) => {
                self.status = format!("Scrobble failed: {e} — {}, retrying with the next song", waiting(self.scrobble_queue.len()));
                return;
            }
            None => {}
        }
        if let Some((t, why)) = r.ignored.last() {
            self.status = format!("Last.fm ignored {} — {} ({why})", t.artist, t.title);
        } else if let Some(t) = r.accepted.last() {
            let more = r.accepted.len() - 1;
            let more = if more > 0 { format!(" (+{more} earlier)") } else { String::new() };
            self.status = format!("Scrobbled: {} — {}{more}", t.artist, t.title);
        }
        if n_unsent == 0 && !r.accepted.is_empty() {
            // Last.fm is reachable again: send what queued up meanwhile.
            self.flush_scrobbles();
        }
    }

    fn lfm_ready(&self) -> bool {
        !self.cfg.lastfm_session.is_empty() && !self.cfg.lastfm_api_key.is_empty()
    }

    fn lfm_creds(&self) -> (String, String, String) {
        (self.cfg.lastfm_api_key.clone(), self.cfg.lastfm_api_secret.clone(), self.cfg.lastfm_session.clone())
    }

    pub fn play_list(&mut self, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.finish_track();
        let had = !self.queue.is_empty();
        self.snapshot_queue();
        self.queue = tracks;
        self.cur = None;
        self.radio_seq += 1;
        self.radio_busy = false;
        self.play_index(start);
        if had {
            self.status = "Queue replaced — u to undo".into();
        }
    }

    /// Insert right after the current track (or at the end when idle).
    pub fn play_next(&mut self, tracks: Vec<Track>) {
        let (tracks, ai) = self.drop_ai(tracks);
        let n = tracks.len();
        let at = self.cur.map_or(self.queue.len(), |c| c + 1);
        self.queue.splice(at..at, tracks);
        self.sync_next();
        self.status = format!("{n} track{} will play next{}", if n == 1 { "" } else { "s" }, ai_left_out(ai));
        self.resume_if_ended();
    }

    pub fn append(&mut self, tracks: Vec<Track>) {
        let (tracks, ai) = self.drop_ai(tracks);
        let n = tracks.len();
        self.queue.extend(tracks);
        self.sync_next();
        self.status = format!("Added {n} track{} to queue{}", if n == 1 { "" } else { "s" }, ai_left_out(ai));
        self.resume_if_ended();
    }

    fn start_radio(&mut self, t: Track) {
        let had = !self.queue.is_empty();
        self.play_list(vec![t], 0);
        if let Some(id) = self.queue[0].video_id().map(str::to_string) {
            self.request_radio(id);
            self.status = if had { "Starting radio… (u to undo)" } else { "Starting radio…" }.into();
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
        let cut = self.queue.len() > c + 1;
        if cut {
            self.snapshot_queue();
        }
        self.queue.truncate(c + 1);
        self.sync_next();
        self.request_radio(id);
        self.status = if cut { "Starting radio… (u to undo)" } else { "Starting radio…" }.into();
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
        self.snapshot_queue();
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
        if before == self.queue.len() {
            self.queue_undo.pop();
        }
        before - self.queue.len()
    }

    fn prev(&mut self) {
        match self.cur.and_then(|c| self.prev_playable(c)) {
            Some(i) if self.pos <= 3.0 => self.play_index(i),
            _ => self.mpv_cmd(json!(["seek", 0, "absolute"])),
        }
    }

    fn toggle_pause(&mut self) {
        if self.mpv.is_none() {
            self.pending_play = self.mpv_starting && self.cur.is_some();
            self.status = if self.mpv_starting { "Starting mpv…".into() } else { "mpv is not running".into() };
            return;
        }
        if self.idle {
            // After the queue ran out, go on with anything added since;
            // with nothing after it, play the last song again.
            let from = match self.cur {
                Some(c) if self.at_end && self.next_playable(c + 1).is_some() => c + 1,
                c => c.unwrap_or(0),
            };
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
        if was_current {
            // Before the queue shifts: the scrobble is for this song.
            self.finish_track();
        }
        self.snapshot_queue();
        self.queue.remove(i);
        self.status = "Removed — u to undo".into();
        match self.cur {
            Some(c) if c > i => self.cur = Some(c - 1),
            _ => {}
        }
        if was_current {
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
        self.lib_error = None;
        if self.cfg.library_dir.is_empty() {
            return;
        }
        if let Err(e) = library::check_dir(&dir) {
            self.lib_error = Some(e.clone());
            self.status = e;
            return;
        }
        self.lib_scanning = true;
        self.spawn(move || Msg::Library(library::try_scan(&dir)));
    }

    /// Library tracks matching the filter (indices into `library`), cached.
    pub fn lib_visible(&self) -> &[usize] {
        &self.lib_vis
    }

    pub fn lib_filter(&self) -> &str {
        &self.lib_filter
    }

    pub fn set_lib_filter(&mut self, f: String) {
        if f != self.lib_filter {
            self.lib_filter = f;
            self.refilter_library();
        }
    }

    fn set_library(&mut self, l: Vec<Track>) {
        self.lib_search = l.iter().map(|t| format!("{} {} {}", t.title, t.artist, t.album).to_lowercase()).collect();
        self.library = l;
        self.refilter_library();
    }

    fn refilter_library(&mut self) {
        let f = self.lib_filter.to_lowercase();
        let words: Vec<&str> = f.split_whitespace().collect();
        self.lib_vis = (0..self.library.len()).filter(|&i| words.iter().all(|w| self.lib_search[i].contains(w))).collect();
    }

    fn run_search(&mut self) {
        if self.query.trim().is_empty() {
            return;
        }
        self.search_seq += 1;
        // A page still loading would cover these results when it lands.
        self.browse_seq += 1;
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
            // A Discover song's list is its shelf: the songs between the headers around it.
            let i = self.discover_sel.selected().filter(|&i| matches!(self.discover.get(i), Some(DItem::Track(_))))?;
            let start = self.discover[..i].iter().rposition(|d| matches!(d, DItem::Header(_))).map_or(0, |h| h + 1);
            let end = self.discover[i..].iter().position(|d| matches!(d, DItem::Header(_))).map_or(self.discover.len(), |n| i + n);
            let at = self.discover[start..i].iter().filter(|d| matches!(d, DItem::Track(_))).count();
            let shelf = self.discover[start..end].iter().filter_map(|d| if let DItem::Track(t) = d { Some(t.clone()) } else { None }).collect();
            return Some((shelf, at));
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
        // mpv reports these many times a second; the periodic redraw shows them.
        let frequent = matches!(&m, Msg::Mpv(MpvEvent::Prop(n, _)) if n == "time-pos" || n == "audio-bitrate");
        let status = frequent.then(|| self.status.clone());
        self.handle_msg(m);
        self.keep_sticky();
        if status.is_none_or(|s| s != self.status) {
            self.dirty = true;
        }
    }

    fn handle_msg(&mut self, m: Msg) {
        match m {
            Msg::Mpv(ev) => self.on_mpv(ev),
            Msg::MpvReady(r) => {
                self.mpv_starting = false;
                match r {
                    Ok((m, devices)) => {
                        self.devices = devices;
                        self.mpv = Some(m);
                        if std::mem::take(&mut self.pending_play) {
                            if let Some(c) = self.cur {
                                self.play_index(c);
                            }
                        }
                    }
                    Err(e) => {
                        self.pending_play = false;
                        self.mpv_error = Some(e);
                    }
                }
            }
            Msg::Scrobbled(r) => self.on_scrobbled(r),
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
            Msg::Browse(seq, lib, c, r) => {
                let fresh = seq == self.browse_seq;
                if fresh {
                    self.searching = false;
                    self.yt_loading = false;
                }
                // A page opened before something else: keep a library copy, drop the rest.
                if !fresh && (!lib || r.is_err()) {
                    return;
                }
                match r {
                    Ok(mut t) => {
                        ytm::fill_from_collection(&mut t, &c);
                        if fresh {
                            self.status = format!("{}: {} tracks", c.title, t.len());
                        }
                        let first = (!t.is_empty()).then_some(0);
                        if lib {
                            // A refresh of what's already on screen keeps the cursor.
                            let same = self.yt_open.as_ref().is_some_and(|(o, _)| o.browse_id == c.browse_id);
                            let show = same || (fresh && self.yt_open.is_none());
                            if show && (!same || self.yt_open_sel.selected().is_none_or(|i| i >= t.len())) {
                                self.yt_open_sel.select(first);
                            }
                            if self.cache.pages.len() >= MAX_CACHED_PAGES && !self.cache.pages.contains_key(&c.browse_id) {
                                if let Some(k) = self.cache.pages.keys().next().cloned() {
                                    self.cache.pages.remove(&k);
                                }
                            }
                            self.cache.pages.insert(c.browse_id.clone(), t.clone());
                            // Only replace the view if the user is still on it (or on the list).
                            if show {
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
            // Radio for a queue that has been replaced since.
            Msg::Radio(seq, _) if seq != self.radio_seq => {}
            Msg::Radio(_, r) => {
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
                        self.resume_if_ended();
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
            Msg::Library(Ok(l)) => {
                self.lib_scanning = false;
                self.status = format!("Library: {} tracks", l.len());
                self.set_library(l);
                nav(&mut self.lib_sel, self.lib_vis.len(), 0);
            }
            // The folder went away or became unreadable since check_dir.
            Msg::Library(Err(e)) => {
                self.lib_scanning = false;
                self.lib_error = Some(e.clone());
                self.status = e;
            }
            Msg::Status(s) => {
                if !s.is_empty() {
                    self.status = s;
                }
            }
            // A reply to a connect attempt that was cancelled or started over.
            Msg::LfmToken(seq, _) | Msg::LfmSession(seq, _) if seq != self.lfm_seq => {}
            Msg::LfmToken(_, r) => {
                self.lfm_busy = false;
                match r {
                    Ok(tok) => {
                        // Opened once per token; Enter afterwards only checks for approval.
                        let url = lastfm::auth_url(&self.cfg.lastfm_api_key, &tok);
                        lastfm::open_url(&url);
                        self.lfm_auth = Some((tok, url));
                        self.status = "Approve LastWave on Last.fm (link in Settings → Notes), then Enter on Last.fm account".into();
                    }
                    Err(e) => self.status = e,
                }
            }
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
            Msg::LfmSession(_, r) => {
                self.lfm_busy = false;
                match r {
                    Ok((sk, name)) => {
                        self.cfg.lastfm_session = sk;
                        self.status = format!("Last.fm connected as {name}");
                        self.cfg.lastfm_user = name;
                        self.lfm_auth = None;
                        self.lfm_bad_session = false;
                        self.cfg.save();
                        self.flush_scrobbles();
                    }
                    Err(e) if e.token_not_approved() => {
                        self.status = "Not approved yet — open the link in Settings → Notes, approve, then Enter again (Esc cancels)".into();
                    }
                    // The token can't become a session any more: start over.
                    Err(e) if e.token_dead() => {
                        self.lfm_auth = None;
                        self.status = "The Last.fm approval link expired or is invalid — press Enter on Last.fm account to get a new one".into();
                    }
                    Err(e) => self.status = format!("{e} — Enter to try again, Esc to cancel"),
                }
            }
        }
    }

    fn on_mpv(&mut self, ev: MpvEvent) {
        match ev {
            MpvEvent::Prop(name, v) => match name.as_str() {
                "time-pos" => {
                    let p = v.as_f64().unwrap_or(0.0);
                    let d = p - self.last_pos;
                    // After eof the next file's position may arrive first; don't count it.
                    if !self.paused && !self.ended && d > 0.0 && d < 2.0 {
                        self.played += d;
                        if self.played > 1.0 {
                            self.fail_streak = 0;
                        }
                    }
                    self.last_pos = p;
                    self.pos = p;
                }
                "duration" => {
                    self.dur = v.as_f64().unwrap_or(0.0);
                    if self.dur > 0.0 && !self.ended {
                        self.track_len = self.dur;
                    }
                }
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
                    // mpv moved on to the prefetched next entry (gapless advance)
                    // before its eof for the old file came in.
                    if let Some(n) = self.next_playable(c + 1).filter(|&n| self.queue[n].url() == p) {
                        self.advance_to(n);
                        self.skip_eof = true;
                    }
                }
                _ => {}
            },
            MpvEvent::EndFile(reason) => match reason.as_str() {
                "error" => self.on_load_error(),
                "eof" if std::mem::take(&mut self.skip_eof) => {}
                "eof" => {
                    self.ended = true;
                    // With a prefetched entry mpv goes straight on to it. Done
                    // here rather than on `path`, which doesn't change when the
                    // same song is queued twice in a row.
                    let next = self.mpv_next.as_ref().and(self.cur).and_then(|c| self.next_playable(c + 1));
                    match next {
                        Some(n) => self.advance_to(n),
                        None => {
                            self.finish_track();
                            self.at_end = true;
                        }
                    }
                }
                _ => {}
            },
            MpvEvent::Exited => {
                self.mpv = None;
                self.idle = true;
                self.mpv_error = Some("mpv exited — restart LastWave TUI".into());
            }
        }
    }

    /// The playing song failed to load: skip to the next one, up to a point.
    fn on_load_error(&mut self) {
        let Some(c) = self.cur else { return };
        let what = self.current().map(|t| t.title.clone()).unwrap_or_default();
        self.playing = None;
        self.fail_streak += 1;
        if self.fail_streak >= MAX_FAILS {
            self.fail_streak = 0;
            self.mpv_cmd(json!(["stop"]));
            self.mpv_next = None;
            self.at_end = false;
            self.status = format!("Stopped: {MAX_FAILS} songs in a row could not play (is yt-dlp installed and up to date?)");
            return;
        }
        match self.next_playable(c + 1) {
            Some(n) => {
                self.play_index(n);
                self.status = format!("Could not play \"{what}\" — skipped");
            }
            None => {
                self.at_end = true;
                self.status = format!("Could not play \"{what}\" (is yt-dlp installed and up to date?)");
            }
        }
    }

    // ---- keys -------------------------------------------------------------------

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

    fn handle_key(&mut self, k: KeyEvent) {
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

    fn run(&mut self, a: Act) {
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
                if !self.signed_in_row(self.set_sel) {
                    self.change_setting(back);
                }
            }
        }
    }

    /// Enter: play the highlighted song (its list from there on becomes the
    /// queue), open the highlighted album/playlist/artist, or change the
    /// highlighted setting.
    fn activate(&mut self) {
        match self.view {
            View::Settings => self.settings_enter(),
            View::Search | View::Queue | View::Library => {
                if let Some((tracks, i)) = self.list_context() {
                    if self.view == View::Queue {
                        self.play_index(i);
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
    fn back(&mut self) {
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
    fn refresh(&mut self) {
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

    fn move_cursor(&mut self, d: isize) {
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
    fn list_context(&self) -> Option<(Vec<Track>, usize)> {
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
    fn play_next_target(&mut self) {
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
    fn radio_target(&mut self) {
        let Some(t) = self.target_track("start a radio from it") else { return };
        if t.video_id().is_none() {
            self.status = "Radio needs a YouTube song".into();
        } else if self.current().is_some_and(|c| c.source == t.source) {
            self.radio_from_current();
        } else {
            self.start_radio(t);
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
            6 if self.cfg.library_dir.is_empty() => "(not set — Enter to choose your music folder)".into(),
            6 => self.cfg.library_dir.clone(),
            7 => mask(&self.cfg.lastfm_api_key),
            8 => mask(&self.cfg.lastfm_api_secret),
            10 => {
                if ytm::signed_in() {
                    format!("signed in as {} (Enter twice to sign out)", self.cfg.yt_user)
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
                    format!("connected as {} (Enter twice to disconnect)", self.cfg.lastfm_user)
                } else if self.lfm_busy {
                    "contacting Last.fm…".into()
                } else if self.lfm_auth.is_some() {
                    "waiting for approval (Enter checks, Esc cancels)".into()
                } else {
                    "not connected (Enter to connect)".into()
                }
            }
            _ => String::new(),
        }
    }

    /// Enter in Settings. Signed-in account rows sign out on a second Enter.
    fn settings_enter(&mut self) {
        let armed = self.confirm_signout.take();
        let row = self.set_sel;
        if !self.signed_in_row(row) {
            self.change_setting(false);
        } else if armed == Some(row) {
            if row == SET_YT_ACCOUNT {
                self.yt_sign_out();
            } else {
                self.lfm_disconnect();
            }
        } else {
            self.confirm_signout = Some(row);
            let what = if row == SET_YT_ACCOUNT { "sign out of YouTube Music" } else { "disconnect Last.fm" };
            self.status = format!("Press Enter again to {what} (any other key cancels)");
        }
    }

    /// An account row (Last.fm / YouTube Music) that is currently signed in.
    fn signed_in_row(&self, row: usize) -> bool {
        match row {
            SET_LFM_ACCOUNT => self.lfm_ready(),
            SET_YT_ACCOUNT => ytm::signed_in(),
            _ => false,
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
            // Signed-in account rows are handled (with confirmation) in key_settings.
            SET_LFM_ACCOUNT if !self.lfm_ready() => self.lastfm_step(),
            SET_YT_ACCOUNT if !ytm::signed_in() => self.start_yt_login(),
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

    fn lfm_disconnect(&mut self) {
        self.cfg.lastfm_session.clear();
        self.cfg.lastfm_user.clear();
        self.cfg.save();
        self.lfm_bad_session = false;
        self.status = "Last.fm disconnected".into();
    }

    fn lastfm_step(&mut self) {
        if self.lfm_busy {
            self.status = "Still waiting for Last.fm… (Esc cancels)".into();
            return;
        }
        if self.cfg.lastfm_api_key.is_empty() || self.cfg.lastfm_api_secret.is_empty() {
            self.status = "Enter a Last.fm API key + secret first (last.fm/api/account/create)".into();
            return;
        }
        let (k, s, seq) = (self.cfg.lastfm_api_key.clone(), self.cfg.lastfm_api_secret.clone(), self.lfm_seq);
        self.lfm_busy = true;
        match self.lfm_auth.clone() {
            None => {
                self.status = "Asking Last.fm for an approval link…".into();
                self.spawn(move || Msg::LfmToken(seq, lastfm::get_token(&k, &s)));
            }
            // The browser was opened when the token came; now just ask whether it was approved.
            Some((tok, _)) => {
                self.status = "Checking with Last.fm…".into();
                self.spawn(move || Msg::LfmSession(seq, lastfm::get_session(&k, &s, &tok)));
            }
        }
    }

    /// Drop a Last.fm connect in progress; replies still on their way are ignored.
    fn lfm_cancel(&mut self) {
        self.lfm_seq += 1;
        self.lfm_busy = false;
        self.lfm_auth = None;
        self.status = "Last.fm connect cancelled".into();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn t(id: &str) -> Track {
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

    fn ids(q: &[Track]) -> Vec<String> {
        q.iter().map(|t| t.title.clone()).collect()
    }

    #[test]
    fn scrobble_rule() {
        assert!(!scrobble_eligible(25.0, 25.0, true), "too short");
        assert!(scrobble_eligible(200.0, 100.0, false), "half played");
        assert!(!scrobble_eligible(200.0, 90.0, false));
        assert!(scrobble_eligible(1200.0, 240.0, false), "4 minutes of a long track");
        // Length unknown: a song that played to its end counts as its play time.
        assert!(scrobble_eligible(0.0, 180.0, true));
        assert!(!scrobble_eligible(0.0, 20.0, true));
        assert!(!scrobble_eligible(0.0, 180.0, false));
        assert!(scrobble_eligible(0.0, 240.0, false));
    }

    #[test]
    fn library_filter_is_cached_and_follows_changes() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        let mut a = t("Blue Monday");
        a.artist = "New Order".into();
        let mut b = t("Atmosphere");
        b.artist = "Joy Division".into();
        b.album = "Substance".into();
        let mut c = t("Ceremony");
        c.artist = "New Order".into();
        app.set_library(vec![a, b, c]);
        assert_eq!(app.lib_visible(), [0, 1, 2]);
        app.set_lib_filter("new ORDER".into());
        assert_eq!(app.lib_visible(), [0, 2]);
        app.set_lib_filter("order cere".into());
        assert_eq!(app.lib_visible(), [2]);
        app.set_lib_filter("substance".into());
        assert_eq!(app.lib_visible(), [1]);
        // A new scan refilters with the current filter.
        app.set_library(vec![t("x"), t("Substance 1987")]);
        assert_eq!(app.lib_visible(), [1]);
        app.set_lib_filter(String::new());
        assert_eq!(app.lib_visible(), [0, 1]);
    }

    #[test]
    fn undo_keeps_playing_song_current() {
        let old = vec![t("a"), t("b"), t("c")];
        // The old current song is still playing.
        let (q, c) = restore_queue(old.clone(), Some(1), Some(&t("b")));
        assert_eq!((ids(&q), c), (ids(&old), Some(1)));
        // Playing a song that was elsewhere in the old queue.
        let (q, c) = restore_queue(old.clone(), Some(0), Some(&t("c")));
        assert_eq!((ids(&q), c), (ids(&old), Some(2)));
        // A song from the replacing list keeps playing, slotted after the old current.
        let (q, c) = restore_queue(old.clone(), Some(1), Some(&t("x")));
        assert_eq!((ids(&q), c), (vec!["a".into(), "b".into(), "x".into(), "c".into()], Some(2)));
        // Nothing playing.
        let (q, c) = restore_queue(old.clone(), Some(1), None);
        assert_eq!((ids(&q), c), (ids(&old), None));
    }

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
    fn discover_enter_plays_the_shelf() {
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

    #[test]
    fn lastfm_connect_flow() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        let err = |code| Err(lastfm::LfmError { code, msg: format!("Last.fm: error {code}") });
        app.lfm_auth = Some(("tok".into(), "https://www.last.fm/api/auth/?api_key=k&token=tok".into()));
        // Not approved yet: keep the token, Enter checks again.
        app.lfm_busy = true;
        app.handle_msg(Msg::LfmSession(app.lfm_seq, err(14)));
        assert!(app.lfm_auth.is_some() && !app.lfm_busy);
        // Expired or invalid: start over instead of asking again forever.
        for code in [15, 4] {
            app.lfm_auth = Some(("tok".into(), "url".into()));
            app.handle_msg(Msg::LfmSession(app.lfm_seq, err(code)));
            assert!(app.lfm_auth.is_none(), "error {code}");
        }
        // While a request is out, Enter doesn't send another (or open another tab).
        app.cfg.lastfm_api_key = "k".into();
        app.cfg.lastfm_api_secret = "s".into();
        app.lfm_busy = true;
        app.lastfm_step();
        assert!(app.status.starts_with("Still waiting"));
        // Esc cancels; a reply to the cancelled attempt is ignored.
        let seq = app.lfm_seq;
        app.view = View::Settings;
        app.run(Act::Back);
        assert!(!app.lfm_busy);
        app.handle_msg(Msg::LfmToken(seq, Ok("late".into())));
        assert!(app.lfm_auth.is_none());
    }
}
