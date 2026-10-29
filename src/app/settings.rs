//! The Settings rows and what they change, audio output settings, and the
//! account flows (Last.fm connect, YouTube Music sign-in / sign-out).

use super::{App, Cache, Input, Msg, View, CACHE_FILE};
use crate::{
    aifilter,
    art,
    config::{self, REPLAYGAIN, YTDL_FORMATS},
    lastfm,
    ytm,
};
use serde_json::{json, Value};

/// One row of the Settings view. Rows are matched by name; `SETTINGS`
/// gives their order on screen, so reordering them is a one-line change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingRow {
    OutputDevice,
    BitPerfect,
    ReplayGain,
    Gapless,
    YtAudio,
    RadioAutoplay,
    LibraryDir,
    LfmKey,
    LfmSecret,
    LfmAccount,
    YtAccount,
    YtPlayback,
    CoverImages,
    SkipAi,
    HideAi,
}

/// The Settings rows in screen order. Every `SettingRow` appears once.
pub const SETTINGS: &[SettingRow] = &[
    SettingRow::OutputDevice,
    SettingRow::BitPerfect,
    SettingRow::ReplayGain,
    SettingRow::Gapless,
    SettingRow::YtAudio,
    SettingRow::RadioAutoplay,
    SettingRow::LibraryDir,
    SettingRow::LfmKey,
    SettingRow::LfmSecret,
    SettingRow::LfmAccount,
    SettingRow::YtAccount,
    SettingRow::YtPlayback,
    SettingRow::CoverImages,
    SettingRow::SkipAi,
    SettingRow::HideAi,
];

impl SettingRow {
    pub fn label(self) -> &'static str {
        match self {
            SettingRow::OutputDevice => "Output device",
            SettingRow::BitPerfect => "Bit-perfect (exclusive)",
            SettingRow::ReplayGain => "ReplayGain",
            SettingRow::Gapless => "Gapless",
            SettingRow::YtAudio => "YouTube audio",
            SettingRow::RadioAutoplay => "Radio autoplay",
            SettingRow::LibraryDir => "Library folder",
            SettingRow::LfmKey => "Last.fm API key",
            SettingRow::LfmSecret => "Last.fm API secret",
            SettingRow::LfmAccount => "Last.fm account",
            SettingRow::YtAccount => "YouTube Music account",
            SettingRow::YtPlayback => "Use account for playback",
            SettingRow::CoverImages => "Cover images",
            SettingRow::SkipAi => "Skip AI songs",
            SettingRow::HideAi => "Hide AI songs in Discover",
        }
    }

    /// Where the row is in `SETTINGS`.
    pub fn index(self) -> usize {
        SETTINGS.iter().position(|&r| r == self).unwrap_or(0)
    }
}

