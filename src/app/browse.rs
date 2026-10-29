//! Search, Discover, opened album / playlist pages, the Library, and the
//! YouTube Music list actions (like, add to playlist).

use super::{list_nav, App, DItem, Input, Msg, Picker};
use crate::{library, model::Track, ytm::{self, Collection, Results}};
use ratatui::{crossterm::event::KeyCode, widgets::ListState};
use std::path::PathBuf;

/// "PLxxx" / "VLPLxxx" -> the browse id library pages are keyed by.
fn browse_id_of(playlist_id: &str) -> String {
    if playlist_id.starts_with("VL") { playlist_id.to_string() } else { format!("VL{playlist_id}") }
}

impl App {
    pub(super) fn toggle_like(&mut self) {
        let Some((t, id)) = self.account_target("like songs", "like it") else { return };
        let liked = !self.liked.contains(&id);
        if liked {
            self.liked.insert(id.clone());
        } else {
            self.liked.remove(&id);
        }
        self.status = format!("{} {}", if liked { "♥ Liked" } else { "Removed like:" }, t.title);
        self.spawn(move || {
            let r = ytm::set_liked(&id, liked);
            Msg::LikeResult(id, liked, r)
        });
    }

    pub(super) fn open_picker(&mut self) {
        let Some((t, id)) = self.account_target("add to playlists", "add it to a playlist") else { return };
        let mut sel = ListState::default();
        sel.select(Some(0));
        self.picker = Some(Picker { track: t, items: Vec::new(), sel, loading: true });
        self.spawn(move || {
            let r = ytm::add_targets(&id);
            Msg::AddTargets(id, r)
        });
    }

