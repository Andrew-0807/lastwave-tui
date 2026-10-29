//! Last.fm scrobbling: eligibility, the retry queue and sending it.

use super::{App, Msg};
use crate::{lastfm, model::Track};
use std::thread;

/// Outcome of sending queued scrobbles, in order.
#[derive(Default)]
pub struct ScrobbleReport {
    accepted: Vec<Track>,
    /// Taken by Last.fm but not counted, or refused for good: (track, why).
    ignored: Vec<(Track, String)>,
    /// Not sent because of a temporary error; retried later.
    pub(super) unsent: Vec<(Track, u64)>,
    error: Option<lastfm::LfmError>,
}


/// Send scrobbles oldest first, stopping at the first temporary failure.
pub(super) fn submit_scrobbles(k: &str, s: &str, sk: &str, items: Vec<(Track, u64)>) -> ScrobbleReport {
    let mut r = ScrobbleReport::default();
    let mut it = items.into_iter();
    for (t, ts) in it.by_ref() {
        match lastfm::scrobble(k, s, sk, &t, ts) {
            Ok(None) => r.accepted.push(t),
            Ok(Some(why)) => r.ignored.push((t, why)),
            Err(e) if e.retryable() => {
                r.unsent.push((t, ts));
                r.error = Some(e);
                break;
            }
            Err(e) => r.ignored.push((t, e.to_string())),
        }
    }
    r.unsent.extend(it);
    r
}


/// Last.fm's rule: the track is longer than 30s and was played for half
/// its length or 4 minutes. `length` is 0 when unknown; a track that played
/// to its end then counts as long as what was heard.
pub(crate) fn scrobble_eligible(length: f64, played: f64, ended: bool) -> bool {
    let length = if length > 0.0 { length } else if ended { played } else { 0.0 };
    if length <= 0.0 {
        return played >= 240.0;
    }
    length > 30.0 && (played >= length / 2.0 || played >= 240.0)
}

pub(super) const MAX_PENDING_SCROBBLES: usize = 1000;

impl App {
    /// Queue the song that was playing for scrobbling if it qualifies.
    pub(super) fn queue_scrobble(&mut self) {
        let Some(mut t) = self.playing.take() else { return };
        let len = if self.track_len > 0.0 { self.track_len } else { t.duration as f64 };
        if !self.lfm_ready() || !scrobble_eligible(len, self.played, self.ended) {
            return;
        }
        if t.duration == 0 {
            t.duration = if len > 0.0 { len } else { self.played } as u32;
        }
        if self.scrobble_queue.len() >= MAX_PENDING_SCROBBLES {
            self.scrobble_queue.remove(0);
        }
        self.scrobble_queue.push((t, self.started_at));
    }

    /// Send queued scrobbles (the new one plus any that failed before).
    pub(super) fn flush_scrobbles(&mut self) {
        if self.scrobble_job.as_ref().is_some_and(|(j, _)| !j.is_finished())
            || self.scrobble_queue.is_empty()
            || !self.lfm_ready()
            || self.lfm_bad_session
        {
            return;
        }
        let items = std::mem::take(&mut self.scrobble_queue);
        let sent = items.clone();
        let (k, s, sk) = self.lfm_creds();
        let tx = self.tx.clone();
        let job = thread::spawn(move || {
            let r = submit_scrobbles(&k, &s, &sk, items);
            let unsent = r.unsent.clone();
            let _ = tx.send(Msg::Scrobbled(r));
            unsent
        });
        self.scrobble_job = Some((job, sent));
    }

    pub(super) fn on_scrobbled(&mut self, r: ScrobbleReport) {
        self.scrobble_job = None;
        let n_unsent = r.unsent.len();
        let mut q = r.unsent;
        q.append(&mut self.scrobble_queue);
        self.scrobble_queue = q;
        let waiting = |n: usize| if n == 1 { "1 scrobble waiting".to_string() } else { format!("{n} scrobbles waiting") };
        match &r.error {
            Some(e) if e.bad_session() => {
                self.lfm_bad_session = true;
                self.status = format!("{e} ({})", waiting(self.scrobble_queue.len()));
                return;
            }
            Some(e) => {
                self.status = format!("Scrobble failed: {e} — {}, retrying with the next song", waiting(self.scrobble_queue.len()));
                return;
            }
            None => {}
        }
        if let Some((t, why)) = r.ignored.last() {
            self.status = format!("Last.fm ignored {} — {} ({why})", t.artist, t.title);
        } else if let Some(t) = r.accepted.last() {
            let more = r.accepted.len() - 1;
            let more = if more > 0 { format!(" (+{more} earlier)") } else { String::new() };
            self.status = format!("Scrobbled: {} — {}{more}", t.artist, t.title);
        }
        if n_unsent == 0 && !r.accepted.is_empty() {
            // Last.fm is reachable again: send what queued up meanwhile.
            self.flush_scrobbles();
        }
    }

    pub(super) fn lfm_ready(&self) -> bool {
        !self.cfg.lastfm_session.is_empty() && !self.cfg.lastfm_api_key.is_empty()
    }

    pub(super) fn lfm_creds(&self) -> (String, String, String) {
        (self.cfg.lastfm_api_key.clone(), self.cfg.lastfm_api_secret.clone(), self.cfg.lastfm_session.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrobble_rule() {
        assert!(!scrobble_eligible(25.0, 25.0, true), "too short");
        assert!(scrobble_eligible(200.0, 100.0, false), "half played");
        assert!(!scrobble_eligible(200.0, 90.0, false));
        assert!(scrobble_eligible(1200.0, 240.0, false), "4 minutes of a long track");
        // Length unknown: a song that played to its end counts as its play time.
        assert!(scrobble_eligible(0.0, 180.0, true));
        assert!(!scrobble_eligible(0.0, 20.0, true));
        assert!(!scrobble_eligible(0.0, 180.0, false));
        assert!(scrobble_eligible(0.0, 240.0, false));
    }
}
