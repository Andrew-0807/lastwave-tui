//! The queue and playback: playing, advancing and skipping, undo, radio,
//! and the AI-song filter as it applies to the queue.

use super::{nav, now, App, Msg};
use crate::{aifilter, lastfm, lyrics, model::Track, ytm::{self, Collection}};
use serde_json::json;

/// The queue to go back to on undo, kept consistent with what is playing:
/// the playing song stays current, put back after the old current one if
/// the old queue doesn't hold it.
fn restore_queue(mut q: Vec<Track>, old_cur: Option<usize>, playing: Option<&Track>) -> (Vec<Track>, Option<usize>) {
    let Some(p) = playing else { return (q, None) };
    let url = p.url();
    if let Some(c) = old_cur.filter(|&c| q.get(c).is_some_and(|t| t.url() == url)) {
        return (q, Some(c));
    }
    if let Some(i) = q.iter().position(|t| t.url() == url) {
        return (q, Some(i));
    }
    let at = old_cur.map_or(q.len(), |c| (c + 1).min(q.len()));
    q.insert(at, p.clone());
    (q, Some(at))
}


const UNDO_DEPTH: usize = 10;

fn ai_left_out(n: usize) -> String {
    if n == 0 { String::new() } else { format!(" ({n} AI-generated left out)") }
}

impl App {
    pub fn is_ai(&self, t: &Track) -> bool {
        aifilter::is_ai(t, &self.cfg.ai_flagged, &self.cfg.ai_allowed)
    }

    /// Albums by an AI artist ("Album • Artist • 2024") and AI artist pages.
    /// Playlists are left alone: their subtitle names the curator.
    pub fn is_ai_coll(&self, c: &Collection) -> bool {
        let ai = |n: &str| aifilter::is_ai_artist(n, &self.cfg.ai_flagged, &self.cfg.ai_allowed);
        match c.browse_id.get(..2) {
            Some("UC") => ai(&c.title),
            Some("MP") => c.subtitle.split(" • ").map(str::trim).any(|p| {
                !p.is_empty() && !ytm::TYPE_WORDS.contains(&p) && !p.bytes().all(|b| b.is_ascii_digit()) && ai(p)
            }),
            _ => false,
        }
    }

    /// First queue index from `from` on that should play: with "Skip AI
    /// songs" on, tracks by AI artists are passed over.
    pub(super) fn next_playable(&self, from: usize) -> Option<usize> {
        (from..self.queue.len()).find(|&i| !self.cfg.skip_ai || !self.is_ai(&self.queue[i]))
    }

    /// Last queue index before `before` that should play.
    pub(super) fn prev_playable(&self, before: usize) -> Option<usize> {
        (0..before).rev().find(|&i| !self.cfg.skip_ai || !self.is_ai(&self.queue[i]))
    }

    /// With "Skip AI songs" on, drop AI tracks; returns (kept, dropped count).
    pub(super) fn drop_ai(&self, tracks: Vec<Track>) -> (Vec<Track>, usize) {
        let n = tracks.len();
        let kept: Vec<Track> = tracks.into_iter().filter(|t| !self.cfg.skip_ai || !self.is_ai(t)).collect();
        let ai = n - kept.len();
        (kept, ai)
    }

    pub(super) fn note_skipped(&mut self, n: usize) {
        if n > 0 {
            self.status = format!("Skipped {n} AI-generated song{}", if n == 1 { "" } else { "s" });
        }
    }

    pub(super) fn play_index(&mut self, i: usize) {
        let Some(url) = self.queue.get(i).map(Track::url) else { return };
        self.finish_track();
        // The cursor follows playback only if it was on the playing song.
        if self.cur.is_none() || self.queue_sel.selected().is_none() || self.queue_sel.selected() == self.cur {
            self.queue_sel.select(Some(i));
        }
        self.cur = Some(i);
        self.at_end = false;
        self.skip_eof = false;
        if self.mpv.is_none() {
            self.pending_play = self.mpv_starting;
            self.status = if self.mpv_starting { "Starting mpv…".into() } else { "mpv is not running".into() };
            return;
        }
        self.mpv_cmd(json!(["loadfile", url, "replace"]));
        self.mpv_set("pause", false);
        self.mpv_next = None;
        self.on_track_start();
        self.sync_next();
    }

