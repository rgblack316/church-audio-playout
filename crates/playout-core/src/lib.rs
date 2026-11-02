//! Library scanning, filename parsing and search.
//!
//! Layout assumed: `<root>/<songbook folder>/<title ... page>.<ext>`.
//! The first folder under the root is the songbook. The page number is
//! the trailing number of the file name (rule is configurable).

pub mod meta;

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::UNIX_EPOCH,
};
use walkdir::WalkDir;

pub use meta::Meta;

pub const DEFAULT_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac", "wma"];

/// Default rule: title, optional separator / "p" / "pg" / "page", then 1-4 digits at the end.
pub const DEFAULT_PAGE_PATTERN: &str =
    r"^(?P<title>.*?)[\s_\-\u{2013}#.,]*(?:(?:p|pg|page)\.?\s*)?(?P<page>\d{1,4})\s*$";

/// Songbook name used for files placed directly in the root folder.
pub const ROOT_SONGBOOK: &str = "(Root)";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    pub id: u32,
    /// Full path to the audio file.
    pub path: String,
    pub songbook: String,
    pub title: String,
    pub page: Option<u32>,
    /// Full file name including the extension.
    pub file_name: String,
    pub ext: String,
    /// Title / artist / album stored inside the file, if any.
    pub tag_title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
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
    /// Read ID3 / other tags (slower the first time; results are cached).
    pub read_tags: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            extensions: DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
            page_pattern: DEFAULT_PAGE_PATTERN.to_string(),
            read_tags: true,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    mtime: u64,
    size: u64,
    meta: Meta,
}

/// Remembers tag data per file (keyed by path, modified time and size) so rescans only read
/// files that are new or changed.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MetaCache {
    entries: HashMap<String, CacheEntry>,
}

impl MetaCache {
    /// Load from disk; a missing or damaged file just gives an empty cache.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct Found {
    path: PathBuf,
    songbook: String,
    stem: String,
    file_name: String,
    ext: String,
    mtime: u64,
    size: u64,
}

/// Scan `root` recursively without a tag cache (tags are still read if enabled).
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<Library, String> {
    scan_with_cache(root, opts, &mut MetaCache::default())
}

/// Scan `root` recursively. Returns an error only if the pattern is invalid
/// or the root is not a directory.
pub fn scan_with_cache(root: &Path, opts: &ScanOptions, cache: &mut MetaCache) -> Result<Library, String> {
    let re = Regex::new(&opts.page_pattern).map_err(|e| format!("Bad page pattern: {e}"))?;
    if !root.is_dir() {
        return Err(format!("Not a folder: {}", root.display()));
    }
    let exts: Vec<String> = opts.extensions.iter().map(|e| e.to_lowercase()).collect();

    let mut lib = Library::default();
    let mut found: Vec<Found> = Vec::new();
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
        let (mtime, size) = entry
            .metadata()
            .ok()
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                (t, m.len())
            })
            .unwrap_or((0, 0));
        found.push(Found {
            path: path.to_path_buf(),
            songbook,
            stem: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            file_name: path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            ext,
            mtime,
            size,
        });
    }

    let metas = if opts.read_tags { collect_meta(&found, cache) } else { vec![Meta::default(); found.len()] };

    for (f, meta) in found.into_iter().zip(metas) {
        let (title, page) = parse_stem(&f.stem, &re);
        lib.tracks.push(Track {
            id: lib.tracks.len() as u32,
            path: f.path.display().to_string(),
            songbook: f.songbook,
            title,
            page,
            file_name: f.file_name,
            ext: f.ext,
            tag_title: meta.tag_title,
            artist: meta.artist,
            album: meta.album,
            duration_secs: meta.duration_secs,
        });
    }
    Ok(lib)
}

