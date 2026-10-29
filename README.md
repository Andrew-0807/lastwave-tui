# LastWave TUI

A terminal port of [LastWave Desktop](https://github.com/Clash-Projects/LastWave-Desktop) in Rust.
It uses the same audio engine (mpv) and no webview.

## Requirements

- `mpv` on PATH (or set `mpv_path` in the config)
- `yt-dlp` on PATH, **kept up to date** (`pip install -U yt-dlp` / `scoop update yt-dlp`).
  YouTube streams 403 with old builds.

## Install

### 1. Install the Rust toolchain

Install Rust (stable, 1.80 or newer) from <https://rustup.rs>:

- **Windows**: download and run `rustup-init.exe`. The MSVC toolchain also needs the
  "Desktop development with C++" workload from the Visual Studio Build Tools.
- **Linux / macOS**: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`

### 2. Install the runtime dependencies

| | Windows (scoop) | Windows (winget) | macOS (Homebrew) | Debian/Ubuntu | Arch |
|---|---|---|---|---|---|
| mpv | `scoop install mpv` | `winget install mpv.net` or a build from [mpv.io](https://mpv.io/installation/) | `brew install mpv` | `sudo apt install mpv` | `sudo pacman -S mpv` |
| yt-dlp | `scoop install yt-dlp` | `winget install yt-dlp.yt-dlp` | `brew install yt-dlp` | `pip install -U yt-dlp` | `sudo pacman -S yt-dlp` |

Check that both are on PATH:

```
mpv --version
yt-dlp --version
```

Distro packages of yt-dlp are often too old. If YouTube playback fails with 403 errors, update it
with `pip install -U yt-dlp` or `yt-dlp -U`.

## Build

```
git clone https://github.com/Andrew-0807/lastwave-tui.git
cd lastwave-tui
cargo build --release
```

The binary is written to `target/release/lastwave-tui` (`target\release\lastwave-tui.exe` on Windows). Run it:

```
cargo run --release
```

To install it onto your PATH (`~/.cargo/bin`):

```
cargo install --path .
lastwave-tui
```

For a quick debug build use `cargo build` (output in `target/debug/`). The release profile uses LTO,
so the first release build takes a few minutes.

Use a terminal with true color support. For real cover images use one that speaks the Kitty, iTerm2
or Sixel graphics protocol (WezTerm, Kitty, iTerm2, Windows Terminal 1.22+); others get a block-character fallback.

## Features

| LastWave Desktop | TUI |
|---|---|
| YouTube Music search (songs/albums/artists/playlists), album & playlist pages | yes: InnerTube web client, plus pasted YouTube links |
| Radio / endless autoplay | yes: `r` starts a radio from a song, `w` from the playing song, `R` toggles autoplay |
| Queue with gapless + prefetch | yes: mpv's playlist is kept as [current, next] |
| Local hi-res library (FLAC/ALAC/WAV/DSD…) | yes: tags via lofty, bit depth / sample rate shown |
| WASAPI exclusive / bit-perfect output, device selection | yes: Settings view; shows **BIT-PERFECT** when in == out |
| 10-band EQ, presets, Studio Clarity | yes: same presets and mpv `af` chain |
| ReplayGain | yes |
| Synced lyrics (LRCLIB) | yes |
| Last.fm now-playing + scrobbling | yes: bring your own API key/secret |
| Lossless addon servers | no (needs LastWave's private client secret) |
| Discover (YouTube Music home, personalised when signed in) | yes: shown when the search bar is empty |
| YouTube Music sign-in, Liked Music + playlists in Library | yes: paste browser cookies (no webview) |
| Like songs, add songs to playlists / new playlists | yes: `l` likes/unlikes, `P` opens the playlist picker (synced to your YouTube Music account) |
| Skip AI-generated songs | yes: Settings → Skip AI songs, using the community list from [spotify-ai-blocker](https://github.com/CennoxX/spotify-ai-blocker); `B` flags/unflags the playing artist; Settings → Hide AI songs in Discover removes them from Quick picks and other shelves |
| Discord, MPRIS/SMTC, downloads | not yet |

## Keys

`?` shows the key list. Common keys: `1-6` switch views, `/` search from anywhere (an empty search shows Discover), `Enter` play,
`a` add to queue, `l` like, `P` add to playlist, `Space` play/pause, `n`/`p` next/previous, `←`/`→` seek, `+`/`-` volume, `q` quit.

## Files

The config and saved queue are in `%APPDATA%\lastwave-tui\` (`~/.config/lastwave-tui/` on Linux).
`config.json` also accepts `extra_mpv_args`.

Cover art: real images (Kitty, iTerm2, Sixel) where the terminal supports them. Inside herdr the default is **kitty+text**: covers are drawn with sextant block characters, with a Kitty image placed on top. herdr forwards that image only to Kitty-capable clients (WezTerm), so they show the full-resolution cover while others (Rio) show the text version. Change it in Settings → Cover images.

`lastwave-tui --discover` prints the Discover shelves. `lastwave-tui --selftest <album>` runs a headless check of search, album browsing, lyrics and mpv playback.
