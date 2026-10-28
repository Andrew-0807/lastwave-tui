//! YouTube Music catalogue (search / browse / radio) over InnerTube as the
//! regular music.youtube.com web client. Stream resolution is NOT done here:
//! mpv plays the watch URL through its yt-dlp hook.
//!
//! Signing in uses the account's own browser cookies (pasted Cookie header
//! or cookies.txt), sent with the SAPISIDHASH Authorization header exactly
//! like LastWave's cookie login.

use crate::model::{parse_duration, Source, Track};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const API: &str = "https://music.youtube.com/youtubei/v1";
const ORIGIN: &str = "https://music.youtube.com";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";
const CLIENT_VERSION: &str = "1.20260707.12.00";
const MAX_PAGES: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Songs,
    Albums,
    Artists,
    Playlists,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Songs => "Songs",
            Kind::Albums => "Albums",
            Kind::Artists => "Artists",
            Kind::Playlists => "Playlists",
        }
    }
    pub fn next(self) -> Kind {
        match self {
            Kind::Songs => Kind::Albums,
            Kind::Albums => Kind::Artists,
            Kind::Artists => Kind::Playlists,
            Kind::Playlists => Kind::Songs,
        }
    }
    fn filter(self) -> &'static str {
        match self {
            Kind::Songs => "EgWKAQIIAWoKEAkQBRAKEAMQBA==",
            Kind::Albums => "EgWKAQIYAWoKEAkQBRAKEAMQBA==",
            Kind::Artists => "EgWKAQIgAWoKEAkQBRAKEAMQBA==",
            Kind::Playlists => "Eg-KAQwIABAAGAAgACgB",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Collection {
    pub title: String,
    pub subtitle: String,
    pub browse_id: String,
    pub art: String,
}

pub enum Results {
    Tracks(Vec<Track>),
    Collections(Vec<Collection>),
}

pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).user_agent(UA).build())
}

/// Normalised Cookie header of the signed-in account, if any.
static AUTH: RwLock<Option<String>> = RwLock::new(None);

pub fn set_auth(cookies: Option<String>) {
    if let Ok(mut a) = AUTH.write() {
        *a = cookies;
    }
}

pub fn signed_in() -> bool {
    AUTH.read().is_ok_and(|a| a.is_some())
}

fn cookie(cookies: &str, name: &str) -> Option<String> {
    cookies.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// `SAPISIDHASH <ts>_<sha1(ts + " " + sapisid + " " + origin)>`
fn sapisid_hash(cookies: &str) -> Option<String> {
    let sapisid = ["__Secure-3PAPISID", "SAPISID", "APISID"].iter().find_map(|n| cookie(cookies, n))?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let hash = Sha1::digest(format!("{ts} {sapisid} {ORIGIN}").as_bytes());
    Some(format!("SAPISIDHASH {ts}_{hash:x}"))
}

fn post(endpoint: &str, mut body: Value) -> Result<Value, String> {
    body["context"] = json!({"client": {
        "clientName": "WEB_REMIX", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US"
    }});
    let mut req = agent()
        .post(&format!("{API}/{endpoint}{}prettyPrint=false", if endpoint.contains('?') { '&' } else { '?' }))
        .set("Origin", ORIGIN)
        .set("X-Origin", ORIGIN)
        .set("Referer", "https://music.youtube.com/")
        .set("X-YouTube-Client-Name", "67")
        .set("X-YouTube-Client-Version", CLIENT_VERSION);
    if let Some(c) = AUTH.read().ok().and_then(|a| a.clone()) {
        if let Some(h) = sapisid_hash(&c) {
            req = req.set("Authorization", &h);
        }
        req = req.set("Cookie", &c).set("X-Goog-AuthUser", "0");
    }
    match req.send_json(body) {
        Ok(r) => r.into_json::<Value>().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(401 | 403, _)) if signed_in() => {
            Err("YouTube rejected the saved sign-in — paste fresh cookies in Settings".into())
        }
        Err(e) => Err(e.to_string()),
    }
}

pub fn search(query: &str, kind: Kind) -> Result<Results, String> {
    let q = query.trim();
    if let Some(r) = from_link(q) {
        return r;
    }
    let root = post("search", json!({"query": q, "params": kind.filter()}))?;
    Ok(match kind {
        Kind::Songs => Results::Tracks(parse_tracks(&root)),
        _ => Results::Collections(parse_collections(&root)),
    })
}