/// Tag data for every found file, reading only what the cache does not already have,
/// several files at a time. The cache is pruned to the files seen in this scan.
fn collect_meta(found: &[Found], cache: &mut MetaCache) -> Vec<Meta> {
    let key = |f: &Found| f.path.display().to_string();
    let todo: Vec<usize> = (0..found.len())
        .filter(|&i| {
            let f = &found[i];
            !matches!(cache.entries.get(&key(f)), Some(e) if e.mtime == f.mtime && e.size == f.size)
        })
        .collect();

    if !todo.is_empty() {
        let next = AtomicUsize::new(0);
        let done: Mutex<Vec<(usize, Meta)>> = Mutex::new(Vec::with_capacity(todo.len()));
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        std::thread::scope(|scope| {
            for _ in 0..threads.min(todo.len()) {
                scope.spawn(|| loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&i) = todo.get(n) else { break };
                    let m = meta::read_meta(&found[i].path);
                    done.lock().unwrap_or_else(|e| e.into_inner()).push((i, m));
                });
            }
        });
        for (i, meta) in done.into_inner().unwrap_or_else(|e| e.into_inner()) {
            let f = &found[i];
            cache.entries.insert(key(f), CacheEntry { mtime: f.mtime, size: f.size, meta });
        }
    }

    let seen: HashSet<String> = found.iter().map(key).collect();
    cache.entries.retain(|k, _| seen.contains(k));
    found.iter().map(|f| cache.entries.get(&key(f)).map(|e| e.meta.clone()).unwrap_or_default()).collect()
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
    /// * A purely numeric word matches the page number exactly (or appears in the text below).
    /// * Any other word matches as a case-insensitive substring of the title or of the tag
    ///   title, artist or album.
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
                let mut hay = t.title.to_lowercase();
                for extra in [&t.tag_title, &t.artist, &t.album].into_iter().flatten() {
                    hay.push('\n');
                    hay.push_str(&extra.to_lowercase());
                }
                words.iter().all(|w| {
                    if let Ok(n) = w.parse::<u32>() {
                        t.page == Some(n) || hay.contains(w.as_str())
                    } else {
                        hay.contains(w.as_str())
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

    // ---------------------------------------------------------------- metadata

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn reads_tags_from_every_format() {
        for ext in ["mp3", "flac", "ogg", "m4a", "wma"] {
            let m = meta::read_meta(&fixtures().join(format!("tagged.{ext}")));
            assert_eq!(m.tag_title.as_deref(), Some(format!("Tag Title {ext}").as_str()), "{ext} title");
            assert_eq!(m.artist.as_deref(), Some("Tag Artist"), "{ext} artist");
            assert_eq!(m.album.as_deref(), Some("Tag Album"), "{ext} album");
            let d = m.duration_secs.unwrap_or(0.0);
            assert!((1.2..1.9).contains(&d), "{ext} duration {d}");
        }
    }

    #[test]
    fn untagged_and_unreadable_files_give_empty_meta() {
        let m = meta::read_meta(&fixtures().join("untagged.mp3"));
        assert_eq!((m.tag_title, m.artist, m.album), (None, None, None));
        assert!(m.duration_secs.unwrap_or(0.0) > 0.5);

        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.mp3");
        fs::write(&junk, b"definitely not audio").unwrap();
        assert_eq!(meta::read_meta(&junk), Meta::default());
        let junk_wma = dir.path().join("junk.wma");
        fs::write(&junk_wma, [0x30u8, 0x26, 0xB2, 0x75, 0, 0, 0]).unwrap(); // truncated ASF header
        assert_eq!(meta::read_meta(&junk_wma), Meta::default());
        assert_eq!(meta::read_meta(&dir.path().join("missing.mp3")), Meta::default());
    }

    fn library_with_fixtures() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("Hymnal");
        fs::create_dir_all(&book).unwrap();
        fs::copy(fixtures().join("tagged.mp3"), book.join("Amazing Grace 123.mp3")).unwrap();
        fs::copy(fixtures().join("tagged.wma"), book.join("Holy Holy 77.wma")).unwrap();
        fs::copy(fixtures().join("untagged.mp3"), book.join("Plain Song 5.mp3")).unwrap();
        dir
    }

    #[test]
    fn scan_fills_tag_fields_and_search_uses_them() {
        let dir = library_with_fixtures();
        let lib = scan(dir.path(), &ScanOptions::default()).unwrap();
        let t = lib.tracks.iter().find(|t| t.page == Some(123)).unwrap();
        assert_eq!(t.file_name, "Amazing Grace 123.mp3");
        assert_eq!(t.title, "Amazing Grace");
        assert_eq!(t.tag_title.as_deref(), Some("Tag Title mp3"));
        assert_eq!(t.artist.as_deref(), Some("Tag Artist"));
        let plain = lib.tracks.iter().find(|t| t.page == Some(5)).unwrap();
        assert!(plain.artist.is_none() && plain.duration_secs.is_some());

        let q = |text: &str| Query { text: text.into(), songbook: None, limit: None };
        assert_eq!(lib.search(&q("artist")).len(), 2, "tagged files match on artist");
        assert_eq!(lib.search(&q("tag album")).len(), 2);
        assert_eq!(lib.search(&q("title wma")).len(), 1, "tag title is searchable");
        assert_eq!(lib.search(&q("plain")).len(), 1, "file-name search still works");
        assert_eq!(lib.search(&q("artist 123")).len(), 1, "page and tag words combine");

        let off = scan(dir.path(), &ScanOptions { read_tags: false, ..Default::default() }).unwrap();
        assert!(off.tracks.iter().all(|t| t.artist.is_none() && t.duration_secs.is_none()));
    }

    #[test]
    fn cache_skips_unchanged_files_and_prunes_deleted_ones() {
        let dir = library_with_fixtures();
        let opts = ScanOptions::default();
        let mut cache = MetaCache::default();
        scan_with_cache(dir.path(), &opts, &mut cache).unwrap();
        assert_eq!(cache.len(), 3);

        // Poison one cached entry: an unchanged file must keep the cached value...
        let key = dir.path().join("Hymnal").join("Plain Song 5.mp3").display().to_string();
        cache.entries.get_mut(&key).unwrap().meta.artist = Some("FROM CACHE".into());
        let lib = scan_with_cache(dir.path(), &opts, &mut cache).unwrap();
        assert_eq!(
            lib.tracks.iter().find(|t| t.page == Some(5)).unwrap().artist.as_deref(),
            Some("FROM CACHE")
        );

        // ...but a changed file (new size) is read again.
        let p = dir.path().join("Hymnal").join("Plain Song 5.mp3");
        let mut bytes = fs::read(&p).unwrap();
        bytes.extend_from_slice(&[0; 16]);
        fs::write(&p, bytes).unwrap();
        let lib = scan_with_cache(dir.path(), &opts, &mut cache).unwrap();
        assert!(lib.tracks.iter().find(|t| t.page == Some(5)).unwrap().artist.is_none());

        // Persisting and reloading keeps the entries; deleting a file prunes it.
        let file = dir.path().join("cache.json");
        cache.save(&file).unwrap();
        let mut reloaded = MetaCache::load(&file);
        assert_eq!(reloaded.len(), 3);
        fs::remove_file(dir.path().join("Hymnal").join("Holy Holy 77.wma")).unwrap();
        scan_with_cache(dir.path(), &opts, &mut reloaded).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert!(MetaCache::load(&dir.path().join("nope.json")).is_empty());
    }
}
