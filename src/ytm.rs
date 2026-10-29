//! YouTube Music catalogue (search / browse / radio) over InnerTube as the
//! regular music.youtube.com web client. Stream resolution is NOT done here:
//! mpv plays the watch URL through its yt-dlp hook.
//!
//! Signing in uses the account's own browser cookies (pasted Cookie header
//! or cookies.txt), sent with the SAPISIDHASH Authorization header exactly
//! like LastWave's cookie login.

use crate::model::{clean_title, parse_duration, primary_artist, Source, Track};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const API: &str = "https://music.youtube.com/youtubei/v1";
const ORIGIN: &str = "https://music.youtube.com";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";
const CLIENT_VERSION: &str = "1.20260707.12.00";
const MAX_PAGES: usize = 20;
/// Upper bound on tracks loaded from one album/playlist page (see `browse`).
const MAX_TRACKS: usize = 10_000;

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
        // Cookies saved by an older build may hold bytes a header can't carry.
        if !c.split("; ").all(valid_cookie) {
            return Err("The saved YouTube cookies are malformed — paste them again in Settings".into());
        }
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
    parse_search(&root, kind)
}

fn parse_search(root: &Value, kind: Kind) -> Result<Results, String> {
    let (results, n) = match kind {
        Kind::Songs => {
            let t = parse_tracks(root);
            let n = t.len();
            (Results::Tracks(t), n)
        }
        _ => {
            let c = parse_collections(root);
            let n = c.len();
            (Results::Collections(c), n)
        }
    };
    if n == 0 && !recognised_empty(root) {
        return Err("YouTube response format not recognised — the app may need an update".into());
    }
    Ok(results)
}

/// Whether a response that parsed to nothing is a genuine "no results" page
/// rather than a layout we no longer understand: it must have the usual
/// section list, and none of its shelves may hold items we failed to read.
fn recognised_empty(root: &Value) -> bool {
    let mut lists = Vec::new();
    collect(root, "sectionListRenderer", &mut lists);
    if lists.is_empty() {
        return false;
    }
    let mut shelves = Vec::new();
    for key in ["musicShelfRenderer", "musicCarouselShelfRenderer", "gridRenderer", "musicCardShelfRenderer"] {
        collect(root, key, &mut shelves);
    }
    let filled = |s: &&Value| ["contents", "items"].iter().any(|k| s[*k].as_array().is_some_and(|a| !a.is_empty()));
    !shelves.iter().any(filled)
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
    // Follow continuations until they run out. YouTube pages ~100 tracks, so
    // the page cap only matters as a guard against a token loop; past
    // MAX_TRACKS (YouTube's own playlist limit is 5,000, Liked Music can go
    // beyond) the rest of the list is deliberately not loaded.
    dedupe(&mut tracks);
    let mut seen: HashSet<String> = tracks.iter().map(Track::url).collect();
    for _ in 0..MAX_TRACKS / 50 {
        if tracks.len() >= MAX_TRACKS {
            tracks.truncate(MAX_TRACKS);
            break;
        }
        root = match next_page(&root) {
            None => break,
            Some(Ok(v)) => v,
            // Retry once; a half-loaded list must not pass for the whole one.
            Some(Err(_)) => match next_page(&root) {
                Some(Ok(v)) => v,
                Some(Err(e)) => return Err(format!("stopped after {} tracks: {e}", tracks.len())),
                None => break,
            },
        };
        let before = tracks.len();
        tracks.extend(parse_tracks(&root).into_iter().filter(|t| seen.insert(t.url())));
        // Nothing new: the list ended, or YouTube is resending a page.
        if tracks.len() == before {
            break;
        }
    }
    Ok(tracks)
}

/// The watch queue YouTube Music builds for a video: the seed first, then radio.
pub fn watch_queue(video_id: &str) -> Result<Vec<Track>, String> {
    Ok(radio_tracks(&next(video_id)?))
}

fn next(video_id: &str) -> Result<Value, String> {
    post(
        "next",
        json!({"videoId": video_id, "playlistId": format!("RDAMVM{video_id}"), "params": "wAEB", "isAudioOnly": true}),
    )
}

