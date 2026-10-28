//! Synced lyrics from LRCLIB (LastWave's primary lyrics source).

use crate::{
    model::{clean_title, primary_artist, Track},
    ytm::agent,
};
use serde_json::Value;

const UA: &str = "lastwave-tui/0.1 (https://github.com/Clash-Projects/LastWave-Desktop)";

pub struct Lyrics {
    /// (seconds, line), sorted.
    pub synced: Vec<(f64, String)>,
    pub plain: Vec<String>,
}

pub fn fetch(t: &Track) -> Option<Lyrics> {
    let title = clean_title(&t.title);
    let artist = t.artist.trim_end_matches(" - Topic").to_string();

    let mut req = agent()
        .get("https://lrclib.net/api/get")
        .set("User-Agent", UA)
        .query("track_name", &title)
        .query("artist_name", &artist);
    if !t.album.is_empty() {
        req = req.query("album_name", &t.album);
    }
    if t.duration > 0 {
        req = req.query("duration", &t.duration.to_string());
    }
    if let Some(l) = req.call().ok().and_then(|r| r.into_json::<Value>().ok()).and_then(|v| from_json(&v)) {
        return Some(l);
    }

    let hits: Vec<Value> = agent()
        .get("https://lrclib.net/api/search")
        .set("User-Agent", UA)
        .query("track_name", &title)
        .query("artist_name", &primary_artist(&artist))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let close = |v: &Value| {
        t.duration == 0 || v["duration"].as_f64().is_some_and(|d| (d - t.duration as f64).abs() <= 5.0)
    };
    let synced = |v: &Value| v["syncedLyrics"].as_str().is_some_and(|s| !s.is_empty());
    hits.iter()
        .find(|v| synced(v) && close(v))
        .or_else(|| hits.iter().find(|v| close(v)))
        .or_else(|| hits.iter().find(|v| synced(v)))
        .or(hits.first())
        .and_then(from_json)
}

fn from_json(v: &Value) -> Option<Lyrics> {
    if v["instrumental"].as_bool() == Some(true) {
        return Some(Lyrics { synced: Vec::new(), plain: vec!["♪ Instrumental ♪".into()] });
    }
    let synced = v["syncedLyrics"].as_str().map(parse_lrc).unwrap_or_default();
    let plain: Vec<String> = v["plainLyrics"].as_str().unwrap_or_default().lines().map(str::to_string).collect();
    (!synced.is_empty() || !plain.is_empty()).then_some(Lyrics { synced, plain })
}

fn parse_lrc(s: &str) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            if let Some(t) = parse_stamp(&stripped[..end]) {
                stamps.push(t);
            }
            rest = &stripped[end + 1..];
        }
        for t in stamps {
            out.push((t, rest.trim().to_string()));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn parse_stamp(s: &str) -> Option<f64> {
    let (m, sec) = s.split_once(':')?;
    Some(m.trim().parse::<f64>().ok()? * 60.0 + sec.trim().parse::<f64>().ok()?)
}
