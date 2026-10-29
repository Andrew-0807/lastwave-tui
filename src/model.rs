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

/// Separators between credited artists. " and " / " x " are left out:
/// they are part of too many real names ("Simon and Garfunkel").
pub const CREDIT_SEPS: &[&str] = &[", ", " & ", " feat. ", " ft. "];

/// `name` minus YouTube's " - Topic" suffix, any case.
pub fn strip_topic(name: &str) -> &str {
    let n = name.trim_end();
    match n.len().checked_sub(8) {
        Some(i) if n.is_char_boundary(i) && n[i..].eq_ignore_ascii_case(" - topic") => &n[..i],
        _ => n,
    }
}

/// First credited artist ("A, B & C" -> "A"), minus YouTube's " - Topic".
pub fn primary_artist(artist: &str) -> String {
    let a = strip_topic(artist);
    // ASCII lowercasing keeps byte offsets valid for slicing `a`.
    let lower = a.to_ascii_lowercase();
    let cut = CREDIT_SEPS.iter().filter_map(|sep| lower.find(sep)).min().unwrap_or(a.len());
    a[..cut].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_artist_credits() {
        assert_eq!(primary_artist("Simon and Garfunkel"), "Simon and Garfunkel");
        assert_eq!(primary_artist("Lil Nas X x Billy Ray"), "Lil Nas X x Billy Ray");
        assert_eq!(primary_artist("A & B - topic"), "A");
        assert_eq!(primary_artist("Foo Feat. Bar"), "Foo");
        // Uppercase non-ASCII whose lowercase form is longer in bytes.
        assert_eq!(primary_artist("İİİ Band, Other"), "İİİ Band");
        assert_eq!(primary_artist("Ⱥⱥ & Ǆ - Topic"), "Ⱥⱥ");
    }
}