/// Queue entries of a `next` response. YouTube lists a song together with
/// its music-video counterpart (and sometimes the same id twice): keep one
/// per videoId, and for an audio track (ATV) / video (OMV, UGC) pair with the
/// same title and artist keep the audio track in the earlier slot.
fn radio_tracks(root: &Value) -> Vec<Track> {
    let mut rows = Vec::new();
    collect(root, "musicResponsiveListItemRenderer", &mut rows);
    let mut panel = Vec::new();
    collect(root, "playlistPanelVideoRenderer", &mut panel);
    let typed = rows
        .into_iter()
        .filter_map(|r| parse_row(r).map(|t| (t, video_type(r))))
        .chain(panel.into_iter().filter_map(|r| parse_panel(r).map(|t| (t, video_type(r)))));

    const ATV: &str = "MUSIC_VIDEO_TYPE_ATV";
    let mut ids = HashSet::new();
    let mut out: Vec<(Track, Option<&str>)> = Vec::new();
    let mut by_song: HashMap<String, usize> = HashMap::new();
    for (t, ty) in typed {
        if !ids.insert(t.url()) {
            continue;
        }
        let key = song_key(&t);
        let is_video = |ty: Option<&str>| ty.is_some_and(|ty| ty != ATV);
        match by_song.get(&key).copied() {
            // A counterpart pair: at least one side is known to be a video.
            Some(i) if is_video(ty) || is_video(out[i].1) => {
                if ty == Some(ATV) {
                    out[i] = (t, ty);
                }
            }
            _ => {
                by_song.entry(key).or_insert(out.len());
                out.push((t, ty));
            }
        }
    }
    out.into_iter().map(|(t, _)| t).collect()
}