/// Pasted YouTube / YouTube Music links: playlists browse, videos start a radio.
fn from_link(q: &str) -> Option<Result<Results, String>> {
    if !(q.contains("youtube.com/") || q.contains("youtu.be/")) {
        return None;
    }
    if let Some(list) = query_param(q, "list") {
        if !list.starts_with("RD") {
            return Some(browse(&format!("VL{list}")).map(Results::Tracks));
        }
    }
    let id = query_param(q, "v").or_else(|| {
        q.split("youtu.be/").nth(1).and_then(|s| s.split(['?', '&', '/']).next()).map(str::to_string)
    })?;
    Some(watch_queue(&id).map(Results::Tracks))
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .map(|v| v.split('#').next().unwrap_or(v).to_string())
        .filter(|v| !v.is_empty())
}

/// Tracks of an album (MPRE…), playlist (VL…) or artist (UC…) page.
pub fn browse(browse_id: &str) -> Result<Vec<Track>, String> {
    let mut root = post("browse", json!({"browseId": browse_id}))?;
    let mut tracks = parse_tracks(&root);
    for _ in 0..MAX_PAGES {
        root = match next_page(&root) {
            Some(Ok(v)) => v,
            _ => break,
        };
        let more = parse_tracks(&root);
        if more.is_empty() {
            break;
        }
        tracks.extend(more);
    }
    dedupe(&mut tracks);
    Ok(tracks)
}

/// The watch queue YouTube Music builds for a video: the seed first, then radio.
pub fn watch_queue(video_id: &str) -> Result<Vec<Track>, String> {
    let root = post(
        "next",
        json!({"videoId": video_id, "playlistId": format!("RDAMVM{video_id}"), "params": "wAEB", "isAudioOnly": true}),
    )?;
    let mut tracks = parse_tracks(&root);
    dedupe(&mut tracks);
    Ok(tracks)
}

pub fn radio(video_id: &str) -> Result<Vec<Track>, String> {
    Ok(watch_queue(video_id)?.into_iter().filter(|t| t.video_id() != Some(video_id)).collect())
}

/// Next page of a browse response. `continuationCommand` tokens go in the
/// body; legacy `nextContinuationData` tokens go in the URL (`ctoken=`).
fn next_page(root: &Value) -> Option<Result<Value, String>> {
    let mut found = Vec::new();
    collect(root, "continuationCommand", &mut found);
    if let Some(t) = found.iter().find_map(|c| c["token"].as_str()) {
        return Some(post("browse", json!({"continuation": t})));
    }
    found.clear();
    collect(root, "nextContinuationData", &mut found);
    let t = found.iter().find_map(|c| c["continuation"].as_str())?;
    Some(post(&format!("browse?ctoken={t}&continuation={t}&type=next"), json!({})))
}

fn collect<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                if k == key {
                    out.push(child);
                } else {
                    collect(child, key, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| collect(c, key, out)),
        _ => {}
    }
}

fn dedupe(tracks: &mut Vec<Track>) {
    let mut seen = HashSet::new();
    tracks.retain(|t| seen.insert(t.url()));
}

fn text(v: &Value) -> String {
    match v["runs"].as_array() {
        Some(runs) => runs.iter().filter_map(|r| r["text"].as_str()).collect(),
        None => v["simpleText"].as_str().unwrap_or_default().to_string(),
    }
}

fn flex(r: &Value, i: usize) -> &Value {
    &r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"]
}