    pub(super) fn key_picker(&mut self, code: KeyCode) {
        let Some(p) = &mut self.picker else { return };
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('P') => self.picker = None,
            KeyCode::Enter => match p.sel.selected() {
                Some(0) => self.begin_input(Input::NewPlaylist, String::new()),
                Some(i) => {
                    let Some(target) = p.items.get(i - 1).cloned() else { return };
                    if target.has {
                        self.status = format!("Already in {}", target.title);
                        return;
                    }
                    let id = p.track.video_id().unwrap_or_default().to_string();
                    self.picker = None;
                    self.status = format!("Adding to {}…", target.title);
                    self.spawn(move || {
                        let r = ytm::add_to_playlist(&target.playlist_id, &id).map(|_| browse_id_of(&target.playlist_id));
                        Msg::Added(target.title, r)
                    });
                }
                None => {}
            },
            code => {
                let len = p.items.len() + 1;
                list_nav(&mut p.sel, len, code);
            }
        }
    }

    pub(super) fn create_playlist(&mut self, title: String) {
        let Some(p) = self.picker.take() else { return };
        if title.is_empty() {
            return;
        }
        let id = p.track.video_id().unwrap_or_default().to_string();
        self.status = format!("Creating {title}…");
        self.spawn(move || {
            let r = ytm::create_playlist(&title, &id).map(|pid| browse_id_of(&pid));
            Msg::Added(title, r)
        });
    }

    pub(super) fn open_collection(&mut self, c: Collection, lib: bool) {
        if lib {
            self.yt_loading = true;
            // Cached tracks first; the fetch below refreshes them.
            if let Some(t) = self.cache.pages.get(&c.browse_id) {
                self.yt_open_sel.select((!t.is_empty()).then_some(0));
                self.yt_open = Some((c.clone(), t.clone()));
            }
        } else {
            self.searching = true;
        }
        self.status = format!("Opening {}…", c.title);
        self.browse_seq += 1;
        let seq = self.browse_seq;
        self.spawn(move || {
            let r = ytm::browse(&c.browse_id);
            Msg::Browse(seq, lib, c, r)
        });
    }

    /// Drop AI tracks (and shelves left empty) from Discover when hiding is on.
    pub(super) fn hide_ai_discover(&mut self) {
        if !self.cfg.hide_ai {
            return;
        }
        let sel = self.discover_sel.selected().and_then(|i| self.discover.get(i)).map(|d| match d {
            DItem::Track(t) => t.url(),
            DItem::Coll(c) => c.browse_id.clone(),
            DItem::Header(h) => h.clone(),
        });
        let items = std::mem::take(&mut self.discover);
        for d in items {
            if matches!(&d, DItem::Track(t) if self.is_ai(t)) || matches!(&d, DItem::Coll(c) if self.is_ai_coll(c)) {
                continue;
            }
            if matches!(d, DItem::Header(_)) && matches!(self.discover.last(), Some(DItem::Header(_))) {
                self.discover.pop();
            }
            self.discover.push(d);
        }
        if matches!(self.discover.last(), Some(DItem::Header(_))) {
            self.discover.pop();
        }
        let i = sel.and_then(|k| {
            self.discover.iter().position(|d| match d {
                DItem::Track(t) => t.url() == k,
                DItem::Coll(c) => c.browse_id == k,
                DItem::Header(h) => *h == k,
            })
        });
        self.discover_sel.select(i.or((self.discover.len() > 1).then_some(1)));
    }

    pub(super) fn rescan(&mut self) {
        let dir = PathBuf::from(&self.cfg.library_dir);
        self.lib_error = None;
        if self.cfg.library_dir.is_empty() {
            return;
        }
        if let Err(e) = library::check_dir(&dir) {
            self.lib_error = Some(e.clone());
            self.status = e;
            return;
        }
        self.lib_scanning = true;
        self.spawn(move || Msg::Library(library::try_scan(&dir)));
    }

    /// Library tracks matching the filter (indices into `library`), cached.
    pub fn lib_visible(&self) -> &[usize] {
        &self.lib_vis
    }

    pub fn lib_filter(&self) -> &str {
        &self.lib_filter
    }

    pub fn set_lib_filter(&mut self, f: String) {
        if f != self.lib_filter {
            self.lib_filter = f;
            self.refilter_library();
        }
    }

    pub(super) fn set_library(&mut self, l: Vec<Track>) {
        self.lib_search = l.iter().map(|t| format!("{} {} {}", t.title, t.artist, t.album).to_lowercase()).collect();
        self.library = l;
        self.refilter_library();
    }

    pub(super) fn refilter_library(&mut self) {
        let f = self.lib_filter.to_lowercase();
        let words: Vec<&str> = f.split_whitespace().collect();
        self.lib_vis = (0..self.library.len()).filter(|&i| words.iter().all(|w| self.lib_search[i].contains(w))).collect();
    }

    pub(super) fn run_search(&mut self) {
        if self.query.trim().is_empty() {
            return;
        }
        self.search_seq += 1;
        // A page still loading would cover these results when it lands.
        self.browse_seq += 1;
        self.searching = true;
        self.browse = None;
        let (seq, q, kind) = (self.search_seq, self.query.clone(), self.kind);
        self.spawn(move || Msg::Search(seq, ytm::search(&q, kind)));
    }

    pub(super) fn selected_search_track(&self) -> Option<(Vec<Track>, usize)> {
        if let Some((_, tracks)) = &self.browse {
            let i = self.browse_sel.selected()?;
            return (i < tracks.len()).then(|| (tracks.clone(), i));
        }
        if self.discover_mode() {
            // A Discover song's list is its shelf: the songs between the headers around it.
            let i = self.discover_sel.selected().filter(|&i| matches!(self.discover.get(i), Some(DItem::Track(_))))?;
            let start = self.discover[..i].iter().rposition(|d| matches!(d, DItem::Header(_))).map_or(0, |h| h + 1);
            let end = self.discover[i..].iter().position(|d| matches!(d, DItem::Header(_))).map_or(self.discover.len(), |n| i + n);
            let at = self.discover[start..i].iter().filter(|d| matches!(d, DItem::Track(_))).count();
            let shelf = self.discover[start..end].iter().filter_map(|d| if let DItem::Track(t) = d { Some(t.clone()) } else { None }).collect();
            return Some((shelf, at));
        }
        if let Some(Results::Tracks(t)) = &self.results {
            let i = self.search_sel.selected()?;
            return (i < t.len()).then(|| (t.clone(), i));
        }
        None
    }

    pub(super) fn selected_search_collection(&self) -> Option<Collection> {
        if self.browse.is_some() {
            return None;
        }
        if self.discover_mode() {
            return match self.discover_sel.selected().and_then(|i| self.discover.get(i)) {
                Some(DItem::Coll(c)) => Some(c.clone()),
                _ => None,
            };
        }
        match &self.results {
            Some(Results::Collections(cs)) => self.search_sel.selected().and_then(|i| cs.get(i)).cloned(),
            _ => None,
        }
    }

    /// Discover rows include shelf headers; never leave the cursor on one.
    pub(super) fn skip_headers(&mut self, before: usize) {
        let Some(mut i) = self.discover_sel.selected() else { return };
        let len = self.discover.len() as isize;
        let mut dir: isize = if i >= before { 1 } else { -1 };
        let mut flipped = false;
        while matches!(self.discover.get(i), Some(DItem::Header(_))) {
            let n = i as isize + dir;
            if n < 0 || n >= len {
                if flipped {
                    break;
                }
                flipped = true;
                dir = -dir;
                continue;
            }
            i = n as usize;
        }
        self.discover_sel.select(Some(i));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testutil::t;
    use std::sync::mpsc;
    #[test]
    fn library_filter_is_cached_and_follows_changes() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        let mut a = t("Blue Monday");
        a.artist = "New Order".into();
        let mut b = t("Atmosphere");
        b.artist = "Joy Division".into();
        b.album = "Substance".into();
        let mut c = t("Ceremony");
        c.artist = "New Order".into();
        app.set_library(vec![a, b, c]);
        assert_eq!(app.lib_visible(), [0, 1, 2]);
        app.set_lib_filter("new ORDER".into());
        assert_eq!(app.lib_visible(), [0, 2]);
        app.set_lib_filter("order cere".into());
        assert_eq!(app.lib_visible(), [2]);
        app.set_lib_filter("substance".into());
        assert_eq!(app.lib_visible(), [1]);
        // A new scan refilters with the current filter.
        app.set_library(vec![t("x"), t("Substance 1987")]);
        assert_eq!(app.lib_visible(), [1]);
        app.set_lib_filter(String::new());
        assert_eq!(app.lib_visible(), [0, 1]);
    }
}
