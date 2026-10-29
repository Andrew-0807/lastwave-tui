# Keys

Every key has one meaning. Keys in the first three tables work in every view; the
view tables only list keys that the other views don't use. While a text field has focus,
keys type text (see "Typing"); in the help and playlist pop-ups, `Esc` closes.

In code: `global_act` and `view_act` in `src/app/keys.rs`. The `keymap_*` tests check that no
view rebinds a global key and that the help screen (`?`) lists every key.

## Player (always the playing song)

| Key | Action |
|---|---|
| `Space` | Play / pause |
| `n` / `p` | Next / previous song (`p` more than 3 s into a song restarts it) |
| `←` / `→` | Seek 5 s back / forward (in Search, Queue, Library and Lyrics; Equalizer and Settings use them, see below) |
| `,` / `.` | Seek 10 s back / forward |
| `+` (or `=`) / `-` | Volume up / down |
| `w` | Radio from the playing song (replaces what's queued after it) |
| `B` | Flag / unflag the playing song's artist as AI-generated |
| `R` | Radio autoplay on / off |

## App

| Key | Action |
|---|---|
| `1`–`6` | Search, Queue, Library, Lyrics, Equalizer, Settings |
| `Tab` / `Shift+Tab` | Next / previous view |
| `/` | Search YouTube Music (from any view; an empty search shows Discover) |
| `c` | Show the playing song in the Queue |
| `v` | Show / hide the details drawer |
| `L` | Sign in to YouTube Music |
| `u` | Undo the last queue change (up to 10) |
| `?` | Key help |
| `q`, `Ctrl+C` | Quit (`Ctrl+C` also works while typing) |

## Song actions (the highlighted row; the playing song when no list has a highlight)

In Search, Queue and Library these act on the highlighted row. In Lyrics, Equalizer and
Settings, or when the list is empty, they act on the playing song. On a highlighted album,
playlist or artist, the song-only actions say to open it first instead of falling back to
the playing song.

| Key | Action |
|---|---|
| `Enter` | Song in Search results or Discover: play it now, inserted after the playing song (the rest of the queue stays). Song on an album / playlist page or in the Library: play it, with its list from there on as the queue (`u` undoes). Queue: jump to it. Album / playlist / artist: open it. Settings: change the setting |
| `N` | Play next (Queue: move the highlighted song up to play next) |
| `a` | Add to the end of the queue |
| `A` | Add the whole list to the queue |
| `r` | Start a radio from the song |
| `l` | Like / unlike (YouTube Music) |
| `P` | Add to a YouTube Music playlist |

## Lists (Search, Queue, Library, Settings)

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Move |
| `PgUp` / `PgDn` | Move a page |
| `g` `Home` / `G` `End` | Top / bottom |
| `Esc` / `Backspace` | Back: close the album/playlist page, else clear the search (back to Discover) or the Library filter. Settings: cancel a Last.fm connect |
| `S` | Reload: Discover, the search, the open page, your playlists; Library: rescan the folder |
| Mouse | Click selects, double-click = `Enter`, wheel scrolls; wheel over the player changes volume |

## View keys

| View | Key | Action |
|---|---|---|
| Search | `t` | Result type: songs / albums / artists / playlists |
| Queue | `d` / `Delete` | Remove the song |
| Queue | `K` / `J` | Move the song up / down |
| Queue | `X` | Clear the queue (keeps the playing song) |
| Library | `y` | Local files ↔ YouTube Music (the tabs can be clicked too) |
| Library | `f` | Filter local files |
| Library | `o` | Set the music folder |
| Equalizer | `←` / `→` | Select band |
| Equalizer | `↑` `↓` / `k` `j` | Gain up / down |
| Equalizer | `e` | EQ on / off |
| Equalizer | `[` / `]` | Previous / next preset |
| Equalizer | `z` | Reset (flat) |
| Equalizer | `s` | Studio Clarity on / off |
| Equalizer | `{` / `}` | Previous / next Clarity profile |
| Settings | `Enter`, `←` / `→` | Change the setting (account rows: `Enter` twice signs out) |

## Typing

| Key | Action |
|---|---|
| `Enter` | Confirm |
| `Esc` | Cancel |
| `Backspace` | Delete a character |
| `Ctrl+U` | Clear the field |
| `Ctrl+V` | YouTube Music cookies field: sign in with the clipboard |
| Click | Ignored: the text stays until `Enter` or `Esc` |

## Changes from the old keymap

| Key | Before | Now |
|---|---|---|
| `Enter` | Search results and Discover: insert the song after the playing one. Library and album pages: play from here | Unchanged: Search results and Discover insert the song after the playing one and play it; album / playlist pages and the Library play from here (the list becomes the queue, `u` undoes) |
| `l`, `P`, `a`, `N`, `r` | `l`/`P` used the playing song when an album was highlighted; `r` only in Search and YouTube Music playlists; `a`/`N` did nothing without a highlighted song | Highlighted row, else the playing song; an album highlight says to open it. `r` works in every view, and on the playing song it doesn't restart it |
| `l` | Like; in Equalizer: next band; in Settings: change | Like everywhere |
| `h` | Equalizer: previous band; Settings: change | Unbound (use `←`) |
| `c` | Queue: cursor to the playing song; Equalizer: Studio Clarity | Everywhere: show the playing song in the Queue |
| `C` | Equalizer: next Clarity profile | `{` / `}` (previous / next profile) |
| Studio Clarity on/off | `c` | `s` |
| `f` | Search: result type; Library: filter | Library filter only; result type is `t` |
| `i` | Search: edit the search; Library: filter | Unbound (use `/` or `f`) |
| `S` | Discover / playlists / rescan only | Reloads whatever the view shows (also the search and an open page) |
| `L` | Library → YouTube Music only | Any view |
| `Backspace` | Back from an open page only | Same as `Esc` |
| `A` | Did nothing in Discover | Adds the highlighted song's shelf |
| Click while typing | Closed the field and dropped the text (a pasted cookie too) | Ignored; `Enter` / `Esc` end the field |