fn video_type(r: &Value) -> Option<&str> {
    fn ep(root: &Value) -> Option<&str> {
        root["watchEndpoint"]["watchEndpointMusicSupportedConfigs"]["watchEndpointMusicConfig"]["musicVideoType"]
            .as_str()
    }
    ep(&r["navigationEndpoint"]).or_else(|| {
        ep(&r["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]
            ["playNavigationEndpoint"])
    })
}

/// Title + primary artist, lower-cased, minus "(Official Video)"-style noise
/// and the "- Topic"/"VEVO" channel suffixes videos carry.
fn song_key(t: &Track) -> String {
    let artist = primary_artist(&t.artist).to_lowercase();
    let artist = artist.strip_suffix("vevo").unwrap_or(&artist).trim();
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    format!("{}\u{0}{}", norm(&clean_title(&t.title).to_lowercase()), norm(artist))
}

pub fn radio(video_id: &str) -> Result<Vec<Track>, String> {
    let root = next(video_id)?;
    // The seed may come back as its audio/video counterpart under another id.
    let seed = parse_tracks(&root).iter().find(|t| t.video_id() == Some(video_id)).map(song_key);
    Ok(radio_tracks(&root)
        .into_iter()
        .filter(|t| t.video_id() != Some(video_id) && seed.as_ref() != Some(&song_key(t)))
        .collect())
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

pub const TYPE_WORDS: &[&str] = &["Song", "Video", "Episode", "Album", "Single", "EP", "Playlist", "Artist"];

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
            // Browser exports carry every site's cookies; keep only Google's.
            for c in list.iter().filter(|c| c["domain"].as_str().is_none_or(google_domain)) {
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
            // `get` rather than `[..7]`: a non-ASCII paste must not split a char.
            if line.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("cookie:")) {
                line = line[7..].trim();
            }
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 7 {
                if google_domain(cols[0]) {
                    add(&mut pairs, cols[5], cols[6]);
                }
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
    if let Some(name) = pairs.iter().find(|(k, v)| !valid_cookie(k) || !valid_cookie(v)).map(|(k, _)| k) {
        let name: String = name.chars().take(40).collect();
        return Err(format!("Cookie \"{name}\" has characters a Cookie header can't carry (non-ASCII?) — copy the cookies again"));
    }
    Ok(pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
}

/// Cookie domain (`.youtube.com`, `accounts.google.com`, …) YouTube sign-in needs.
fn google_domain(domain: &str) -> bool {
    let d = domain.trim().trim_start_matches('.').to_ascii_lowercase();
    ["youtube.com", "google.com", "googleapis.com"]
        .iter()
        .any(|s| d == *s || d.strip_suffix(s).is_some_and(|p| p.ends_with('.')))
}

/// Printable ASCII minus the header separators; anything else makes the
/// Cookie header invalid (and ureq's error would echo the whole header).
fn valid_cookie(s: &str) -> bool {
    s.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b';' && b != b',')
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
    let mut root = post("browse", json!({"browseId": "FEmusic_liked_playlists"}))?;
    let mut lists = parse_collections(&root);
    for _ in 0..MAX_PAGES {
        root = match next_page(&root) {
            Some(Ok(v)) => v,
            _ => break,
        };
        let more = parse_collections(&root);
        if more.is_empty() {
            break;
        }
        lists.extend(more);
    }
    let mut seen = HashSet::new();
    lists.retain(|c| (c.browse_id.starts_with("VL") || c.browse_id.starts_with("PL")) && seen.insert(c.browse_id.clone()));
    let liked = |c: &Collection| c.browse_id == "VLLM";
    if !lists.iter().any(liked) {
        lists.insert(0, Collection { title: "Liked Music".into(), subtitle: "Auto playlist".into(), browse_id: "VLLM".into(), art: String::new() });
    } else if let Some(i) = lists.iter().position(liked) {
        let c = lists.remove(i);
        lists.insert(0, c);
    }
    Ok(lists)
}

/// Video ids in the account's Liked Music.
pub fn liked_ids() -> Result<HashSet<String>, String> {
    Ok(browse("VLLM")?.iter().filter_map(|t| t.video_id().map(str::to_string)).collect())
}

pub fn set_liked(video_id: &str, liked: bool) -> Result<(), String> {
    let ep = if liked { "like/like" } else { "like/removelike" };
    post(ep, json!({"target": {"videoId": video_id}})).map(|_| ())
}

#[derive(Clone, Debug)]
pub struct AddTarget {
    pub playlist_id: String,
    pub title: String,
    /// The video is already in it.
    pub has: bool,
}

/// Playlists the account can add `video_id` to.
pub fn add_targets(video_id: &str) -> Result<Vec<AddTarget>, String> {
    Ok(parse_add_targets(&post("playlist/get_add_to_playlist", json!({"videoIds": [video_id]}))?))
}

fn parse_add_targets(root: &Value) -> Vec<AddTarget> {
    let mut opts = Vec::new();
    collect(root, "playlistAddToOptionRenderer", &mut opts);
    opts.into_iter()
        .filter_map(|o| {
            Some(AddTarget {
                playlist_id: o["playlistId"].as_str().filter(|s| !s.is_empty())?.to_string(),
                title: text(&o["title"]).trim().to_string(),
                has: o["containsSelectedVideos"].as_str() == Some("ALL"),
            })
        })
        .collect()
}

pub fn add_to_playlist(playlist_id: &str, video_id: &str) -> Result<(), String> {
    let id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
    let root = post(
        "browse/edit_playlist",
        json!({"playlistId": id, "actions": [{"action": "ACTION_ADD_VIDEO", "addedVideoId": video_id, "dedupeOption": "DEDUPE_OPTION_SKIP"}]}),
    )?;
    match root["status"].as_str() {
        Some("STATUS_SUCCEEDED") => Ok(()),
        s => Err(format!("YouTube said {}", s.unwrap_or("nothing"))),
    }
}

/// New private playlist holding `video_id`; returns its id.
pub fn create_playlist(title: &str, video_id: &str) -> Result<String, String> {
    let root = post("playlist/create", json!({"title": title, "privacyStatus": "PRIVATE", "videoIds": [video_id]}))?;
    root["playlistId"].as_str().map(str::to_string).ok_or_else(|| "YouTube didn't create the playlist".into())
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
    fn add_targets_json() {
        let root = json!({"contents": [{"addToPlaylistRenderer": {"playlists": [
            {"playlistAddToOptionRenderer": {"playlistId": "PLabc", "title": {"simpleText": "Road trip"}, "containsSelectedVideos": "ALL"}},
            {"playlistAddToOptionRenderer": {"playlistId": "PLdef", "title": {"runs": [{"text": "Chill"}]}, "containsSelectedVideos": "NONE"}},
            {"playlistAddToOptionRenderer": {"title": {"simpleText": "broken"}}}
        ]}}]});
        let t = parse_add_targets(&root);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].playlist_id.as_str(), t[0].title.as_str(), t[0].has), ("PLabc", "Road trip", true));
        assert_eq!((t[1].title.as_str(), t[1].has), ("Chill", false));
    }

    #[test]
    fn json_export_and_missing_sapisid() {
        let c = normalize_cookies(r#"[{"name":"SAPISID","value":"q"},{"name":"SID","value":"\"a\""}]"#).unwrap();
        assert_eq!(c, "SAPISID=q; SID=a");
        assert!(normalize_cookies("SID=abc; HSID=def").unwrap_err().contains("SAPISID"));
    }

    #[test]
    fn non_ascii_cookie_is_an_error_not_a_panic() {
        assert!(normalize_cookies("Cöokie: SAPISID=q").is_err());
        // Byte 7 falls inside a char: used to panic on `line[..7]`.
        assert!(normalize_cookies("ab\u{e9}\u{e9}\u{e9}=1").is_err());
        let e = normalize_cookies("SAPISID=q; NAME=caf\u{e9}").unwrap_err();
        assert!(e.contains("NAME"), "{e}");
    }

    #[test]
    fn imports_only_google_cookies() {
        let txt = ".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tq\n\
                   accounts.google.com\tTRUE\t/\tTRUE\t0\tLSID\tl\n\
                   .bank.example\tTRUE\t/\tTRUE\t0\tSESSION\tsecret\n\
                   .notyoutube.com\tTRUE\t/\tTRUE\t0\tEVIL\te\n";
        assert_eq!(normalize_cookies(txt).unwrap(), "LSID=l; SAPISID=q");
        let js = r#"[{"domain":".youtube.com","name":"SAPISID","value":"q"},{"domain":"github.com","name":"gh","value":"x"},
                     {"domain":"www.googleapis.com","name":"G","value":"g"}]"#;
        assert_eq!(normalize_cookies(js).unwrap(), "G=g; SAPISID=q");
    }

    fn panel(id: &str, title: &str, artist: &str, ty: &str) -> Value {
        json!({"playlistPanelVideoRenderer": {
            "videoId": id,
            "title": {"runs": [{"text": title}]},
            "longBylineText": {"runs": [{"text": artist}, {"text": " • "}, {"text": "3:00"}]},
            "navigationEndpoint": {"watchEndpoint": {"videoId": id, "watchEndpointMusicSupportedConfigs":
                {"watchEndpointMusicConfig": {"musicVideoType": ty}}}}
        }})
    }

    #[test]
    fn radio_dedupes_video_counterparts() {
        const ATV: &str = "MUSIC_VIDEO_TYPE_ATV";
        const OMV: &str = "MUSIC_VIDEO_TYPE_OMV";
        let root = json!({"contents": [
            // Video listed first, its audio counterpart nested after it.
            {"playlistPanelVideoWrapperRenderer": {
                "primaryRenderer": panel("vid1", "Song A (Official Video)", "Artist", OMV),
                "counterpart": [{"counterpartRenderer": panel("aud1", "Song A", "Artist", ATV)}]
            }},
            panel("aud2", "Song B", "Other", ATV),
            panel("vid2", "Song B", "OtherVEVO", "MUSIC_VIDEO_TYPE_UGC"),
            panel("aud2", "Song B", "Other", ATV),
            // Two genuine audio versions with the same name stay.
            panel("aud3", "Intro", "Band", ATV),
            panel("aud4", "Intro", "Band", ATV),
        ]});
        let ids: Vec<String> = radio_tracks(&root).iter().map(|t| t.video_id().unwrap().to_string()).collect();
        assert_eq!(ids, ["aud1", "aud2", "aud3", "aud4"]);
    }

    fn song_row(id: &str, title: &str) -> Value {
        json!({"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": id},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Artist", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCx"}}},
                    {"text": " • "}, {"text": "3:45"}]}}}
            ]
        }})
    }

    fn search_page(shelf_contents: Value) -> Value {
        json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content":
            {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents": shelf_contents}}]}}}}]}}})
    }

    #[test]
    fn search_parses_valid_response() {
        let root = search_page(json!([song_row("abc", "Hello"), song_row("def", "World")]));
        let Ok(Results::Tracks(t)) = parse_search(&root, Kind::Songs) else { panic!("expected tracks") };
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].title.as_str(), t[0].artist.as_str(), t[0].duration), ("Hello", "Artist", 225));
    }

    #[test]
    fn search_genuinely_empty_is_ok() {
        let root = json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content":
            {"sectionListRenderer": {"contents": [{"itemSectionRenderer": {"contents": [
                {"messageRenderer": {"text": {"runs": [{"text": "No results found"}]}}}]}}]}}}}]}}});
        assert!(matches!(parse_search(&root, Kind::Songs), Ok(Results::Tracks(t)) if t.is_empty()));
    }

    #[test]
    fn search_unrecognised_format_is_err() {
        // Shelf has items, but in a renderer we don't know.
        let root = search_page(json!([{"musicFancyNewItemRenderer": {"videoId": "abc"}}]));
        assert!(parse_search(&root, Kind::Songs).err().unwrap_or_default().contains("not recognised"));
        assert!(parse_search(&root, Kind::Albums).is_err());
        // No section list at all.
        assert!(parse_search(&json!({"frameworkUpdates": {}}), Kind::Songs).is_err());
    }
}
