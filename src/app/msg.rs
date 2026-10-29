//! Messages from background threads and mpv, and how the app takes them.

use super::{nav, scrobble::ScrobbleReport, App, DItem, View, MAX_CACHED_PAGES};
use crate::{
    config, lastfm,
    lyrics::Lyrics,
    model::Track,
    mpv::{Mpv, MpvEvent},
    ytm::{self, AddTarget, Collection, Entry, Results, Shelf},
};
use image::DynamicImage;
use serde_json::json;
use std::collections::HashSet;

pub enum Msg {
    Mpv(MpvEvent),
    Search(u64, Result<Results, String>),
    /// (request id, opened from the Library view?, page, tracks)
    Browse(u64, bool, Collection, Result<Vec<Track>, String>),
    Home(Result<Vec<Shelf>, String>),
    YtLogin(Result<(String, String), String>),
    YtLists(Result<Vec<Collection>, String>),
    /// (request id, tracks)
    Radio(u64, Result<Vec<Track>, String>),
    /// mpv finished starting in the background: (player, audio devices).
    MpvReady(Result<(Mpv, Vec<(String, String)>), String>),
    Scrobbled(ScrobbleReport),
    Lyrics(String, Option<Lyrics>),
    Library(Result<Vec<Track>, String>),
    Status(String),
    /// (connect attempt, token)
    LfmToken(u64, Result<String, String>),
    /// (connect attempt, (session key, user name))
    LfmSession(u64, Result<(String, String), lastfm::LfmError>),
    Art(String, Option<DynamicImage>),
    AiList(Result<usize, String>),
    Liked(Result<HashSet<String>, String>),
    /// (video id, liked, result)
    LikeResult(String, bool, Result<(), String>),
    AddTargets(String, Result<Vec<AddTarget>, String>),
    /// (playlist title, browse id of the playlist added to)
    Added(String, Result<String, String>),
}

/// Songs in a row that may fail to load before playback stops.
const MAX_FAILS: u32 = 5;

impl App {
    pub fn on_msg(&mut self, m: Msg) {
        // mpv reports these many times a second; the periodic redraw shows them.
        let frequent = matches!(&m, Msg::Mpv(MpvEvent::Prop(n, _)) if n == "time-pos" || n == "audio-bitrate");
        let status = frequent.then(|| self.status.clone());
        self.handle_msg(m);
        self.keep_sticky();
        if status.is_none_or(|s| s != self.status) {
            self.dirty = true;
        }
    }

