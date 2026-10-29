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
    let exact = req.call().ok().and_then(|r| r.into_json::<Value>().ok());
    if let Some(l) = exact.filter(|v| matches(t, &title, &artist, v)).and_then(|v| from_json(&v)) {
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
    // Only hits that are plausibly this song; better no lyrics than wrong ones.
    let good: Vec<&Value> = hits.iter().filter(|v| matches(t, &title, &artist, v)).collect();
    let synced = |v: &&Value| v["syncedLyrics"].as_str().is_some_and(|s| !s.is_empty());
    good.iter().copied().find(synced).or(good.first().copied()).and_then(from_json)
}

/// Whether an LRCLIB record is this track: similar title and artist, and the
/// same length (±5 s) when both durations are known.
fn matches(t: &Track, title: &str, artist: &str, v: &Value) -> bool {
    let close = match v["duration"].as_f64() {
        Some(d) if t.duration > 0 && d > 0.0 => (d - t.duration as f64).abs() <= 5.0,
        _ => true,
    };
    let hit_title = v["trackName"].as_str().unwrap_or_default();
    let hit_artist = v["artistName"].as_str().unwrap_or_default();
    // Without our own artist there is nothing to compare against.
    let unknown = artist.is_empty() || artist.eq_ignore_ascii_case("unknown artist");
    close
        && title_similarity(title, hit_title) >= 0.7
        && (unknown || coverage(&words(&primary_artist(artist)), &words(hit_artist)) >= 0.5)
}

/// Lower-cased alphanumeric words, "(Remastered)"-style noise removed.
fn words(s: &str) -> Vec<String> {
    clean_title(s)
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Share of `a`'s words also in `b`.
fn coverage(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() {
        return 0.0;
    }
    a.iter().filter(|w| b.contains(w)).count() as f64 / a.len() as f64
}

/// Dice similarity of the word sets, also tried on the part before " - " or
/// a bracket ("Song - 2011 Remaster", "Song (Remastered 2011)").
fn title_similarity(a: &str, b: &str) -> f64 {
    let dice = |a: &str, b: &str| {
        let (a, b) = (words(a), words(b));
        if a.is_empty() || b.is_empty() {
            return 0.0;
        }
        2.0 * a.iter().filter(|w| b.contains(w)).count() as f64 / (a.len() + b.len()) as f64
    };
    let head = |s: &'_ str| {
        let cut = [s.find(" - "), s.find(['(', '['])].into_iter().flatten().min().unwrap_or(s.len());
        s[..cut].to_string()
    };
    dice(a, b).max(dice(&head(a), &head(b)))
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
    // `[offset:+/-ms]`: a positive offset shows the lyrics earlier.
    let mut offset = 0.0;
    for line in s.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            if let Some(t) = parse_stamp(tag) {
                stamps.push(t);
            } else if let Some((k, v)) = tag.split_once(':') {
                if k.trim().eq_ignore_ascii_case("offset") {
                    offset = v.trim().parse::<f64>().map_or(offset, |ms| ms / 1000.0);
                }
            }
            rest = &stripped[end + 1..];
        }
        for t in stamps {
            out.push((t, rest.trim().to_string()));
        }
    }
    for line in &mut out {
        line.0 = (line.0 - offset).max(0.0);
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn parse_stamp(s: &str) -> Option<f64> {
    let (m, sec) = s.split_once(':')?;
    Some(m.trim().parse::<f64>().ok()? * 60.0 + sec.trim().parse::<f64>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;
    use serde_json::json;

    #[test]
    fn lrc_offset_shifts_timestamps() {
        let lrc = "[ar:Someone]\n[offset:+500]\n[00:01.00]one\n[00:10.50][00:20.00]two\n[00:00.20]zero";
        let l = parse_lrc(lrc);
        assert_eq!(l, vec![(0.0, "zero".into()), (0.5, "one".into()), (10.0, "two".into()), (19.5, "two".into())]);
        let late = parse_lrc("[offset:-250]\n[00:01.00]x");
        assert_eq!(late, vec![(1.25, "x".into())]);
        assert_eq!(parse_lrc("[00:02.00]y"), vec![(2.0, "y".into())]);
    }

    #[test]
    fn rejects_wrong_song() {
        let t = Track {
            source: Source::Yt("x".into()),
            title: "Bohemian Rhapsody (Remastered 2011)".into(),
            artist: "Queen".into(),
            album: String::new(),
            duration: 355,
            quality: String::new(),
            art: String::new(),
        };
        let (title, artist) = (clean_title(&t.title), t.artist.clone());
        let hit = |n: &str, a: &str, d: f64| json!({"trackName": n, "artistName": a, "duration": d});
        assert!(matches(&t, &title, &artist, &hit("Bohemian Rhapsody", "Queen", 357.0)));
        assert!(matches(&t, &title, &artist, &hit("Bohemian Rhapsody - Remastered 2011", "Queen", 354.0)));
        assert!(!matches(&t, &title, &artist, &hit("Bohemian Rhapsody", "Queen", 380.0)));
        assert!(!matches(&t, &title, &artist, &hit("Bohemian Rhapsody", "Panic! At The Disco", 355.0)));
        assert!(!matches(&t, &title, &artist, &hit("Radio Ga Ga", "Queen", 355.0)));
    }
}
