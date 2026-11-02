//! Audio file metadata (title, artist, album, length).
//!
//! `lofty` reads ID3 / Vorbis / MP4 / FLAC / WAV tags. WMA (ASF) is not covered by lofty, so a
//! small reader for the ASF header lives here.

use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Title stored in the file's tags (not the file name).
    pub tag_title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().trim_matches('\0').trim().to_string()).filter(|v| !v.is_empty())
}

/// Read whatever metadata the file has. Never fails: unreadable or untagged files give an
/// empty `Meta`, and a malformed file cannot take the scan down with it.
pub fn read_meta(path: &Path) -> Meta {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let result = catch_unwind(AssertUnwindSafe(|| match ext.as_str() {
        "wma" | "asf" => read_asf(path),
        _ => read_lofty(path),
    }));
    let mut m = result.ok().flatten().unwrap_or_default();
    m.tag_title = clean(m.tag_title.take());
    m.artist = clean(m.artist.take());
    m.album = clean(m.album.take());
    m.duration_secs = m.duration_secs.filter(|d| d.is_finite() && *d > 0.0);
    m
}

fn read_lofty(path: &Path) -> Option<Meta> {
    use lofty::{file::AudioFile, file::TaggedFileExt, tag::Accessor};
    let tagged = lofty::read_from_path(path).ok()?;
    let duration = tagged.properties().duration().as_secs_f64();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    Some(Meta {
        tag_title: tag.and_then(|t| t.title().map(|c| c.into_owned())),
        artist: tag.and_then(|t| t.artist().map(|c| c.into_owned())),
        album: tag.and_then(|t| t.album().map(|c| c.into_owned())),
        duration_secs: Some(duration),
    })
}

// ---------------------------------------------------------------- ASF / WMA

const ASF_HEADER: [u8; 16] = [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
const ASF_CONTENT_DESC: [u8; 16] = [0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
const ASF_EXT_CONTENT_DESC: [u8; 16] = [0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50];
const ASF_FILE_PROPS: [u8; 16] = [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}
fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

pub(crate) fn read_asf(path: &Path) -> Option<Meta> {
    let mut f = File::open(path).ok()?;
    let mut head = [0u8; 30];
    f.read_exact(&mut head).ok()?;
    if head[..16] != ASF_HEADER {
        return None;
    }
    let size = u64le(&head, 16)?;
    if !(30..=8 * 1024 * 1024).contains(&size) {
        return None;
    }
    let count = u32::from_le_bytes(head[24..28].try_into().ok()?);
    let mut buf = vec![0u8; (size - 30) as usize];
    f.read_exact(&mut buf).ok()?;

    let mut meta = Meta::default();
    let mut album_artist = None;
    let mut pos = 0usize;
    for _ in 0..count {
        let guid = buf.get(pos..pos + 16)?;
        let osz = u64le(&buf, pos + 16)? as usize;
        if osz < 24 || pos + osz > buf.len() {
            break;
        }
        let body = &buf[pos + 24..pos + osz];
        if guid == ASF_CONTENT_DESC {
            // five lengths (title, author, copyright, description, rating), then the strings
            let lens: Vec<usize> = (0..5).filter_map(|i| u16le(body, i * 2).map(|l| l as usize)).collect();
            if lens.len() == 5 {
                let mut at = 10;
                let mut parts = Vec::new();
                for l in &lens {
                    parts.push(body.get(at..at + l).map(utf16).unwrap_or_default());
                    at += l;
                }
                meta.tag_title = Some(parts[0].clone());
                meta.artist = Some(parts[1].clone());
            }
        } else if guid == ASF_EXT_CONTENT_DESC {
            let n = u16le(body, 0).unwrap_or(0) as usize;
            let mut at = 2;
            for _ in 0..n {
                let Some(nlen) = u16le(body, at).map(|v| v as usize) else { break };
                let Some(name) = body.get(at + 2..at + 2 + nlen).map(utf16) else { break };
                at += 2 + nlen;
                let (Some(vtype), Some(vlen)) = (u16le(body, at), u16le(body, at + 2)) else { break };
                let vlen = vlen as usize;
                let Some(value) = body.get(at + 4..at + 4 + vlen) else { break };
                at += 4 + vlen;
                if vtype == 0 {
                    match name.as_str() {
                        "WM/AlbumTitle" => meta.album = Some(utf16(value)),
                        "WM/AlbumArtist" => album_artist = Some(utf16(value)),
                        _ => {}
                    }
                }
            }
        } else if guid == ASF_FILE_PROPS {
            // File ID(16) File size(8) Creation date(8) Packets(8) Play duration(8) Send duration(8) Preroll(8)
            if let (Some(play), Some(preroll_ms)) = (u64le(body, 40), u64le(body, 56)) {
                meta.duration_secs = Some(play as f64 / 1e7 - preroll_ms as f64 / 1000.0);
            }
        }
        pos += osz;
    }
    if meta.artist.as_deref().map_or(true, |a| a.trim().is_empty()) {
        meta.artist = album_artist;
    }
    Some(meta)
}