    pub(super) fn handle_msg(&mut self, m: Msg) {
        match m {
            Msg::Mpv(ev) => self.on_mpv(ev),
            Msg::MpvReady(r) => {
                self.mpv_starting = false;
                match r {
                    Ok((m, devices)) => {
                        self.devices = devices;
                        self.mpv = Some(m);
                        if std::mem::take(&mut self.pending_play) {
                            if let Some(c) = self.cur {
                                self.play_index(c);
                            }
                        }
                    }
                    Err(e) => {
                        self.pending_play = false;
                        self.mpv_error = Some(e);
                    }
                }
            }
            Msg::Scrobbled(r) => self.on_scrobbled(r),
            Msg::Search(seq, r) if seq == self.search_seq => {
                self.searching = false;
                match r {
                    Ok(r) => {
                        let n = match &r {
                            Results::Tracks(t) => t.len(),
                            Results::Collections(c) => c.len(),
                        };
                        self.status = format!("{n} results");
                        self.search_sel.select((n > 0).then_some(0));
                        self.results = Some(r);
                    }
                    Err(e) => self.status = format!("Search failed: {e}"),
                }
            }
            Msg::Search(..) => {}
            Msg::Browse(seq, lib, c, r) => {
                let fresh = seq == self.browse_seq;
                if fresh {
                    self.searching = false;
                    self.yt_loading = false;
                }
                // A page opened before something else: keep a library copy, drop the rest.
                if !fresh && (!lib || r.is_err()) {
                    return;
                }
                match r {
                    Ok(mut t) => {
                        ytm::fill_from_collection(&mut t, &c);
                        if fresh {
                            self.status = format!("{}: {} tracks", c.title, t.len());
                        }
                        let first = (!t.is_empty()).then_some(0);
                        if lib {
                            // A refresh of what's already on screen keeps the cursor.
                            let same = self.yt_open.as_ref().is_some_and(|(o, _)| o.browse_id == c.browse_id);
                            let show = same || (fresh && self.yt_open.is_none());
                            if show && (!same || self.yt_open_sel.selected().is_none_or(|i| i >= t.len())) {
                                self.yt_open_sel.select(first);
                            }
                            if self.cache.pages.len() >= MAX_CACHED_PAGES && !self.cache.pages.contains_key(&c.browse_id) {
                                if let Some(k) = self.cache.pages.keys().next().cloned() {
                                    self.cache.pages.remove(&k);
                                }
                            }
                            self.cache.pages.insert(c.browse_id.clone(), t.clone());
                            // Only replace the view if the user is still on it (or on the list).
                            if show {
                                self.yt_open = Some((c, t));
                            }
                            self.save_cache();
                        } else {
                            self.browse_sel.select(first);
                            self.browse = Some((c, t));
                        }
                    }
                    Err(e) => self.status = format!("Could not open {}: {e}", c.title),
                }
            }
            Msg::Home(r) => {
                self.discover_loading = false;
                match r {
                    Ok(shelves) => {
                        self.discover.clear();
                        for sh in shelves {
                            self.discover.push(DItem::Header(sh.title));
                            self.discover.extend(sh.items.into_iter().map(|e| match e {
                                Entry::Track(t) => DItem::Track(t),
                                Entry::Collection(c) => DItem::Coll(c),
                            }));
                        }
                        self.hide_ai_discover();
                        let keep = self.discover_sel.selected().filter(|&i| matches!(self.discover.get(i), Some(DItem::Track(_) | DItem::Coll(_))));
                        self.discover_sel.select(keep.or((self.discover.len() > 1).then_some(1)));
                        self.save_cache();
                    }
                    Err(e) => self.status = format!("Discover failed: {e}"),
                }
            }
            Msg::YtLogin(r) => match r {
                Ok((cookies, name)) => {
                    config::save_cookies(Some(&cookies));
                    self.status = format!("Signed in to YouTube Music as {name}");
                    self.cfg.yt_user = name;
                    self.cfg.save();
                    self.mpv_set("ytdl-raw-options", config::ytdl_raw_options(&self.cfg));
                    self.load_home();
                    self.load_yt_lists();
                    self.load_liked();
                }
                Err(e) => self.status = e,
            },
            Msg::YtLists(r) => {
                self.yt_loading = false;
                match r {
                    Ok(l) => {
                        if self.yt_lists_sel.selected().is_none_or(|i| i >= l.len()) {
                            self.yt_lists_sel.select((!l.is_empty()).then_some(0));
                        }
                        self.yt_lists = l;
                        self.save_cache();
                    }
                    Err(e) => self.status = format!("Could not load playlists: {e}"),
                }
            }
            // Radio for a queue that has been replaced since.
            Msg::Radio(seq, _) if seq != self.radio_seq => {}
            Msg::Radio(_, r) => {
                self.radio_busy = false;
                match r {
                    Ok(tracks) => {
                        let have: HashSet<String> = self.queue.iter().map(Track::url).collect();
                        let fresh: Vec<Track> = tracks.into_iter().filter(|t| !have.contains(&t.url())).collect();
                        let total = fresh.len();
                        let fresh: Vec<Track> = fresh.into_iter().filter(|t| !self.cfg.skip_ai || !self.is_ai(t)).collect();
                        let n = fresh.len();
                        self.queue.extend(fresh);
                        self.sync_next();
                        self.status = match total - n {
                            0 => format!("Radio: +{n} tracks"),
                            ai => format!("Radio: +{n} tracks ({ai} AI-generated left out)"),
                        };
                        self.resume_if_ended();
                    }
                    Err(e) => self.status = format!("Radio failed: {e}"),
                }
            }
            Msg::Lyrics(key, l) => {
                if key == self.lyrics_key {
                    self.lyrics_loading = false;
                    self.lyrics = l;
                }
            }
            Msg::Library(Ok(l)) => {
                self.lib_scanning = false;
                self.status = format!("Library: {} tracks", l.len());
                self.set_library(l);
                nav(&mut self.lib_sel, self.lib_vis.len(), 0);
            }
            // The folder went away or became unreadable since check_dir.
            Msg::Library(Err(e)) => {
                self.lib_scanning = false;
                self.lib_error = Some(e.clone());
                self.status = e;
            }
            Msg::Status(s) => {
                if !s.is_empty() {
                    self.status = s;
                }
            }
            // A reply to a connect attempt that was cancelled or started over.
            Msg::LfmToken(seq, _) | Msg::LfmSession(seq, _) if seq != self.lfm_seq => {}
            Msg::LfmToken(_, r) => {
                self.lfm_busy = false;
                match r {
                    Ok(tok) => {
                        // Opened once per token; Enter afterwards only checks for approval.
                        let url = lastfm::auth_url(&self.cfg.lastfm_api_key, &tok);
                        lastfm::open_url(&url);
                        self.lfm_auth = Some((tok, url));
                        self.status = "Approve LastWave on Last.fm (link in Settings → Notes), then Enter on Last.fm account".into();
                    }
                    Err(e) => self.status = e,
                }
            }
            Msg::Art(key, img) => {
                if let Some(c) = &mut self.covers {
                    c.loaded(key, img);
                }
            }
            Msg::AiList(r) => match r {
                Ok(n) => {
                    if self.view == View::Settings {
                        self.status = format!("AI artist list updated: {n} artists");
                    }
                    self.hide_ai_discover();
                    self.sync_next();
                }
                Err(e) => self.status = e,
            },
            Msg::Liked(r) => {
                if let Ok(l) = r {
                    self.liked = l;
                    self.save_cache();
                }
            }
            Msg::LikeResult(id, liked, r) => match r {
                // Liked Music changed: refetch it next time it's opened.
                Ok(()) => {
                    self.cache.pages.remove("VLLM");
                    self.save_cache();
                }
                Err(e) => {
                    if liked {
                        self.liked.remove(&id);
                    } else {
                        self.liked.insert(id);
                    }
                    self.status = format!("Could not update like: {e}");
                }
            },
            Msg::AddTargets(id, r) => {
                let Some(p) = self.picker.as_mut().filter(|p| p.track.video_id() == Some(id.as_str())) else { return };
                match r {
                    Ok(items) => {
                        p.items = items;
                        p.loading = false;
                    }
                    Err(e) => {
                        self.picker = None;
                        self.status = format!("Could not load playlists: {e}");
                    }
                }
            }
            Msg::Added(title, r) => match r {
                Ok(bid) => {
                    self.status = format!("Added to {title}");
                    self.cache.pages.remove(&bid);
                    if let Some(c) = self.yt_open.as_ref().map(|(c, _)| c.clone()).filter(|c| c.browse_id == bid) {
                        self.open_collection(c, true);
                    }
                    self.save_cache();
                    self.load_yt_lists();
                }
                Err(e) => self.status = format!("Could not add to {title}: {e}"),
            },
            Msg::LfmSession(_, r) => {
                self.lfm_busy = false;
                match r {
                    Ok((sk, name)) => {
                        self.cfg.lastfm_session = sk;
                        self.status = format!("Last.fm connected as {name}");
                        self.cfg.lastfm_user = name;
                        self.lfm_auth = None;
                        self.lfm_bad_session = false;
                        self.cfg.save();
                        self.flush_scrobbles();
                    }
                    Err(e) if e.token_not_approved() => {
                        self.status = "Not approved yet — open the link in Settings → Notes, approve, then Enter again (Esc cancels)".into();
                    }
                    // The token can't become a session any more: start over.
                    Err(e) if e.token_dead() => {
                        self.lfm_auth = None;
                        self.status = "The Last.fm approval link expired or is invalid — press Enter on Last.fm account to get a new one".into();
                    }
                    Err(e) => self.status = format!("{e} — Enter to try again, Esc to cancel"),
                }
            }
        }
    }