    /// mpv moved on to the prefetched entry `n` (gapless advance).
    pub(super) fn advance_to(&mut self, n: usize) {
        let Some(c) = self.cur else { return };
        self.finish_track();
        self.note_skipped(n.saturating_sub(c + 1));
        if self.queue_sel.selected() == Some(c) {
            self.queue_sel.select(Some(n));
        }
        self.cur = Some(n);
        self.mpv_cmd(json!(["playlist-remove", 0]));
        self.mpv_next = None;
        self.on_track_start();
        self.sync_next();
    }

    /// After the queue ran out, start whatever has been added since.
    pub(super) fn resume_if_ended(&mut self) {
        if self.at_end && self.idle && self.mpv.is_some() && self.cur.and_then(|c| self.next_playable(c + 1)).is_some() {
            self.next();
        }
    }

    /// Remember the queue before a change that replaces or shortens it.
    pub(super) fn snapshot_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.queue_undo.len() >= UNDO_DEPTH {
            self.queue_undo.remove(0);
        }
        self.queue_undo.push((self.queue.clone(), self.cur));
    }

    pub(super) fn undo_queue(&mut self) {
        let Some((q, old_cur)) = self.queue_undo.pop() else {
            self.status = "Nothing to undo".into();
            return;
        };
        let playing = self.current().cloned();
        let (q, cur) = restore_queue(q, old_cur, playing.as_ref());
        self.queue = q;
        self.cur = cur;
        self.queue_sel.select(cur.or((!self.queue.is_empty()).then_some(0)));
        // Radio asked for the replaced queue no longer applies.
        self.radio_seq += 1;
        self.radio_busy = false;
        self.sync_next();
        self.status = format!("Queue restored ({} tracks)", self.queue.len());
        self.resume_if_ended();
    }

    /// Keep mpv's playlist as [current, next] so gapless + prefetch work.
    pub(super) fn sync_next(&mut self) {
        let Some(c) = self.cur else { return };
        let want = self.next_playable(c + 1).map(|i| self.queue[i].url());
        if want == self.mpv_next {
            return;
        }
        self.mpv_cmd(json!(["playlist-clear"]));
        if let Some(u) = &want {
            self.mpv_cmd(json!(["loadfile", u, "append"]));
        }
        self.mpv_next = want;
    }

    pub(super) fn on_track_start(&mut self) {
        self.pos = 0.0;
        self.dur = 0.0;
        self.played = 0.0;
        self.last_pos = 0.0;
        self.track_len = 0.0;
        self.ended = false;
        self.at_end = false;
        self.started_at = now();
        self.playing = self.current().cloned();
        let Some(t) = self.current().cloned() else { return };
        self.flush_scrobbles();

        self.lyrics = None;
        self.lyrics_loading = true;
        self.lyrics_key = t.url();
        let key = self.lyrics_key.clone();
        let lt = t.clone();
        self.spawn(move || Msg::Lyrics(key, lyrics::fetch(&lt)));

        if self.lfm_ready() && !self.lfm_bad_session {
            let (k, s, sk) = self.lfm_creds();
            let nt = t.clone();
            self.spawn(move || match lastfm::now_playing(&k, &s, &sk, &nt) {
                Ok(()) => Msg::Status(String::new()),
                Err(e) => Msg::Status(e.to_string()),
            });
        }
        self.maybe_radio();
    }

    pub(super) fn maybe_radio(&mut self) {
        let Some(c) = self.cur else { return };
        if !self.cfg.radio || self.radio_busy || c + 2 < self.queue.len() {
            return;
        }
        let Some(id) = self.queue[c].video_id().map(str::to_string) else { return };
        self.request_radio(id);
    }

    pub(super) fn request_radio(&mut self, id: String) {
        self.radio_busy = true;
        self.radio_seq += 1;
        let seq = self.radio_seq;
        self.spawn(move || Msg::Radio(seq, ytm::radio(&id)));
    }

    pub(super) fn finish_track(&mut self) {
        self.queue_scrobble();
        self.flush_scrobbles();
    }

    /// Play `t` right away, inserted after the current song; the rest of the queue stays.
    pub fn play_now(&mut self, t: Track) {
        let i = match self.cur {
            Some(c) => {
                self.queue.insert(c + 1, t);
                c + 1
            }
            None => {
                self.queue.push(t);
                self.queue.len() - 1
            }
        };
        self.play_index(i);
    }

    pub fn play_list(&mut self, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.finish_track();
        let had = !self.queue.is_empty();
        self.snapshot_queue();
        self.queue = tracks;
        self.cur = None;
        self.radio_seq += 1;
        self.radio_busy = false;
        self.play_index(start);
        if had {
            self.status = "Queue replaced — u to undo".into();
        }
    }

    /// Insert right after the current track (or at the end when idle).
    pub fn play_next(&mut self, tracks: Vec<Track>) {
        let (tracks, ai) = self.drop_ai(tracks);
        let n = tracks.len();
        let at = self.cur.map_or(self.queue.len(), |c| c + 1);
        self.queue.splice(at..at, tracks);
        self.sync_next();
        self.status = format!("{n} track{} will play next{}", if n == 1 { "" } else { "s" }, ai_left_out(ai));
        self.resume_if_ended();
    }

    pub fn append(&mut self, tracks: Vec<Track>) {
        let (tracks, ai) = self.drop_ai(tracks);
        let n = tracks.len();
        self.queue.extend(tracks);
        self.sync_next();
        self.status = format!("Added {n} track{} to queue{}", if n == 1 { "" } else { "s" }, ai_left_out(ai));
        self.resume_if_ended();
    }

    pub(super) fn start_radio(&mut self, t: Track) {
        let had = !self.queue.is_empty();
        self.play_list(vec![t], 0);
        if let Some(id) = self.queue[0].video_id().map(str::to_string) {
            self.request_radio(id);
            self.status = if had { "Starting radio… (u to undo)" } else { "Starting radio…" }.into();
        }
    }

    /// Radio from the playing song without restarting it: what's queued
    /// after it is replaced by the radio.
    pub(super) fn radio_from_current(&mut self) {
        let Some(c) = self.cur else {
            self.status = "Nothing is playing".into();
            return;
        };
        let Some(id) = self.queue[c].video_id().map(str::to_string) else {
            self.status = "Radio needs a YouTube song".into();
            return;
        };
        let cut = self.queue.len() > c + 1;
        if cut {
            self.snapshot_queue();
        }
        self.queue.truncate(c + 1);
        self.sync_next();
        self.request_radio(id);
        self.status = if cut { "Starting radio… (u to undo)" } else { "Starting radio…" }.into();
    }

    pub(super) fn next(&mut self) {
        let from = self.cur.map_or(0, |c| c + 1);
        match self.next_playable(from) {
            Some(i) => {
                self.play_index(i);
                self.note_skipped(i - from);
            }
            None => self.status = "End of queue".into(),
        }
    }

    /// Flag (or unflag) the playing song's artist as AI-generated.
    pub(super) fn toggle_ai_flag(&mut self) {
        let Some(t) = self.current().cloned() else { return };
        if self.is_ai(&t) {
            aifilter::unflag(&t.artist, &mut self.cfg.ai_flagged, &mut self.cfg.ai_allowed, aifilter::listed);
            self.cfg.save();
            self.status = format!("{} is no longer treated as AI", t.artist);
            self.sync_next();
            return;
        }
        let name = aifilter::flag(&t.artist, &mut self.cfg.ai_flagged, &mut self.cfg.ai_allowed, aifilter::listed);
        self.cfg.save();
        self.hide_ai_discover();
        if !self.cfg.skip_ai {
            self.status = format!("Flagged {name} — turn on Skip AI songs in Settings to skip them");
            return;
        }
        let removed = self.drop_ai_from_queue();
        self.next();
        if self.status != "End of queue" {
            self.status = format!("Flagged {name} as AI-generated");
        }
        if removed > 0 {
            self.status += &format!("; removed {removed} more AI song{} from the queue", if removed == 1 { "" } else { "s" });
        }
    }

    /// Remove AI tracks from the queue, except the current one.
    pub(super) fn drop_ai_from_queue(&mut self) -> usize {
        self.snapshot_queue();
        let before = self.queue.len();
        let cur = self.cur;
        let keep: Vec<bool> = self.queue.iter().enumerate().map(|(i, t)| Some(i) == cur || !self.is_ai(t)).collect();
        let mut new_cur = None;
        let mut q = Vec::with_capacity(before);
        for (i, t) in std::mem::take(&mut self.queue).into_iter().enumerate() {
            if keep[i] {
                if Some(i) == cur {
                    new_cur = Some(q.len());
                }
                q.push(t);
            }
        }
        self.queue = q;
        self.cur = new_cur;
        nav(&mut self.queue_sel, self.queue.len(), 0);
        if before == self.queue.len() {
            self.queue_undo.pop();
        }
        before - self.queue.len()
    }

    pub(super) fn prev(&mut self) {
        match self.cur.and_then(|c| self.prev_playable(c)) {
            Some(i) if self.pos <= 3.0 => self.play_index(i),
            _ => self.mpv_cmd(json!(["seek", 0, "absolute"])),
        }
    }

    pub(super) fn toggle_pause(&mut self) {
        if self.mpv.is_none() {
            self.pending_play = self.mpv_starting && self.cur.is_some();
            self.status = if self.mpv_starting { "Starting mpv…".into() } else { "mpv is not running".into() };
            return;
        }
        if self.idle {
            // After the queue ran out, go on with anything added since;
            // with nothing after it, play the last song again.
            let from = match self.cur {
                Some(c) if self.at_end && self.next_playable(c + 1).is_some() => c + 1,
                c => c.unwrap_or(0),
            };
            match self.next_playable(from) {
                Some(i) => {
                    self.play_index(i);
                    self.note_skipped(i - from);
                }
                None if !self.queue.is_empty() => self.status = "Nothing left to play (the rest is AI-generated)".into(),
                None => {}
            }
        } else {
            self.mpv_cmd(json!(["cycle", "pause"]));
        }
    }

    pub(super) fn remove_from_queue(&mut self, i: usize) {
        if i >= self.queue.len() {
            return;
        }
        let was_current = self.cur == Some(i);
        if was_current {
            // Before the queue shifts: the scrobble is for this song.
            self.finish_track();
        }
        self.snapshot_queue();
        self.queue.remove(i);
        self.status = "Removed — u to undo".into();
        match self.cur {
            Some(c) if c > i => self.cur = Some(c - 1),
            _ => {}
        }
        if was_current {
            match self.next_playable(i) {
                Some(n) => {
                    self.play_index(n);
                    self.note_skipped(n - i);
                }
                None => {
                    self.cur = None;
                    self.mpv_cmd(json!(["stop"]));
                }
            }
        } else {
            self.sync_next();
        }
        nav(&mut self.queue_sel, self.queue.len(), 0);
    }

    pub(super) fn move_in_queue(&mut self, i: usize, up: bool) {
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|&j| j < self.queue.len()) };
        let Some(j) = j else { return };
        self.queue.swap(i, j);
        if self.cur == Some(i) {
            self.cur = Some(j);
        } else if self.cur == Some(j) {
            self.cur = Some(i);
        }
        self.queue_sel.select(Some(j));
        self.sync_next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testutil::{ids, t};
    #[test]
    fn undo_keeps_playing_song_current() {
        let old = vec![t("a"), t("b"), t("c")];
        // The old current song is still playing.
        let (q, c) = restore_queue(old.clone(), Some(1), Some(&t("b")));
        assert_eq!((ids(&q), c), (ids(&old), Some(1)));
        // Playing a song that was elsewhere in the old queue.
        let (q, c) = restore_queue(old.clone(), Some(0), Some(&t("c")));
        assert_eq!((ids(&q), c), (ids(&old), Some(2)));
        // A song from the replacing list keeps playing, slotted after the old current.
        let (q, c) = restore_queue(old.clone(), Some(1), Some(&t("x")));
        assert_eq!((ids(&q), c), (vec!["a".into(), "b".into(), "x".into(), "c".into()], Some(2)));
        // Nothing playing.
        let (q, c) = restore_queue(old.clone(), Some(1), None);
        assert_eq!((ids(&q), c), (ids(&old), None));
    }
}
