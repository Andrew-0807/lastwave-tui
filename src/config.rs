use crate::{eq::EqState, model::Track};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mpv_path: String,
    /// yt-dlp format selector handed to mpv's ytdl hook.
    pub ytdl_format: String,
    pub audio_device: String,
    /// WASAPI exclusive / bit-perfect: drops EQ, ReplayGain and resampling.
    pub exclusive: bool,
    pub replaygain: String,
    pub gapless: bool,
    pub volume: f64,
    pub radio: bool,
    pub library_dir: String,
    pub eq: EqState,
    pub lastfm_api_key: String,
    pub lastfm_api_secret: String,
    pub lastfm_session: String,
    pub lastfm_user: String,
    pub extra_mpv_args: Vec<String>,
    /// Details drawer (cover + info) beside or below the lists.
    pub show_details: bool,
    /// Display name of the signed-in YouTube Music account.
    pub yt_user: String,
    /// Hand the account cookies to yt-dlp (Premium formats, age-gated songs).
    pub yt_account_playback: bool,
    /// Cover rendering: auto | kitty | iterm2 | sixel | halfblocks.
    pub image_protocol: String,
    /// Skip songs by known AI-generated artists when advancing.
    pub skip_ai: bool,
    /// Leave AI-generated songs out of Discover (Quick picks etc.).
    pub hide_ai: bool,
    /// Artists the user flagged as AI (normalised names).
    pub ai_flagged: Vec<String>,
    /// Artists on the community list the user unflagged.
    pub ai_allowed: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mpv_path: "mpv".into(),
            ytdl_format: YTDL_FORMATS[0].0.into(),
            audio_device: "auto".into(),
            exclusive: false,
            replaygain: "no".into(),
            gapless: true,
            volume: 100.0,
            radio: true,
            library_dir: dirs::audio_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            eq: EqState::default(),
            lastfm_api_key: String::new(),
            lastfm_api_secret: String::new(),
            lastfm_session: String::new(),
            lastfm_user: String::new(),
            extra_mpv_args: Vec::new(),
            show_details: true,
            yt_user: String::new(),
            yt_account_playback: false,
            image_protocol: "auto".into(),
            skip_ai: false,
            hide_ai: false,
            ai_flagged: Vec::new(),
            ai_allowed: Vec::new(),
        }
    }
}

/// (selector, label)
pub const YTDL_FORMATS: &[(&str, &str)] = &[
    ("bestaudio/best", "Best available"),
    ("bestaudio[acodec=opus]/bestaudio/best", "Prefer Opus"),
    ("bestaudio[ext=m4a]/bestaudio/best", "Prefer AAC (m4a)"),
];

pub const REPLAYGAIN: &[&str] = &["no", "track", "album"];

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub queue: Vec<Track>,
    pub cur: Option<usize>,
}

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("lastwave-tui")
}

pub fn config_path() -> PathBuf {
    dir().join("config.json")
}

/// A file that exists but doesn't parse is kept as `<name>.bak` so the
/// next save doesn't silently wipe it.
pub fn load<T: Default + for<'de> Deserialize<'de>>(name: &str) -> T {
    let path = dir().join(name);
    let Ok(s) = fs::read_to_string(&path) else { return T::default() };
    serde_json::from_str(&s).unwrap_or_else(|e| {
        let bak = dir().join(format!("{name}.bak"));
        let _ = fs::rename(&path, &bak);
        eprintln!("lastwave-tui: {} is unreadable ({e}); moved to {}", path.display(), bak.display());
        T::default()
    })
}

pub fn save<T: Serialize>(name: &str, v: &T) {
    let _ = fs::create_dir_all(dir());
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = fs::write(dir().join(name), s);
    }
}

impl Config {
    pub fn load() -> Self {
        load("config.json")
    }
    pub fn save(&self) {
        save("config.json", self)
    }
}

impl State {
    pub fn load() -> Self {
        load("state.json")
    }
    pub fn save(&self) {
        save("state.json", self)
    }
}

/// The signed-in YouTube Music cookies (plain text, like a browser profile).
fn auth_path() -> PathBuf {
    dir().join("yt_auth.txt")
}

/// Netscape copy of the same cookies for yt-dlp.
pub fn cookies_txt_path() -> PathBuf {
    dir().join("yt_cookies.txt")
}

pub fn load_cookies() -> Option<String> {
    fs::read_to_string(auth_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn save_cookies(cookies: Option<&str>) {
    let _ = fs::create_dir_all(dir());
    match cookies {
        Some(c) => {
            let _ = fs::write(auth_path(), c);
            let _ = fs::write(cookies_txt_path(), crate::ytm::netscape(c));
        }
        None => {
            let _ = fs::remove_file(auth_path());
            let _ = fs::remove_file(cookies_txt_path());
        }
    }
}

/// mpv `ytdl-raw-options` value: account cookies for yt-dlp when enabled.
pub fn ytdl_raw_options(cfg: &Config) -> String {
    let path = cookies_txt_path();
    if cfg.yt_account_playback && path.is_file() {
        format!("cookies={}", path.to_string_lossy())
    } else {
        String::new()
    }
}