fn thumb(r: &Value) -> String {
    let list = r["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
        .as_array()
        .or_else(|| r["thumbnailRenderer"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"].as_array())
        .or_else(|| r["thumbnail"]["thumbnails"].as_array());
    list.and_then(|l| l.last()).and_then(|t| t["url"].as_str()).map(hi_res).unwrap_or_default()
}

/// Same upgrade as LastWave's highResolutionArtwork (smaller: terminals don't need 512px).
fn hi_res(url: &str) -> String {
    let u = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    if u.contains("googleusercontent.com") || u.contains("ggpht.com") {
        if let Some(i) = u.rfind('=') {
            return format!("{}=w360-h360-l90-rj", &u[..i]);
        }
    }
    u
}

fn browse_id(run: &Value) -> Option<&str> {
    run["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str()
}

const TYPE_WORDS: &[&str] = &["Song", "Video", "Episode", "Album", "Single", "EP", "Playlist", "Artist"];

/// Split "Artist • Album • 3:45" byline runs into (artist, album, duration).
fn byline(runs: &[Value]) -> (String, String, u32) {
    let mut segments: Vec<(String, Vec<&Value>)> = vec![(String::new(), Vec::new())];
    for run in runs {
        let t = run["text"].as_str().unwrap_or_default();
        if t.trim() == "•" {
            segments.push((String::new(), Vec::new()));
        } else {
            let seg = segments.last_mut().unwrap();
            seg.0.push_str(t);
            seg.1.push(run);
        }
    }
    let has = |seg: &(String, Vec<&Value>), prefix: &str| {
        seg.1.iter().any(|r| browse_id(r).is_some_and(|b| b.starts_with(prefix)))
    };
    let duration = segments.iter().find_map(|s| parse_duration(&s.0)).unwrap_or(0);
    let album = segments.iter().find(|s| has(s, "MPRE")).map(|s| s.0.trim().to_string()).unwrap_or_default();
    let artist = segments
        .iter()
        .find(|s| has(s, "UC"))
        .or_else(|| {
            segments.iter().find(|s| {
                let t = s.0.trim();
                !t.is_empty() && !TYPE_WORDS.contains(&t) && parse_duration(t).is_none() && !t.ends_with("views")
            })
        })
        .map(|s| s.0.trim().to_string())
        .unwrap_or_default();
    (artist, album, duration)
}

fn watch_id(r: &Value) -> Option<String> {
    [
        &r["playlistItemData"]["videoId"],
        &r["navigationEndpoint"]["watchEndpoint"]["videoId"],
        &r["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]
            ["playNavigationEndpoint"]["watchEndpoint"]["videoId"],
        &r["videoId"],
    ]
    .iter()
    .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
    .map(str::to_string)
}

fn parse_tracks(root: &Value) -> Vec<Track> {
    let mut out = Vec::new();
    let mut rows = Vec::new();
    collect(root, "musicResponsiveListItemRenderer", &mut rows);
    out.extend(rows.into_iter().filter_map(parse_row));
    let mut panel = Vec::new();
    collect(root, "playlistPanelVideoRenderer", &mut panel);
    out.extend(panel.into_iter().filter_map(parse_panel));
    out
}

fn parse_row(r: &Value) -> Option<Track> {
    let id = watch_id(r)?;
    let title = text(flex(r, 0)).trim().to_string();
    if title.is_empty() {
        return None;
    }
    let empty = Vec::new();
    let (artist, album, mut duration) = byline(flex(r, 1)["runs"].as_array().unwrap_or(&empty));
    if duration == 0 {
        let fixed = text(&r["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"]);
        duration = parse_duration(&fixed).unwrap_or(0);
    }
    if duration == 0 {
        for i in 2..5 {
            if let Some(d) = parse_duration(&text(flex(r, i))) {
                duration = d;
                break;
            }
        }
    }
    Some(Track { source: Source::Yt(id), title, artist, album, duration, quality: String::new(), art: thumb(r) })
}

fn parse_panel(r: &Value) -> Option<Track> {
    if r.get("unplayableText").is_some() {
        return None;
    }
    let id = watch_id(r)?;
    let title = text(&r["title"]).trim().to_string();
    if title.is_empty() {
        return None;
    }
    let by = if r["longBylineText"].is_object() { &r["longBylineText"] } else { &r["shortBylineText"] };
    let empty = Vec::new();
    let (artist, album, mut duration) = byline(by["runs"].as_array().unwrap_or(&empty));
    if duration == 0 {
        duration = parse_duration(&text(&r["lengthText"])).unwrap_or(0);
    }
    Some(Track { source: Source::Yt(id), title, artist, album, duration, quality: String::new(), art: thumb(r) })
}

fn parse_collections(root: &Value) -> Vec<Collection> {
    let mut rows = Vec::new();
    collect(root, "musicResponsiveListItemRenderer", &mut rows);
    let mut cards = Vec::new();
    collect(root, "musicTwoRowItemRenderer", &mut cards);
    let mut seen = HashSet::new();
    rows.into_iter()
        .filter_map(|r| {
            let id = r["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str()?.to_string();
            let title = text(flex(r, 0)).trim().to_string();
            (!title.is_empty()).then(|| Collection {
                title,
                subtitle: text(flex(r, 1)).trim().to_string(),
                browse_id: id,
                art: thumb(r),
            })
        })
        .chain(cards.into_iter().filter_map(two_row_collection))
        .filter(|c| seen.insert(c.browse_id.clone()))
        .collect()
}

/// Grid/carousel card ("musicTwoRowItemRenderer") pointing at a page.
fn two_row_collection(r: &Value) -> Option<Collection> {
    let nav = &r["navigationEndpoint"];
    let id = nav["browseEndpoint"]["browseId"]
        .as_str()
        .map(str::to_string)
        .or_else(|| nav["watchEndpoint"]["playlistId"].as_str().map(|p| format!("VL{p}")))?;
    let title = text(&r["title"]).trim().to_string();
    (!title.is_empty()).then(|| Collection { title, subtitle: text(&r["subtitle"]).trim().to_string(), browse_id: id, art: thumb(r) })
}

fn two_row_track(r: &Value) -> Option<Track> {
    let id = r["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str()?.to_string();
    let title = text(&r["title"]).trim().to_string();
    let empty = Vec::new();
    let (artist, album, duration) = byline(r["subtitle"]["runs"].as_array().unwrap_or(&empty));
    Some(Track { source: Source::Yt(id), title, artist, album, duration, quality: String::new(), art: thumb(r) })
}

/// Album rows often omit the artist; fill it from the album's subtitle.
pub fn fill_from_collection(tracks: &mut [Track], c: &Collection) {
    let is_album = c.browse_id.starts_with("MPRE");
    let artist = c
        .subtitle
        .split(" • ")
        .map(str::trim)
        .find(|p| !p.is_empty() && !TYPE_WORDS.contains(p) && !p.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or_default();
    for t in tracks {
        if t.artist.is_empty() {
            t.artist = if artist.is_empty() { "Unknown artist".into() } else { artist.to_string() };
        }
        if is_album && t.album.is_empty() {
            t.album = c.title.clone();
        }
        if is_album || t.art.is_empty() {
            t.art = c.art.clone();
        }
    }
}

// ---- account ---------------------------------------------------------------

/// Accepts a Cookie header (with or without `Cookie:`), a Netscape
/// cookies.txt body, a JSON cookie export, or a path to any of those.
/// Mirrors LastWave's `normalizeCookies`.
pub fn normalize_cookies(raw: &str) -> Result<String, String> {
    let mut input = raw.trim().trim_matches('"').to_string();
    if !input.contains('=') && std::path::Path::new(&input).is_file() {
        input = std::fs::read_to_string(&input).map_err(|e| e.to_string())?;
    }
    let mut pairs = BTreeMap::new();
    fn add(pairs: &mut BTreeMap<String, String>, n: &str, v: &str) {
        let (n, v) = (n.trim(), v.trim().trim_matches('"'));
        if !n.is_empty() && !v.is_empty() {
            pairs.insert(n.to_string(), v.to_string());
        }
    }
    if input.starts_with('[') || input.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(&input) {
            let list = v.as_array().or_else(|| v["cookies"].as_array()).cloned().unwrap_or_default();
            for c in &list {
                add(&mut pairs, c["name"].as_str().unwrap_or_default(), c["value"].as_str().unwrap_or_default());
            }
            if list.is_empty() {
                if let Some(m) = v.as_object() {
                    m.iter().for_each(|(k, v)| add(&mut pairs, k, v.as_str().unwrap_or_default()));
                }
            }
        }
    }
    if pairs.is_empty() {
        for line in input.lines() {
            let mut line = line.trim();
            if let Some(rest) = line.strip_prefix("#HttpOnly_") {
                line = rest;
            } else if line.starts_with('#') {
                continue;
            }
            if line.len() >= 7 && line[..7].eq_ignore_ascii_case("cookie:") {
                line = line[7..].trim();
            }
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 7 {
                add(&mut pairs, cols[5], cols[6]);
                continue;
            }
            for part in line.split(';') {
                if let Some((n, v)) = part.split_once('=') {
                    add(&mut pairs, n, v);
                }
            }
        }
    }
    if pairs.is_empty() {
        return Err("No cookies found — paste the Cookie header or a cookies.txt path".into());
    }
    if !["__Secure-3PAPISID", "SAPISID", "APISID"].iter().any(|k| pairs.contains_key(*k)) {
        return Err("Missing SAPISID — copy the cookies from music.youtube.com while signed in".into());
    }
    Ok(pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
}

/// Cookie header -> Netscape cookies.txt (for yt-dlp's --cookies).
pub fn netscape(cookies: &str) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for part in cookies.split(';') {
        if let Some((n, v)) = part.split_once('=') {
            out.push_str(&format!(".youtube.com\tTRUE\t/\tTRUE\t0\t{}\t{}\n", n.trim(), v.trim()));
        }
    }
    out
}

/// Display name of the signed-in account (verifies the cookies work).
pub fn account_name() -> Result<String, String> {
    let root = post("account/account_menu", json!({}))?;
    let mut headers = Vec::new();
    collect(&root, "activeAccountHeaderRenderer", &mut headers);
    let h = headers.first().ok_or("YouTube didn't accept these cookies (signed out?)")?;
    let name = text(&h["accountName"]);
    let handle = text(&h["channelHandle"]);
    Ok(if handle.is_empty() { name } else { format!("{name} ({handle})") })
}

/// Liked Music + the account's saved/created playlists.
pub fn library_playlists() -> Result<Vec<Collection>, String> {
    let root = post("browse", json!({"browseId": "FEmusic_liked_playlists"}))?;
    let mut lists: Vec<Collection> = parse_collections(&root)
        .into_iter()
        .filter(|c| c.browse_id.starts_with("VL") || c.browse_id.starts_with("PL"))
        .collect();
    let liked = |c: &Collection| c.browse_id == "VLLM";
    if !lists.iter().any(liked) {
        lists.insert(0, Collection { title: "Liked Music".into(), subtitle: "Auto playlist".into(), browse_id: "VLLM".into(), art: String::new() });
    } else if let Some(i) = lists.iter().position(liked) {
        let c = lists.remove(i);
        lists.insert(0, c);
    }
    Ok(lists)
}

// ---- discover ----------------------------------------------------------------

pub enum Entry {
    Track(Track),
    Collection(Collection),
}

pub struct Shelf {
    pub title: String,
    pub items: Vec<Entry>,
}

/// YouTube Music home (personalised when signed in), as titled shelves.
pub fn home() -> Result<Vec<Shelf>, String> {
    let mut root = post("browse", json!({"browseId": "FEmusic_home"}))?;
    let mut shelves = parse_shelves(&root);
    for _ in 0..2 {
        root = match next_page(&root) {
            Some(Ok(v)) => v,
            _ => break,
        };
        // Without a session YouTube ignores the token and resends page 1.
        if root.get("continuationContents").is_none() {
            break;
        }
        let more = parse_shelves(&root);
        if more.is_empty() {
            break;
        }
        shelves.extend(more);
    }
    Ok(shelves)
}

fn parse_shelves(root: &Value) -> Vec<Shelf> {
    let mut lists = Vec::new();
    collect(root, "sectionListRenderer", &mut lists);
    collect(root, "sectionListContinuation", &mut lists);
    let mut out = Vec::new();
    for list in lists {
        for section in list["contents"].as_array().into_iter().flatten() {
            if let Some(shelf) = parse_shelf(section) {
                out.push(shelf);
            }
        }
    }
    out
}

fn parse_shelf(section: &Value) -> Option<Shelf> {
    if section["musicShelfRenderer"].is_object() {
        let s = &section["musicShelfRenderer"];
        let items: Vec<Entry> = parse_tracks(s).into_iter().map(Entry::Track).collect();
        return (!items.is_empty()).then(|| Shelf { title: text(&s["title"]), items });
    }
    let c = &section["musicCarouselShelfRenderer"];
    if !c.is_object() {
        return None;
    }
    let h = &c["header"]["musicCarouselShelfBasicHeaderRenderer"];
    let mut title = text(&h["title"]);
    let strap = text(&h["strapline"]);
    if !strap.is_empty() {
        title = format!("{title} · {strap}");
    }
    let mut items = Vec::new();
    for item in c["contents"].as_array().into_iter().flatten() {
        if let Some(t) = item.get("musicResponsiveListItemRenderer").and_then(parse_row) {
            items.push(Entry::Track(t));
        } else if let Some(r) = item.get("musicTwoRowItemRenderer") {
            let has_video = r["navigationEndpoint"]["watchEndpoint"]["videoId"].is_string();
            if has_video {
                items.extend(two_row_track(r).map(Entry::Track));
            } else if let Some(c) = two_row_collection(r).filter(|c| c.browse_id != "VLLM") {
                items.push(Entry::Collection(c));
            }
        }
    }
    (!items.is_empty()).then_some(Shelf { title, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header() {
        let c = normalize_cookies("Cookie: SID=abc; __Secure-3PAPISID=xyz/1; PREF=f6=40000000").unwrap();
        assert_eq!(c, "PREF=f6=40000000; SID=abc; __Secure-3PAPISID=xyz/1");
        assert!(sapisid_hash(&c).unwrap().starts_with("SAPISIDHASH "));
    }

    #[test]
    fn netscape_file_roundtrip() {
        let txt = "# Netscape HTTP Cookie File\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tq\n";
        let c = normalize_cookies(txt).unwrap();
        assert_eq!(c, "SAPISID=q; SID=abc");
        assert_eq!(normalize_cookies(&netscape(&c)).unwrap(), c);
    }

    #[test]
    fn json_export_and_missing_sapisid() {
        let c = normalize_cookies(r#"[{"name":"SAPISID","value":"q"},{"name":"SID","value":"\"a\""}]"#).unwrap();
        assert_eq!(c, "SAPISID=q; SID=a");
        assert!(normalize_cookies("SID=abc; HSID=def").unwrap_err().contains("SAPISID"));
    }
}
