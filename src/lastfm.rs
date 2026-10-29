//! Last.fm desktop auth + scrobbling (signing mirrors LastWave's LastFmSigner).

use crate::{
    model::{clean_title, Track},
    ytm::agent,
};
use md5::{Digest, Md5};
use serde_json::Value;

const API: &str = "https://ws.audioscrobbler.com/2.0/";

fn sign(params: &[(&str, String)], secret: &str) -> String {
    let mut sorted: Vec<_> = params.iter().filter(|(k, v)| *k != "format" && !v.is_empty()).collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut s = String::new();
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    s.push_str(secret);
    format!("{:x}", Md5::digest(s.as_bytes()))
}

/// A failed Last.fm call: the API error code (0 for network/HTTP trouble) and message.
#[derive(Debug, Clone, PartialEq)]
pub struct LfmError {
    pub code: i64,
    pub msg: String,
}

impl LfmError {
    /// Error 9: the session key was revoked or expired; the user must reconnect.
    pub fn bad_session(&self) -> bool {
        self.code == 9
    }

    /// Worth sending again later: network trouble or a temporary Last.fm outage.
    pub fn retryable(&self) -> bool {
        matches!(self.code, 0 | 8 | 9 | 11 | 16 | 29)
    }
}

impl std::fmt::Display for LfmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.bad_session() {
            f.write_str("Last.fm session expired — reconnect in Settings → Last.fm account")
        } else {
            f.write_str(&self.msg)
        }
    }
}

fn call(params: Vec<(&str, String)>, secret: &str, post: bool) -> Result<Value, String> {
    call_coded(params, secret, post).map_err(|e| e.to_string())
}

fn call_coded(mut params: Vec<(&str, String)>, secret: &str, post: bool) -> Result<Value, LfmError> {
    let sig = sign(&params, secret);
    params.push(("api_sig", sig));
    params.push(("format", "json".into()));
    let pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let res = if post {
        agent().post(API).send_form(&pairs)
    } else {
        let mut req = agent().get(API);
        for (k, v) in &pairs {
            req = req.query(k, v);
        }
        req.call()
    };
    let net = |msg: String| LfmError { code: 0, msg };
    let v: Value = match res {
        Ok(r) => r.into_json().map_err(|e| net(e.to_string()))?,
        Err(ureq::Error::Status(code, r)) => r.into_json().unwrap_or_else(|_| Value::from(format!("HTTP {code}"))),
        Err(e) => return Err(net(e.to_string())),
    };
    if let Some(code) = v.get("error") {
        let msg = v["message"].as_str().unwrap_or("unknown error");
        return Err(LfmError { code: code.as_i64().unwrap_or(-1), msg: format!("Last.fm: {msg}") });
    }
    if let Some(s) = v.as_str() {
        return Err(net(s.to_string()));
    }
    Ok(v)
}

pub fn get_token(key: &str, secret: &str) -> Result<String, String> {
    let v = call(vec![("method", "auth.getToken".into()), ("api_key", key.into())], secret, false)?;
    v["token"].as_str().map(str::to_string).ok_or_else(|| "no token in response".into())
}

pub fn auth_url(key: &str, token: &str) -> String {
    format!("https://www.last.fm/api/auth/?api_key={key}&token={token}")
}

/// Returns (session key, user name).
pub fn get_session(key: &str, secret: &str, token: &str) -> Result<(String, String), String> {
    let v = call(
        vec![("method", "auth.getSession".into()), ("api_key", key.into()), ("token", token.into())],
        secret,
        false,
    )?;
    let s = &v["session"];
    match (s["key"].as_str(), s["name"].as_str()) {
        (Some(k), Some(n)) => Ok((k.into(), n.into())),
        _ => Err("authorise LastWave in the browser first, then press Enter again".into()),
    }
}

fn track_params<'a>(t: &Track, key: &str, sk: &str, method: &'a str) -> Vec<(&'a str, String)> {
    let mut p = vec![
        ("method", method.to_string()),
        ("api_key", key.to_string()),
        ("sk", sk.to_string()),
        ("artist", t.artist.trim_end_matches(" - Topic").to_string()),
        ("track", clean_title(&t.title)),
    ];
    if !t.album.is_empty() {
        p.push(("album", t.album.clone()));
    }
    if t.duration > 0 {
        p.push(("duration", t.duration.to_string()));
    }
    p
}

pub fn now_playing(key: &str, secret: &str, sk: &str, t: &Track) -> Result<(), LfmError> {
    call_coded(track_params(t, key, sk, "track.updateNowPlaying"), secret, true).map(|_| ())
}

/// `Ok(None)`: accepted. `Ok(Some(reason))`: Last.fm took the request but
/// ignored the scrobble (filtered artist, bad timestamp, daily limit…).
pub fn scrobble(key: &str, secret: &str, sk: &str, t: &Track, started_unix: u64) -> Result<Option<String>, LfmError> {
    let mut p = track_params(t, key, sk, "track.scrobble");
    p.push(("timestamp", started_unix.to_string()));
    call_coded(p, secret, true).map(|v| ignored_reason(&v))
}

/// Why a `track.scrobble` reply was not counted, if it wasn't.
fn ignored_reason(v: &Value) -> Option<String> {
    let s = &v["scrobbles"];
    let num = |x: &Value| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()));
    let accepted = num(&s["@attr"]["accepted"]);
    // A batch reply holds an array; a single scrobble an object.
    let one = if s["scrobble"].is_array() { &s["scrobble"][0] } else { &s["scrobble"] };
    let msg = &one["ignoredMessage"];
    let code = num(&msg["code"]).unwrap_or(0);
    if code == 0 && accepted != Some(0) {
        return None;
    }
    let text = msg["#text"].as_str().unwrap_or_default().trim();
    let why = match code {
        1 => "artist was filtered",
        2 => "track was filtered",
        3 => "timestamp too old",
        4 => "timestamp too new",
        5 => "daily scrobble limit reached",
        _ => "not accepted",
    };
    Some(if text.is_empty() { why.to_string() } else { format!("{why}: {text}") })
}

pub fn open_url(url: &str) {
    use std::process::Command;
    #[cfg(windows)]
    let r = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = Command::new("xdg-open").arg(url).spawn();
    let _ = r;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrobble_reply_parsing() {
        let ok = json!({"scrobbles": {"@attr": {"accepted": 1, "ignored": 0},
            "scrobble": {"ignoredMessage": {"code": "0", "#text": ""}}}});
        assert_eq!(ignored_reason(&ok), None);
        let ignored = json!({"scrobbles": {"@attr": {"accepted": 0, "ignored": 1},
            "scrobble": {"ignoredMessage": {"code": "1", "#text": "Artist name failed filter"}}}});
        assert_eq!(ignored_reason(&ignored).as_deref(), Some("artist was filtered: Artist name failed filter"));
        let none = json!({"scrobbles": {"@attr": {"accepted": "0", "ignored": "1"}, "scrobble": {}}});
        assert_eq!(ignored_reason(&none).as_deref(), Some("not accepted"));
    }

    #[test]
    fn error_classes() {
        let e = |code| LfmError { code, msg: String::new() };
        assert!(e(9).bad_session() && e(9).retryable());
        assert!(e(0).retryable() && e(16).retryable() && e(29).retryable());
        assert!(!e(6).retryable() && !e(13).retryable());
    }
}
