//! Local hi-res library scan (FLAC/ALAC/WAV/DSD…), tags via lofty.

use crate::model::{Source, Track};
use lofty::prelude::*;
use std::path::Path;
use walkdir::WalkDir;

const EXTS: &[&str] = &[
    "flac", "alac", "m4a", "mp3", "aac", "ogg", "opus", "wav", "aif", "aiff", "ape", "wv", "dsf", "dff", "mka", "wma",
];

/// Why a library folder can't be scanned, in words the status line can show.
pub fn check_dir(dir: &Path) -> Result<(), String> {
    if dir.as_os_str().is_empty() {
        return Err("Library folder is not set (Settings → Library folder)".into());
    }
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => std::fs::read_dir(dir)
            .map(|_| ())
            .map_err(|e| format!("Library folder {} can't be read: {e}", dir.display())),
        Ok(_) => Err(format!("Library path {} is a file, not a folder", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("Library folder {} does not exist", dir.display()))
        }
        Err(e) => Err(format!("Library folder {}: {e}", dir.display())),
    }
}

/// Every audio file under `dir`, sorted by artist / album / disc / track;
/// an unusable folder is an error the status line can show. A file whose tags make the reader fail (or panic) is still listed, with
/// its file name as title, so one damaged file can't break the scan.
pub fn try_scan(dir: &Path) -> Result<Vec<Track>, String> {
    check_dir(dir)?;
    let mut items: Vec<((String, String, u32, u32), Track)> = WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| EXTS.contains(&x.to_ascii_lowercase().as_str()))
        })
        .map(|e| read(e.path()))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(items.into_iter().map(|(_, t)| t).collect())
}

fn read(path: &Path) -> ((String, String, u32, u32), Track) {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|s| s.to_string_lossy().to_uppercase()).unwrap_or_default();
    let mut t = Track {
        source: Source::Local(path.to_path_buf()),
        title: stem,
        artist: "Unknown artist".into(),
        album: String::new(),
        duration: 0,
        quality: ext.clone(),
        art: String::new(),
    };
    let (mut disc, mut no) = (0, 0);
    // lofty can panic on malformed files; treat that like a read error.
    let tagged = std::panic::catch_unwind(|| lofty::read_from_path(path)).ok().and_then(Result::ok);
    if let Some(f) = tagged {
        let p = f.properties();
        t.duration = p.duration().as_secs() as u32;
        let rate = p.sample_rate().map(|r| format!("{:.1}", r as f64 / 1000.0).trim_end_matches(".0").to_string());
        t.quality = match (p.bit_depth(), rate) {
            (Some(b), Some(r)) => format!("{ext} {b}/{r}"),
            (None, Some(r)) => format!("{ext} {r}k{}", p.audio_bitrate().map(|b| format!(" {b}kbps")).unwrap_or_default()),
            _ => ext,
        };
        if let Some(tag) = f.primary_tag().or_else(|| f.first_tag()) {
            if let Some(v) = tag.title() {
                t.title = v.into_owned();
            }
            if let Some(v) = tag.artist() {
                t.artist = v.into_owned();
            }
            if let Some(v) = tag.album() {
                t.album = v.into_owned();
            }
            disc = tag.disk().unwrap_or(0);
            no = tag.track().unwrap_or(0);
        }
    }
    ((t.artist.to_lowercase(), t.album.to_lowercase(), disc, no), t)
}
