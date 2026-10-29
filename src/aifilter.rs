//! Known AI-generated artists, so playback can skip their songs.
//!
//! The list is the community-maintained one from CennoxX/spotify-ai-blocker
//! (MIT), cached in the config dir and refreshed once a day. Matching is by
//! artist name, so it works for YouTube Music tracks too.

use crate::{
    config,
    model::{primary_artist, strip_topic, Track, CREDIT_SEPS},
};
use std::{
    collections::HashSet,
    fs,
    sync::RwLock,
    time::{Duration, SystemTime},
};

const URL: &str = "https://raw.githubusercontent.com/CennoxX/spotify-ai-blocker/main/SpotifyAiArtists.csv";
const FILE: &str = "ai_artists.csv";
const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

static LIST: RwLock<Option<HashSet<String>>> = RwLock::new(None);

pub fn norm(name: &str) -> String {
    strip_topic(name.trim()).trim().to_lowercase()
}

/// Every credited name: the whole string plus each "A, B & C" part.
fn credited(artist: &str) -> Vec<String> {
    let mut names = vec![norm(artist)];
    let mut parts = vec![norm(artist)];
    for sep in CREDIT_SEPS {
        parts = parts.iter().flat_map(|p| p.split(sep).map(str::to_string).collect::<Vec<_>>()).collect();
    }
    names.extend(parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    names
}

fn parse(csv: &str) -> HashSet<String> {
    csv.lines()
        .skip(1)
        .filter_map(|l| l.rsplit_once(',').map(|(name, _id)| norm(name)))
        .filter(|n| !n.is_empty())
        .collect()
}

fn set(list: HashSet<String>) -> usize {
    let n = list.len();
    if let Ok(mut l) = LIST.write() {
        *l = Some(list);
    }
    n
}

/// Number of listed artists (0 until loaded).
pub fn count() -> usize {
    LIST.read().ok().and_then(|l| l.as_ref().map(HashSet::len)).unwrap_or(0)
}

/// Load the cached copy; true when it is missing or older than a day.
pub fn load_cached() -> bool {
    let path = config::dir().join(FILE);
    if let Ok(s) = fs::read_to_string(&path) {
        set(parse(&s));
    }
    let age = fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    age.is_none_or(|a| a > MAX_AGE)
}

/// Download the current list and cache it. Blocking.
pub fn refresh() -> Result<usize, String> {
    let body = crate::ytm::agent()
        .get(URL)
        .call()
        .map_err(|e| format!("AI artist list download failed: {e}"))?
        .into_string()
        .map_err(|e| format!("AI artist list download failed: {e}"))?;
    let list = parse(&body);
    if list.is_empty() {
        return Err("AI artist list download was empty".into());
    }
    let _ = fs::create_dir_all(config::dir());
    let _ = fs::write(config::dir().join(FILE), &body);
    Ok(set(list))
}

/// On the community list.
pub fn listed(name: &str) -> bool {
    LIST.read().is_ok_and(|l| l.as_ref().is_some_and(|l| l.contains(name)))
}

/// Flagged by the user, or listed by the community and not allowed back.
pub fn is_ai(t: &Track, flagged: &[String], allowed: &[String]) -> bool {
    hit(&credited(&t.artist), flagged, allowed, listed)
}

/// Same check for a bare artist name (album and artist cards).
pub fn is_ai_artist(artist: &str, flagged: &[String], allowed: &[String]) -> bool {
    hit(&credited(artist), flagged, allowed, listed)
}

fn hit(names: &[String], flagged: &[String], allowed: &[String], listed: impl Fn(&str) -> bool) -> bool {
    names.iter().any(|n| flagged.contains(n) || (listed(n) && !allowed.contains(n)))
}

/// Flag `artist`: credited names on the community list are taken off the
/// allowed list; when none are listed, the primary artist is flagged.
/// Returns the name(s) now treated as AI.
pub fn flag(artist: &str, flagged: &mut Vec<String>, allowed: &mut Vec<String>, listed: impl Fn(&str) -> bool) -> String {
    let names: Vec<String> = credited(artist).into_iter().filter(|n| listed(n)).collect();
    allowed.retain(|n| !names.contains(n));
    if !names.is_empty() {
        return names.join(", ");
    }
    let name = norm(&primary_artist(artist));
    if !flagged.contains(&name) {
        flagged.push(name.clone());
    }
    name
}

/// Unflag every credited name; listed ones are allowed back explicitly.
pub fn unflag(artist: &str, flagged: &mut Vec<String>, allowed: &mut Vec<String>, listed: impl Fn(&str) -> bool) {
    let names = credited(artist);
    flagged.retain(|n| !names.contains(n));
    for n in names.into_iter().filter(|n| listed(n)) {
        if !allowed.contains(&n) {
            allowed.push(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_split() {
        let c = credited("Foo Bar, Baz & Qux - Topic");
        assert!(c.contains(&"foo bar".to_string()));
        assert!(c.contains(&"baz".to_string()));
        assert!(c.contains(&"qux".to_string()));
        assert!(credited("Simon and Garfunkel").iter().all(|n| n == "simon and garfunkel"));
    }

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn precedence() {
        let listed = |n: &str| n == "bot";
        let names = credited("Human & Bot");
        assert!(hit(&names, &[], &[], listed));
        assert!(!hit(&names, &[], &v(&["bot"]), listed), "allowed beats the community list");
        assert!(hit(&names, &[], &v(&["human"]), listed), "allowing a collaborator doesn't clear the AI one");
        assert!(hit(&credited("Human"), &v(&["human"]), &v(&["human"]), |_| false), "flag beats allowed");
    }

    #[test]
    fn unflag_collab_allows_only_listed() {
        let listed = |n: &str| n == "bot";
        let (mut flagged, mut allowed) = (v(&["human", "other"]), Vec::new());
        unflag("Human & Bot", &mut flagged, &mut allowed, listed);
        assert_eq!(flagged, v(&["other"]));
        assert_eq!(allowed, v(&["bot"]));
        assert!(!hit(&credited("Bot"), &flagged, &allowed, listed));
        assert!(hit(&credited("Other feat. Human"), &flagged, &allowed, listed));
    }

    #[test]
    fn flag_collab_prefers_listed_name() {
        let listed = |n: &str| n == "bot";
        let (mut flagged, mut allowed) = (Vec::new(), v(&["bot"]));
        assert_eq!(flag("Real & Bot", &mut flagged, &mut allowed, listed), "bot");
        assert!(flagged.is_empty() && allowed.is_empty());
        assert_eq!(flag("Fake & Real - Topic", &mut flagged, &mut allowed, listed), "fake");
        flag("Fake", &mut flagged, &mut allowed, listed);
        assert_eq!(flagged, v(&["fake"]));
    }

    #[test]
    fn csv_names_with_commas() {
        let l = parse("artist,id\nAtomship,4Pb\nOne, Two,9xx\n");
        assert!(l.contains("atomship"));
        assert!(l.contains("one, two"));
    }
}
