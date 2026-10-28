use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    Yt(String),
    Local(PathBuf),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub source: Source,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub album: String,
    /// Seconds; 0 when unknown.
    #[serde(default)]
    pub duration: u32,
    /// Local files only: "FLAC 24/96" style tag shown in lists.
    #[serde(default)]
    pub quality: String,
    /// Cover image URL (YouTube Music artwork); empty for local files.
    #[serde(default)]
    pub art: String,
}

impl Track {
    /// What mpv is given. YouTube links resolve through mpv's ytdl hook.
    pub fn url(&self) -> String {
        match &self.source {
            Source::Yt(id) => format!("https://music.youtube.com/watch?v={id}"),
            Source::Local(p) => p.to_string_lossy().into_owned(),
        }
    }

    /// Cache key for the cover: a URL, or `local:<path>` for embedded art.
    pub fn art_key(&self) -> String {
        match &self.source {
            _ if !self.art.is_empty() => self.art.clone(),
            Source::Yt(id) => format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
            Source::Local(p) => format!("local:{}", p.to_string_lossy()),
        }
    }

    pub fn video_id(&self) -> Option<&str> {
        match &self.source {
            Source::Yt(id) => Some(id),
            Source::Local(_) => None,
        }
    }
}

pub fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "3:45" / "1:02:03" -> seconds.
pub fn parse_duration(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || !s.contains(':') {
        return None;
    }
    let mut total = 0u32;
    for part in s.split(':') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        total = total * 60 + part.parse::<u32>().ok()?;
    }
    Some(total)
}

const NOISE: &[&str] = &[
    "official", "video", "audio", "lyric", "visualizer", "visualiser", "hd", "hq", "4k", "mv", "m/v",
];

/// Strip "(Official Video)"-style clauses so lyrics and scrobbles match.
pub fn clean_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut rest = title;
    while let Some(start) = rest.find(['(', '[']) {
        let close = if rest[start..].starts_with('(') { ')' } else { ']' };
        let Some(len) = rest[start..].find(close) else { break };
        let inner = rest[start + 1..start + len].to_lowercase();
        out.push_str(&rest[..start]);
        if !NOISE.iter().any(|w| inner.split(|c: char| !c.is_alphanumeric() && c != '/').any(|t| t == *w)) {
            out.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First credited artist ("A, B & C" -> "A"), minus YouTube's " - Topic".
pub fn primary_artist(artist: &str) -> String {
    let a = artist.trim_end_matches(" - Topic");
    let mut cut = a.len();
    for sep in [", ", " & ", " feat. ", " ft. ", " x ", " and "] {
        if let Some(i) = a.to_lowercase().find(sep) {
            cut = cut.min(i);
        }
    }
    a[..cut].trim().to_string()
}