    pub(super) fn on_mpv(&mut self, ev: MpvEvent) {
        match ev {
            MpvEvent::Prop(name, v) => match name.as_str() {
                "time-pos" => {
                    let p = v.as_f64().unwrap_or(0.0);
                    let d = p - self.last_pos;
                    // After eof the next file's position may arrive first; don't count it.
                    if !self.paused && !self.ended && d > 0.0 && d < 2.0 {
                        self.played += d;
                        if self.played > 1.0 {
                            self.fail_streak = 0;
                        }
                    }
                    self.last_pos = p;
                    self.pos = p;
                }
                "duration" => {
                    self.dur = v.as_f64().unwrap_or(0.0);
                    if self.dur > 0.0 && !self.ended {
                        self.track_len = self.dur;
                    }
                }
                "pause" => self.paused = v.as_bool().unwrap_or(false),
                "volume" => self.volume = v.as_f64().unwrap_or(self.volume),
                "idle-active" => self.idle = v.as_bool().unwrap_or(false),
                "audio-codec-name" => self.codec = v.as_str().unwrap_or_default().to_string(),
                "audio-params" => self.params = v,
                "audio-out-params" => self.out_params = v,
                "audio-bitrate" => self.bitrate = v.as_f64().unwrap_or(0.0),
                "current-ao" => self.ao = v.as_str().unwrap_or_default().to_string(),
                "af" => self.af = v.as_array().map(|a| if a.is_empty() { "" } else { "on" }).unwrap_or("").into(),
                "path" => {
                    let Some(p) = v.as_str() else { return };
                    let Some(c) = self.cur else { return };
                    if self.queue.get(c).is_some_and(|t| t.url() == p) {
                        return;
                    }
                    // mpv moved on to the prefetched next entry (gapless advance)
                    // before its eof for the old file came in.
                    if let Some(n) = self.next_playable(c + 1).filter(|&n| self.queue[n].url() == p) {
                        self.advance_to(n);
                        self.skip_eof = true;
                    }
                }
                _ => {}
            },
            MpvEvent::EndFile(reason) => match reason.as_str() {
                "error" => self.on_load_error(),
                "eof" if std::mem::take(&mut self.skip_eof) => {}
                "eof" => {
                    self.ended = true;
                    // With a prefetched entry mpv goes straight on to it. Done
                    // here rather than on `path`, which doesn't change when the
                    // same song is queued twice in a row.
                    let next = self.mpv_next.as_ref().and(self.cur).and_then(|c| self.next_playable(c + 1));
                    match next {
                        Some(n) => self.advance_to(n),
                        None => {
                            self.finish_track();
                            self.at_end = true;
                        }
                    }
                }
                _ => {}
            },
            MpvEvent::Exited => {
                self.mpv = None;
                self.idle = true;
                self.mpv_error = Some("mpv exited — restart LastWave TUI".into());
            }
        }
    }

    /// The playing song failed to load: skip to the next one, up to a point.
    pub(super) fn on_load_error(&mut self) {
        let Some(c) = self.cur else { return };
        let what = self.current().map(|t| t.title.clone()).unwrap_or_default();
        self.playing = None;
        self.fail_streak += 1;
        if self.fail_streak >= MAX_FAILS {
            self.fail_streak = 0;
            self.mpv_cmd(json!(["stop"]));
            self.mpv_next = None;
            self.at_end = false;
            self.status = format!("Stopped: {MAX_FAILS} songs in a row could not play (is yt-dlp installed and up to date?)");
            return;
        }
        match self.next_playable(c + 1) {
            Some(n) => {
                self.play_index(n);
                self.status = format!("Could not play \"{what}\" — skipped");
            }
            None => {
                self.at_end = true;
                self.status = format!("Could not play \"{what}\" (is yt-dlp installed and up to date?)");
            }
        }
    }
}
