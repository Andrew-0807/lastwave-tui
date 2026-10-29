use crate::{eq::EqState, model::Track};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

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

/// Parses `s`, keeping every top-level field that is valid on its own: a
/// field of the wrong type falls back to its default instead of resetting
/// the whole file. The flag is true when something had to be dropped.
fn parse_tolerant<T: Default + Serialize + for<'de> Deserialize<'de>>(s: &str) -> (T, bool) {
    let Ok(file) = serde_json::from_str::<Value>(s) else { return (T::default(), true) };
    if let Ok(v) = serde_json::from_value(file.clone()) {
        return (v, false);
    }
    let (Value::Object(fields), Ok(Value::Object(mut base))) = (file, serde_json::to_value(T::default())) else {
        return (T::default(), true);
    };
    for (k, v) in fields {
        let old = base.insert(k.clone(), v);
        if serde_json::from_value::<T>(Value::Object(base.clone())).is_err() {
            match old {
                Some(o) => base.insert(k, o),
                None => base.remove(&k),
            };
        }
    }
    (serde_json::from_value(Value::Object(base)).unwrap_or_default(), true)
}

/// A file that exists but doesn't fully parse is copied to `<name>.bak`
/// first, so the next save can't silently lose what was dropped.
pub fn load<T: Default + Serialize + for<'de> Deserialize<'de>>(name: &str) -> T {
    load_path(&dir().join(name))
}

pub fn save<T: Serialize>(name: &str, v: &T) {
    save_path(&dir().join(name), v)
}

fn load_path<T: Default + Serialize + for<'de> Deserialize<'de>>(path: &Path) -> T {
    let Ok(s) = fs::read_to_string(path) else { return T::default() };
    let (v, lossy) = parse_tolerant(&s);
    if lossy {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bak = path.with_file_name(format!("{name}.bak"));
        let _ = write_private(&bak, s.as_bytes());
        eprintln!("lastwave-tui: {} had invalid values (reset to defaults); original kept as {}", path.display(), bak.display());
    }
    v
}

fn save_path<T: Serialize>(path: &Path, v: &T) {
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = write_private(path, s.as_bytes());
    }
}

/// Scrobbles not yet accepted by Last.fm: (track, start time), oldest first.
pub type PendingScrobbles = Vec<(Track, u64)>;

fn scrobbles_path() -> PathBuf {
    dir().join("scrobbles.json")
}

/// The scrobbles left over from the last run, at most `max` (the newest).
pub fn load_scrobbles(max: usize) -> PendingScrobbles {
    load_scrobbles_from(&scrobbles_path(), max)
}

pub fn save_scrobbles(items: &[(Track, u64)]) {
    save_path(&scrobbles_path(), &items)
}

fn load_scrobbles_from(path: &Path, max: usize) -> PendingScrobbles {
    let mut v: PendingScrobbles = load_path(path);
    if v.len() > max {
        v.drain(..v.len() - max);
    }
    v
}

/// Creates the folder `path` goes in (0700 on unix: the config dir holds
/// session keys and cookies).
fn ensure_dir(path: &Path) -> std::io::Result<()> {
    let d = path.parent().map(Path::to_path_buf).unwrap_or_else(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(&d)?;
    Ok(())
}

/// Atomic, owner-only write: temp file in the same dir, synced, then renamed
/// over the target, so a crash mid-save never leaves a half-written file.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    ensure_dir(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
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
    match cookies {
        Some(c) => {
            let _ = write_private(&auth_path(), c.as_bytes());
            let _ = write_private(&cookies_txt_path(), crate::ytm::netscape(c).as_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_bad_field_keeps_the_rest() {
        let s = r#"{"volume": "loud", "lastfm_session": "sk-123", "lastfm_user": "me", "gapless": false}"#;
        let (c, lossy): (Config, bool) = parse_tolerant(s);
        assert!(lossy);
        assert_eq!(c.volume, Config::default().volume);
        assert_eq!(c.lastfm_session, "sk-123");
        assert_eq!(c.lastfm_user, "me");
        assert!(!c.gapless);
    }

    #[test]
    fn scrobble_queue_round_trip() {
        use crate::model::Source;
        let d = std::env::temp_dir().join(format!("lastwave-tui-test-{}", std::process::id()));
        let path = d.join("scrobbles.json");
        let t = |n: &str| Track {
            source: Source::Yt(n.into()),
            title: n.into(),
            artist: "a".into(),
            album: String::new(),
            duration: 200,
            quality: String::new(),
            art: String::new(),
        };
        let items = vec![(t("one"), 100), (t("two"), 200), (t("three"), 300)];
        save_path(&path, &items);
        let back = load_scrobbles_from(&path, 1000);
        assert_eq!(back.iter().map(|(t, ts)| (t.title.as_str(), *ts)).collect::<Vec<_>>(), [("one", 100), ("two", 200), ("three", 300)]);
        assert_eq!(back[0].0.source, items[0].0.source);
        // Over the cap: the newest are kept.
        let back = load_scrobbles_from(&path, 2);
        assert_eq!(back.iter().map(|(_, ts)| *ts).collect::<Vec<_>>(), [200, 300]);
        // Missing file: nothing pending.
        assert!(load_scrobbles_from(&d.join("missing.json"), 10).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn valid_and_broken_files() {
        let (c, lossy): (Config, bool) = parse_tolerant(r#"{"lastfm_session": "x"}"#);
        assert!(!lossy);
        assert_eq!(c.lastfm_session, "x");
        let (c, lossy): (Config, bool) = parse_tolerant("{not json");
        assert!(lossy);
        assert_eq!(c.mpv_path, "mpv");
    }
}
