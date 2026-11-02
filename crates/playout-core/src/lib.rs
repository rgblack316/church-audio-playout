//! Library scanning, filename parsing and search.
//!
//! Layout assumed: `<root>/<songbook folder>/<title ... page>.<ext>`.
//! The first folder under the root is the songbook. The page number is
//! the trailing number of the file name (rule is configurable).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;
use walkdir::WalkDir;

pub const DEFAULT_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac", "wma"];

/// Default rule: title, optional separator / "p" / "pg" / "page", then 1-4 digits at the end.
pub const DEFAULT_PAGE_PATTERN: &str =
    r"^(?P<title>.*?)[\s_\-\u{2013}#.,]*(?:(?:p|pg|page)\.?\s*)?(?P<page>\d{1,4})\s*$";

/// Songbook name used for files placed directly in the root folder.
pub const ROOT_SONGBOOK: &str = "(Root)";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    /// Full path to the audio file.
    pub path: String,
    pub songbook: String,
    pub title: String,
    pub page: Option<u32>,
    pub file_name: String,
    pub ext: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Problem {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub extensions: Vec<String>,
    pub page_pattern: String,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            extensions: DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
            page_pattern: DEFAULT_PAGE_PATTERN.to_string(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Library {
    pub tracks: Vec<Track>,
    /// Files / folders that could not be read.
    pub problems: Vec<Problem>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub text: String,
    /// Exact songbook name, or None for all songbooks.
    #[serde(default)]
    pub songbook: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Split a file stem into (title, page) using the page regex.
/// Falls back to (stem, None) when there is no match or the title would be empty.
pub fn parse_stem(stem: &str, re: &Regex) -> (String, Option<u32>) {
    if let Some(caps) = re.captures(stem) {
        let page = caps.name("page").and_then(|m| m.as_str().parse::<u32>().ok());
        let title = caps
            .name("title")
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        if page.is_some() {
            if title.is_empty() {
                // File named only "123": keep the stem as the title.
                return (stem.trim().to_string(), page);
            }
            return (title, page);
        }
    }
    (stem.trim().to_string(), None)
}

/// Scan `root` recursively. Returns an error only if the pattern is invalid
/// or the root is not a directory.
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<Library, String> {
    let re = Regex::new(&opts.page_pattern).map_err(|e| format!("Bad page pattern: {e}"))?;
    if !root.is_dir() {
        return Err(format!("Not a folder: {}", root.display()));
    }
    let exts: Vec<String> = opts.extensions.iter().map(|e| e.to_lowercase()).collect();

    let mut lib = Library::default();
    for entry in WalkDir::new(root).follow_links(true).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                lib.problems.push(Problem {
                    path: e.path().map(|p| p.display().to_string()).unwrap_or_default(),
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if !exts.contains(&ext) {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let songbook = match rel.components().count() {
            0 | 1 => ROOT_SONGBOOK.to_string(),
            _ => rel
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_else(|| ROOT_SONGBOOK.to_string()),
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (title, page) = parse_stem(&stem, &re);
        lib.tracks.push(Track {
            id: lib.tracks.len() as u32,
            path: path.display().to_string(),
            songbook,
            title,
            page,
            file_name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ext,
        });
    }
    Ok(lib)
}

impl Library {
    /// Sorted, de-duplicated songbook names.
    pub fn songbooks(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tracks.iter().map(|t| t.songbook.clone()).collect();
        v.sort_by_key(|s| s.to_lowercase());
        v.dedup();
        v
    }

    pub fn get(&self, id: u32) -> Option<&Track> {
        self.tracks.get(id as usize)
    }

    /// Tracks without a detected page number (useful for cleaning up names).
    pub fn without_page(&self) -> Vec<&Track> {
        self.tracks.iter().filter(|t| t.page.is_none()).collect()
    }

    /// Search by page number and/or words in the title, optionally within one songbook.
    ///
    /// * Every whitespace-separated word in `text` must match.
    /// * A purely numeric word matches the page number exactly.
    /// * Any other word matches as a case-insensitive substring of the title.
    /// * An empty `text` returns everything in the songbook (or all).
    pub fn search(&self, q: &Query) -> Vec<&Track> {
        let words: Vec<String> = q
            .text
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .collect();

        let mut out: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|t| match &q.songbook {
                Some(sb) if !sb.is_empty() => &t.songbook == sb,
                _ => true,
            })
            .filter(|t| {
                let title = t.title.to_lowercase();
                words.iter().all(|w| {
                    if let Ok(n) = w.parse::<u32>() {
                        t.page == Some(n) || title.contains(w.as_str())
                    } else {
                        title.contains(w.as_str())
                    }
                })
            })
            .collect();

        out.sort_by(|a, b| {
            a.songbook
                .to_lowercase()
                .cmp(&b.songbook.to_lowercase())
                .then(a.page.cmp(&b.page))
                .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        if let Some(limit) = q.limit {
            out.truncate(limit);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn re() -> Regex {
        Regex::new(DEFAULT_PAGE_PATTERN).unwrap()
    }

    #[test]
    fn parses_common_name_styles() {
        let cases = [
            ("Amazing Grace 123", "Amazing Grace", Some(123)),
            ("Amazing Grace - 123", "Amazing Grace", Some(123)),
            ("Amazing_Grace_123", "Amazing_Grace", Some(123)),
            ("Amazing Grace p. 45", "Amazing Grace", Some(45)),
            ("Amazing Grace pg45", "Amazing Grace", Some(45)),
            ("Amazing Grace #7", "Amazing Grace", Some(7)),
            ("How Great Thou Art", "How Great Thou Art", None),
            ("123", "123", Some(123)),
            ("Psalm 23 Setting 410", "Psalm 23 Setting", Some(410)),
            ("Joy to the World (verse 1) 100", "Joy to the World (verse 1)", Some(100)),
        ];
        for (stem, title, page) in cases {
            let (t, p) = parse_stem(stem, &re());
            assert_eq!((t.as_str(), p), (title, page), "stem: {stem}");
        }
    }

    fn make_library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (sb, name) in [
            ("Hymnal 1991", "Amazing Grace 123.mp3"),
            ("Hymnal 1991", "How Great Thou Art 77.WAV"),
            ("Hymnal 1991", "Notes.txt"),
            ("Praise Songs", "Amazing Grace 12.flac"),
            ("Praise Songs", "No Page Here.wma"),
            ("Praise Songs/Nested", "Deep Song 5.m4a"),
        ] {
            let d = root.join(sb);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(name), b"x").unwrap();
        }
        fs::write(root.join("Loose 9.mp3"), b"x").unwrap();
        let lib = scan(root, &ScanOptions::default()).unwrap();
        (dir, lib)
    }

    #[test]
    fn scans_songbooks_and_skips_non_audio() {
        let (_d, lib) = make_library();
        assert_eq!(lib.tracks.len(), 6);
        assert_eq!(
            lib.songbooks(),
            vec!["(Root)", "Hymnal 1991", "Praise Songs"]
        );
        // nested folder still belongs to top-level songbook
        let deep = lib.tracks.iter().find(|t| t.title == "Deep Song").unwrap();
        assert_eq!(deep.songbook, "Praise Songs");
        assert_eq!(lib.without_page().len(), 1);
    }

    #[test]
    fn searches_by_page_and_songbook() {
        let (_d, lib) = make_library();
        let q = |text: &str, sb: Option<&str>| Query {
            text: text.into(),
            songbook: sb.map(String::from),
            limit: None,
        };
        // page only, all songbooks
        assert_eq!(lib.search(&q("12", None)).len(), 1);
        // same title in two books, narrowed by songbook
        assert_eq!(lib.search(&q("amazing", None)).len(), 2);
        let r = lib.search(&q("amazing", Some("Hymnal 1991")));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].page, Some(123));
        // page within songbook
        let r = lib.search(&q("77", Some("Hymnal 1991")));
        assert_eq!(r[0].title, "How Great Thou Art");
        // empty text lists the whole songbook, sorted by page
        let r = lib.search(&q("", Some("Hymnal 1991")));
        assert_eq!(r.iter().map(|t| t.page).collect::<Vec<_>>(), vec![Some(77), Some(123)]);
        // mixed words
        assert_eq!(lib.search(&q("grace 123", None)).len(), 1);
        // limit
        let mut lq = q("", None);
        lq.limit = Some(2);
        assert_eq!(lib.search(&lq).len(), 2);
    }

    #[test]
    fn bad_inputs_return_errors() {
        assert!(scan(Path::new("/definitely/not/here"), &ScanOptions::default()).is_err());
        let opts = ScanOptions { page_pattern: "(".into(), ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(dir.path(), &opts).is_err());
    }
}
