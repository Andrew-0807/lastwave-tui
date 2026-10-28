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

fn call(mut params: Vec<(&str, String)>, secret: &str, post: bool) -> Result<Value, String> {
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
    let v: Value = match res {
        Ok(r) => r.into_json().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, r)) => r.into_json().unwrap_or_else(|_| Value::from(format!("HTTP {code}"))),
        Err(e) => return Err(e.to_string()),
    };
    if let Some(msg) = v["message"].as_str().filter(|_| v.get("error").is_some()) {
        return Err(format!("Last.fm: {msg}"));
    }
    if let Some(s) = v.as_str() {
        return Err(s.to_string());
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

pub fn now_playing(key: &str, secret: &str, sk: &str, t: &Track) -> Result<(), String> {
    call(track_params(t, key, sk, "track.updateNowPlaying"), secret, true).map(|_| ())
}

pub fn scrobble(key: &str, secret: &str, sk: &str, t: &Track, started_unix: u64) -> Result<(), String> {
    let mut p = track_params(t, key, sk, "track.scrobble");
    p.push(("timestamp", started_unix.to_string()));
    call(p, secret, true).map(|_| ())
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