/// System clipboard text, via the platform's own tool (no extra crate).
fn read_clipboard() -> Result<String, String> {
    use std::process::Command;
    #[cfg(windows)]
    let out = {
        use std::os::windows::process::CommandExt;
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Get-Clipboard -Raw"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
    };
    #[cfg(target_os = "macos")]
    let out = Command::new("pbpaste").output();
    #[cfg(all(unix, not(target_os = "macos")))]
    let out = Command::new("wl-paste")
        .arg("--no-newline")
        .output()
        .or_else(|_| Command::new("xclip").args(["-selection", "clipboard", "-o"]).output());
    let out = out.map_err(|e| format!("could not read the clipboard: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err("The clipboard is empty — copy your music.youtube.com cookies first".into());
    }
    Ok(text)
}

fn mask(s: &str) -> String {
    if s.is_empty() {
        "(not set)".into()
    } else {
        format!("{}…", s.chars().take(6).collect::<String>())
    }
}

impl App {
    pub(super) fn start_yt_login(&mut self) {
        self.view = View::Settings;
        self.set_sel = SettingRow::YtAccount.index();
        self.begin_input(Input::YtCookies, String::new());
        self.status = "Copy your music.youtube.com cookies, then press Enter here (reads the clipboard) — or paste them / a cookies.txt path first".into();
    }

    /// `None` reads the cookies from the system clipboard.
    pub(super) fn submit_yt_cookies(&mut self, raw: Option<String>) {
        self.status = "Signing in…".into();
        self.spawn(move || {
            let previous = config::load_cookies();
            let r = raw.map_or_else(read_clipboard, Ok).and_then(|raw| ytm::normalize_cookies(&raw)).and_then(|c| {
                ytm::set_auth(Some(c.clone()));
                ytm::account_name().map(|n| (c, n))
            });
            if r.is_err() {
                ytm::set_auth(previous);
            }
            Msg::YtLogin(r)
        });
    }

    pub(super) fn yt_sign_out(&mut self) {
        ytm::set_auth(None);
        config::save_cookies(None);
        self.cfg.yt_user.clear();
        self.cfg.save();
        self.yt_lists.clear();
        self.yt_open = None;
        self.liked.clear();
        self.picker = None;
        self.cache = Cache::default();
        config::save(CACHE_FILE, &self.cache);
        self.mpv_set("ytdl-raw-options", "");
        self.status = "Signed out of YouTube Music".into();
        self.load_home();
    }

    /// Push output-chain settings into the running mpv.
    pub(super) fn apply_audio(&mut self, reload_ao: bool) {
        let Some(m) = &self.mpv else { return };
        m.set("audio-device", self.cfg.audio_device.clone());
        m.set("audio-exclusive", self.cfg.exclusive);
        m.set("gapless-audio", if self.cfg.gapless { "yes" } else { "no" });
        m.set("ytdl-format", self.cfg.ytdl_format.clone());
        if self.cfg.exclusive {
            m.set("replaygain", "no");
            m.set("af", "");
        } else {
            m.set("replaygain", self.cfg.replaygain.clone());
            m.set("af", self.cfg.eq.filter());
        }
        if reload_ao {
            m.cmd(json!(["ao-reload"]));
        }
        self.cfg.save();
    }

    pub(super) fn apply_eq(&mut self) {
        if self.cfg.exclusive && self.cfg.eq.active() {
            self.status = "EQ is bypassed while bit-perfect mode is on".into();
        } else {
            self.mpv_set("af", self.cfg.eq.filter());
        }
        self.cfg.save();
    }

    pub fn bit_perfect(&self) -> bool {
        let rate = |v: &Value| v["samplerate"].as_u64();
        self.cfg.exclusive
            && self.af.is_empty()
            && (self.volume - 100.0).abs() < 0.01
            && rate(&self.params).is_some()
            && rate(&self.params) == rate(&self.out_params)
    }

    /// The highlighted Settings row.
    pub(super) fn setting_row(&self) -> SettingRow {
        SETTINGS[self.set_sel.min(SETTINGS.len() - 1)]
    }

    pub fn setting_value(&self, row: SettingRow) -> String {
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        match row {
            SettingRow::OutputDevice => self
                .devices
                .iter()
                .find(|(n, _)| *n == self.cfg.audio_device)
                .map(|(n, d)| if d.is_empty() { n.clone() } else { d.clone() })
                .unwrap_or_else(|| self.cfg.audio_device.clone()),
            SettingRow::BitPerfect => on(self.cfg.exclusive),
            SettingRow::ReplayGain => self.cfg.replaygain.clone(),
            SettingRow::Gapless => on(self.cfg.gapless),
            SettingRow::YtAudio => YTDL_FORMATS
                .iter()
                .find(|(f, _)| *f == self.cfg.ytdl_format)
                .map(|(_, l)| l.to_string())
                .unwrap_or_else(|| self.cfg.ytdl_format.clone()),
            SettingRow::RadioAutoplay => on(self.cfg.radio),
            SettingRow::LibraryDir if self.cfg.library_dir.is_empty() => "(not set — Enter to choose your music folder)".into(),
            SettingRow::LibraryDir => self.cfg.library_dir.clone(),
            SettingRow::LfmKey => mask(&self.cfg.lastfm_api_key),
            SettingRow::LfmSecret => mask(&self.cfg.lastfm_api_secret),
            SettingRow::YtAccount => {
                if ytm::signed_in() {
                    format!("signed in as {} (Enter twice to sign out)", self.cfg.yt_user)
                } else {
                    "not signed in (copy cookies, then Enter twice)".into()
                }
            }
            SettingRow::YtPlayback => on(self.cfg.yt_account_playback),
            SettingRow::CoverImages => {
                let using = self.covers.as_ref().map(|c| c.mode_name()).unwrap_or_default();
                if self.cfg.image_protocol == "auto" {
                    format!("auto (using {using})")
                } else {
                    self.cfg.image_protocol.clone()
                }
            }
            SettingRow::SkipAi => {
                let n = aifilter::count();
                let listed = if n > 0 { format!("{n} known AI artists") } else { "list not downloaded yet".into() };
                let mine = self.cfg.ai_flagged.len();
                let mine = if mine > 0 { format!(", {mine} flagged by you") } else { String::new() };
                format!("{} ({listed}{mine}; B flags the playing artist)", on(self.cfg.skip_ai))
            }
            SettingRow::HideAi => format!("{} (Quick picks and other Discover shelves)", on(self.cfg.hide_ai)),
            SettingRow::LfmAccount => {
                if !self.cfg.lastfm_user.is_empty() && self.lfm_ready() {
                    format!("connected as {} (Enter twice to disconnect)", self.cfg.lastfm_user)
                } else if self.lfm_busy {
                    "contacting Last.fm…".into()
                } else if self.lfm_auth.is_some() {
                    "waiting for approval (Enter checks, Esc cancels)".into()
                } else {
                    "not connected (Enter to connect)".into()
                }
            }
        }
    }

    /// Enter in Settings. Signed-in account rows sign out on a second Enter.
    pub(super) fn settings_enter(&mut self) {
        let armed = self.confirm_signout.take();
        let row = self.setting_row();
        if !self.signed_in_row(row) {
            self.change_setting(false);
        } else if armed == Some(row) {
            if row == SettingRow::YtAccount {
                self.yt_sign_out();
            } else {
                self.lfm_disconnect();
            }
        } else {
            self.confirm_signout = Some(row);
            let what = if row == SettingRow::YtAccount { "sign out of YouTube Music" } else { "disconnect Last.fm" };
            self.status = format!("Press Enter again to {what} (any other key cancels)");
        }
    }

    /// An account row (Last.fm / YouTube Music) that is currently signed in.
    pub(super) fn signed_in_row(&self, row: SettingRow) -> bool {
        match row {
            SettingRow::LfmAccount => self.lfm_ready(),
            SettingRow::YtAccount => ytm::signed_in(),
            _ => false,
        }
    }

    pub(super) fn change_setting(&mut self, back: bool) {
        fn cycle(cur: usize, len: usize, back: bool) -> usize {
            if back { (cur + len - 1) % len } else { (cur + 1) % len }
        }
        match self.setting_row() {
            SettingRow::OutputDevice => {
                if self.devices.is_empty() {
                    return;
                }
                let i = self.devices.iter().position(|(n, _)| *n == self.cfg.audio_device).unwrap_or(0);
                self.cfg.audio_device = self.devices[cycle(i, self.devices.len(), back)].0.clone();
                self.apply_audio(true);
            }
            SettingRow::BitPerfect => {
                self.cfg.exclusive = !self.cfg.exclusive;
                if self.cfg.exclusive {
                    self.mpv_set("volume", 100);
                    self.status = "Bit-perfect: EQ/ReplayGain bypassed, volume locked to 100% — use your DAC/amp volume".into();
                }
                self.apply_audio(true);
            }
            SettingRow::ReplayGain => {
                let i = REPLAYGAIN.iter().position(|r| *r == self.cfg.replaygain).unwrap_or(0);
                self.cfg.replaygain = REPLAYGAIN[cycle(i, REPLAYGAIN.len(), back)].into();
                self.apply_audio(false);
            }
            SettingRow::Gapless => {
                self.cfg.gapless = !self.cfg.gapless;
                self.apply_audio(false);
            }
            SettingRow::YtAudio => {
                let i = YTDL_FORMATS.iter().position(|(f, _)| *f == self.cfg.ytdl_format).unwrap_or(0);
                self.cfg.ytdl_format = YTDL_FORMATS[cycle(i, YTDL_FORMATS.len(), back)].0.into();
                self.apply_audio(false);
            }
            SettingRow::RadioAutoplay => {
                self.cfg.radio = !self.cfg.radio;
                self.cfg.save();
                self.maybe_radio();
            }
            SettingRow::LibraryDir => self.begin_input(Input::LibDir, self.cfg.library_dir.clone()),
            SettingRow::LfmKey => self.begin_input(Input::LfmKey, self.cfg.lastfm_api_key.clone()),
            SettingRow::LfmSecret => self.begin_input(Input::LfmSecret, self.cfg.lastfm_api_secret.clone()),
            // Signed-in account rows are handled (with confirmation) in settings_enter.
            SettingRow::LfmAccount if !self.lfm_ready() => self.lastfm_step(),
            SettingRow::YtAccount if !ytm::signed_in() => self.start_yt_login(),
            SettingRow::YtPlayback => {
                self.cfg.yt_account_playback = !self.cfg.yt_account_playback;
                self.cfg.save();
                self.mpv_set("ytdl-raw-options", config::ytdl_raw_options(&self.cfg));
                if self.cfg.yt_account_playback && !ytm::signed_in() {
                    self.status = "Sign in to YouTube Music first; until then playback stays anonymous".into();
                }
            }
            SettingRow::CoverImages => {
                let i = art::PROTOCOLS.iter().position(|p| *p == self.cfg.image_protocol).unwrap_or(0);
                self.cfg.image_protocol = art::PROTOCOLS[cycle(i, art::PROTOCOLS.len(), back)].into();
                self.cfg.save();
                if let Some(c) = &mut self.covers {
                    c.set_protocol(&self.cfg.image_protocol);
                }
            }
            SettingRow::SkipAi => {
                self.cfg.skip_ai = !self.cfg.skip_ai;
                self.cfg.save();
                if self.cfg.skip_ai {
                    self.status = "Downloading the AI artist list…".into();
                    self.spawn(|| Msg::AiList(aifilter::refresh()));
                }
                self.sync_next();
            }
            SettingRow::HideAi => {
                self.cfg.hide_ai = !self.cfg.hide_ai;
                self.cfg.save();
                if self.cfg.hide_ai {
                    self.hide_ai_discover();
                    self.save_cache();
                    if aifilter::count() == 0 {
                        self.spawn(|| Msg::AiList(aifilter::refresh()));
                    }
                } else {
                    // Hidden songs are gone from the list; fetch it again.
                    self.load_home();
                }
            }
            _ => {}
        }
    }

    pub(super) fn lfm_disconnect(&mut self) {
        self.cfg.lastfm_session.clear();
        self.cfg.lastfm_user.clear();
        self.cfg.save();
        self.lfm_bad_session = false;
        self.status = "Last.fm disconnected".into();
    }

    pub(super) fn lastfm_step(&mut self) {
        if self.lfm_busy {
            self.status = "Still waiting for Last.fm… (Esc cancels)".into();
            return;
        }
        if self.cfg.lastfm_api_key.is_empty() || self.cfg.lastfm_api_secret.is_empty() {
            self.status = "Enter a Last.fm API key + secret first (last.fm/api/account/create)".into();
            return;
        }
        let (k, s, seq) = (self.cfg.lastfm_api_key.clone(), self.cfg.lastfm_api_secret.clone(), self.lfm_seq);
        self.lfm_busy = true;
        match self.lfm_auth.clone() {
            None => {
                self.status = "Asking Last.fm for an approval link…".into();
                self.spawn(move || Msg::LfmToken(seq, lastfm::get_token(&k, &s)));
            }
            // The browser was opened when the token came; now just ask whether it was approved.
            Some((tok, _)) => {
                self.status = "Checking with Last.fm…".into();
                self.spawn(move || Msg::LfmSession(seq, lastfm::get_session(&k, &s, &tok)));
            }
        }
    }

    /// Drop a Last.fm connect in progress; replies still on their way are ignored.
    pub(super) fn lfm_cancel(&mut self) {
        self.lfm_seq += 1;
        self.lfm_busy = false;
        self.lfm_auth = None;
        self.status = "Last.fm connect cancelled".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::keys::Act;
    use std::sync::mpsc;
    #[test]
    fn lastfm_connect_flow() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        let err = |code| Err(lastfm::LfmError { code, msg: format!("Last.fm: error {code}") });
        app.lfm_auth = Some(("tok".into(), "https://www.last.fm/api/auth/?api_key=k&token=tok".into()));
        // Not approved yet: keep the token, Enter checks again.
        app.lfm_busy = true;
        app.handle_msg(Msg::LfmSession(app.lfm_seq, err(14)));
        assert!(app.lfm_auth.is_some() && !app.lfm_busy);
        // Expired or invalid: start over instead of asking again forever.
        for code in [15, 4] {
            app.lfm_auth = Some(("tok".into(), "url".into()));
            app.handle_msg(Msg::LfmSession(app.lfm_seq, err(code)));
            assert!(app.lfm_auth.is_none(), "error {code}");
        }
        // While a request is out, Enter doesn't send another (or open another tab).
        app.cfg.lastfm_api_key = "k".into();
        app.cfg.lastfm_api_secret = "s".into();
        app.lfm_busy = true;
        app.lastfm_step();
        assert!(app.status.starts_with("Still waiting"));
        // Esc cancels; a reply to the cancelled attempt is ignored.
        let seq = app.lfm_seq;
        app.view = View::Settings;
        app.run(Act::Back);
        assert!(!app.lfm_busy);
        app.handle_msg(Msg::LfmToken(seq, Ok("late".into())));
        assert!(app.lfm_auth.is_none());
    }

    #[test]
    fn every_setting_row_listed_once() {
        use SettingRow::*;
        let all = [OutputDevice, BitPerfect, ReplayGain, Gapless, YtAudio, RadioAutoplay, LibraryDir, LfmKey, LfmSecret, LfmAccount, YtAccount, YtPlayback, CoverImages, SkipAi, HideAi];
        for r in all {
            // Stops compiling when a variant is added, until it is listed above too.
            match r {
                OutputDevice | BitPerfect | ReplayGain | Gapless | YtAudio | RadioAutoplay | LibraryDir | LfmKey | LfmSecret | LfmAccount | YtAccount | YtPlayback | CoverImages | SkipAi | HideAi => {}
            }
            assert_eq!(SETTINGS.iter().filter(|&&s| s == r).count(), 1, "{r:?} must be in SETTINGS once");
            assert_eq!(SETTINGS[r.index()], r);
        }
        assert_eq!(SETTINGS.len(), all.len());
        // `all` is every variant in declaration order.
        assert!(all.iter().enumerate().all(|(i, &r)| r as usize == i));
        let labels: std::collections::HashSet<&str> = SETTINGS.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), SETTINGS.len(), "labels are unique");
    }
}
