#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use playout_core::{
    read_pending, scan_quick, Library, MetaCache, Pending, Problem, Query, ScanOptions, Track, DEFAULT_PAGE_PATTERN,
};
use serde::{Deserialize, Serialize};
use playout_audio::{decode_file, Bank, Engine, RecStatus, Recorder, Snapshot};
use std::{fs, path::Path, path::PathBuf, sync::Arc, sync::Mutex};
use tauri::{AppHandle, Manager, State};

const DEFAULT_SEARCH_LIMIT: usize = 500;
const DEFAULT_COLUMNS: usize = 4;
const DEFAULT_ROWS: usize = 2;
const MAX_GRID: usize = 8;
const MAX_FAV_ROWS: usize = 2;
const DEFAULT_FAV_ROWS: usize = 1;
/// Favorite slots are addressed as row * MAX_GRID + column, so a favorite keeps its place
/// when the number of columns changes.
const FAV_SLOTS: usize = MAX_GRID * MAX_FAV_ROWS;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    music_dir: Option<String>,
    #[serde(default = "default_pattern")]
    page_pattern: String,
    #[serde(default = "default_columns")]
    pad_columns: usize,
    #[serde(default = "default_rows")]
    pad_rows: usize,
    /// Allow several pads to play at the same time.
    #[serde(default = "default_overlap")]
    overlap: bool,
    /// Rows of favorite buttons (0-2); they always use the same column count as the pad grid.
    #[serde(default = "default_fav_rows")]
    fav_rows: usize,
    /// Read ID3 / other tags (artist, album, length) when scanning.
    #[serde(default = "default_overlap")]
    read_tags: bool,
    /// Length of the "Fade all" fade-out, in seconds.
    #[serde(default = "default_fade_secs")]
    fade_secs: f32,
    /// Folder recordings are saved in (None = Documents\\Church Recordings).
    #[serde(default)]
    record_dir: Option<String>,
    /// Chosen recording input (None = the Windows default input; "Name@@3" = channels 3-4).
    #[serde(default)]
    record_device: Option<String>,
    /// Level recordings automatically when they stop.
    #[serde(default = "default_overlap")]
    auto_level: bool,
    /// Loudness target for leveling, in LUFS.
    #[serde(default = "default_target_lufs")]
    target_lufs: f64,
    /// Chosen audio output device (None = the Windows default).
    #[serde(default)]
    audio_device: Option<String>,
    /// Favorite songs by slot (row * 8 + column).
    #[serde(default)]
    favorites: Vec<Option<Favorite>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Favorite {
    path: String,
    title: String,
    sub: String,
}

fn default_fav_rows() -> usize {
    DEFAULT_FAV_ROWS
}

fn default_columns() -> usize {
    DEFAULT_COLUMNS
}
fn default_rows() -> usize {
    DEFAULT_ROWS
}
fn default_target_lufs() -> f64 {
    -16.0
}

fn default_fade_secs() -> f32 {
    3.0
}
fn default_overlap() -> bool {
    true
}

fn default_pattern() -> String {
    DEFAULT_PAGE_PATTERN.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            music_dir: None,
            page_pattern: default_pattern(),
            pad_columns: DEFAULT_COLUMNS,
            pad_rows: DEFAULT_ROWS,
            overlap: true,
            fav_rows: DEFAULT_FAV_ROWS,
            read_tags: true,
            fade_secs: default_fade_secs(),
            record_dir: None,
            record_device: None,
            audio_device: None,
            auto_level: true,
            target_lufs: default_target_lufs(),
            favorites: vec![None; FAV_SLOTS],
        }
    }
}

#[derive(Default)]
struct AppState {
    initialized: bool,
    settings: Settings,
    library: Library,
    meta_cache: MetaCache,
    cache_path: Option<PathBuf>,
    /// Bumped by every successful scan; background tag reading stops when it changes.
    scan_gen: u64,
    /// (files done, files to read) for the current background tag pass.
    tag_progress: (usize, usize),
}

type Shared = Mutex<AppState>;

/// Only one library scan runs at a time.
struct ScanLock(Mutex<()>);

#[derive(Serialize)]
struct Summary {
    music_dir: Option<String>,
    track_count: usize,
    songbooks: Vec<String>,
    without_page: Vec<Track>,
    unreadable: Vec<Problem>,
    read_tags: bool,
}

fn summarize(state: &AppState) -> Summary {
    Summary {
        music_dir: state.settings.music_dir.clone(),
        track_count: state.library.tracks.len(),
        songbooks: state.library.songbooks(),
        without_page: state.library.without_page().into_iter().cloned().collect(),
        unreadable: state.library.problems.clone(),
        read_tags: state.settings.read_tags,
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(settings_path(app)?, json).map_err(|e| e.to_string())
}

fn ensure_init(app: &AppHandle, state: &mut AppState) {
    if !state.initialized {
        state.settings = load_settings(app);
        state.settings.favorites.resize(FAV_SLOTS, None);
        state.cache_path = app.path().app_config_dir().ok().map(|d| d.join("tag_cache.json"));
        if let Some(p) = &state.cache_path {
            state.meta_cache = MetaCache::load(p);
        }
        state.initialized = true;
    }
}

/// Scan the music folder with the current settings and store the result.
/// The state lock is NOT held while files are read, so the rest of the app stays responsive.
fn do_scan(app: &AppHandle) -> Result<(), String> {
    let scan_lock = app.state::<ScanLock>();
    let _one_at_a_time = scan_lock.0.lock().unwrap_or_else(|e| e.into_inner());
    let shared = app.state::<Shared>();
    let (dir, opts, cache) = {
        let mut state = shared.lock().unwrap();
        ensure_init(app, &mut state);
        (
            state.settings.music_dir.clone(),
            ScanOptions {
                page_pattern: state.settings.page_pattern.clone(),
                read_tags: state.settings.read_tags,
                ..ScanOptions::default()
            },
            state.meta_cache.clone(),
        )
    };
    // Phase 1: list the files (fast, never opens an audio file) and publish them at once.
    let (library, pending) = match &dir {
        Some(d) => scan_quick(Path::new(d), &opts, &cache)?,
        None => (Library::default(), Vec::new()),
    };
    let generation = {
        let mut state = shared.lock().unwrap();
        state.scan_gen += 1;
        state.tag_progress = (0, pending.len());
        state.meta_cache.prune(&library);
        state.library = library;
        state.scan_gen
    };
    // Phase 2: read tags in the background; the browser fills in as chunks complete.
    if !pending.is_empty() {
        let app = app.clone();
        std::thread::spawn(move || read_tags_in_background(app, generation, pending));
    }
    Ok(())
}

fn read_tags_in_background(app: AppHandle, generation: u64, pending: Vec<Pending>) {
    let shared = app.state::<Shared>();
    for chunk in pending.chunks(12) {
        if shared.lock().unwrap().scan_gen != generation {
            return; // a newer scan replaced this library
        }
        let metas = read_pending(chunk);
        let mut state = shared.lock().unwrap();
        if state.scan_gen != generation {
            return;
        }
        let state = &mut *state;
        state.library.apply_meta(&mut state.meta_cache, chunk, metas);
        state.tag_progress.0 += chunk.len();
    }
    let state = shared.lock().unwrap();
    if state.scan_gen == generation {
        if let Some(p) = &state.cache_path {
            let _ = state.meta_cache.save(p);
        }
    }
}

#[derive(Serialize)]
struct TagProgress {
    done: usize,
    total: usize,
}

/// How far the background tag reading has got (total 0 = nothing to read).
#[tauri::command]
fn get_scan_progress(shared: State<'_, Shared>) -> TagProgress {
    let (done, total) = shared.lock().unwrap().tag_progress;
    TagProgress { done, total }
}

fn summary_now(shared: &Shared) -> Summary {
    summarize(&shared.lock().unwrap())
}

/// Current settings and library summary (loads settings from disk on first call).
#[tauri::command]
fn get_state(app: AppHandle, shared: State<'_, Shared>) -> Summary {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    summarize(&state)
}

/// (Re)scan the music folder. Runs off the main thread so the window stays responsive.
#[tauri::command(async)]
fn rescan(app: AppHandle, shared: State<'_, Shared>) -> Result<Summary, String> {
    do_scan(&app)?;
    Ok(summary_now(&shared))
}

/// Run `change` on the settings, rescan, and keep the change only if the scan worked.
fn change_and_rescan(
    app: &AppHandle,
    shared: &Shared,
    change: impl FnOnce(&mut Settings),
) -> Result<Summary, String> {
    let previous = {
        let mut state = shared.lock().unwrap();
        ensure_init(app, &mut state);
        let previous = state.settings.clone();
        change(&mut state.settings);
        previous
    };
    if let Err(e) = do_scan(app) {
        shared.lock().unwrap().settings = previous;
        return Err(e);
    }
    let state = shared.lock().unwrap();
    save_settings(app, &state.settings)?;
    Ok(summarize(&state))
}

#[tauri::command(async)]
fn set_music_dir(app: AppHandle, shared: State<'_, Shared>, path: String) -> Result<Summary, String> {
    change_and_rescan(&app, &shared, |s| s.music_dir = Some(path))
}

#[tauri::command(async)]
fn set_page_pattern(app: AppHandle, shared: State<'_, Shared>, pattern: String) -> Result<Summary, String> {
    change_and_rescan(&app, &shared, |s| {
        s.page_pattern = if pattern.trim().is_empty() { default_pattern() } else { pattern };
    })
}

#[tauri::command(async)]
fn set_read_tags(app: AppHandle, shared: State<'_, Shared>, enabled: bool) -> Result<Summary, String> {
    change_and_rescan(&app, &shared, |s| s.read_tags = enabled)
}

#[tauri::command]
fn get_page_pattern(app: AppHandle, shared: State<'_, Shared>) -> String {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    state.settings.page_pattern.clone()
}

#[tauri::command]
fn search(shared: State<'_, Shared>, mut query: Query) -> Vec<Track> {
    let state = shared.lock().unwrap();
    if query.limit.is_none() {
        query.limit = Some(DEFAULT_SEARCH_LIMIT);
    }
    state.library.search(&query).into_iter().cloned().collect()
}

// ---------- playback ----------

/// Name shown on a pad: the tag title when the file has one, else the title from the file name.
fn pad_label(track: &Track) -> String {
    match track.tag_title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => track.title.clone(),
    }
}

/// Second line on a pad: "Artist \u{b7} Songbook \u{b7} p. 123" (parts that are missing are left out).
fn pad_sub(track: &Track) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(a) = track.artist.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        parts.push(a.to_string());
    }
    parts.push(track.songbook.clone());
    if let Some(p) = track.page {
        parts.push(format!("p. {p}"));
    }
    parts.join(" \u{b7} ")
}

#[tauri::command]
fn get_pads(engine: State<'_, Engine>) -> Snapshot {
    engine.snapshot()
}

/// Decode a library track fully into memory and put it on a pad or favorite slot.
/// Favorites are also remembered between launches.
#[tauri::command(async)]
fn load_pad(
    app: AppHandle,
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    bank: Bank,
    pad: usize,
    track_id: u32,
) -> Result<(), String> {
    let track = {
        let state = shared.lock().unwrap();
        state.library.get(track_id).cloned().ok_or("Track not found; try searching again")?
    };
    engine.check_loadable(bank, pad)?;
    engine.set_loading(bank, pad, true);
    let decoded = match decode_file(Path::new(&track.path)) {
        Ok(d) => d,
        Err(e) => {
            engine.set_loading(bank, pad, false);
            return Err(format!("Cannot play \"{}\": {e}", track.file_name));
        }
    };
    let sub = pad_sub(&track);
    engine.load(bank, pad, Arc::new(decoded), pad_label(&track), sub.clone())?;
    if bank == Bank::Fav {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        state.settings.favorites[pad] = Some(Favorite {
            path: track.path.clone(),
            title: pad_label(&track),
            sub,
        });
        save_settings(&app, &state.settings)?;
    }
    Ok(())
}

#[tauri::command]
fn play_pad(engine: State<'_, Engine>, bank: Bank, pad: usize) -> Result<(), String> {
    engine.play(bank, pad)
}

#[tauri::command]
fn stop_pad(engine: State<'_, Engine>, bank: Bank, pad: usize) {
    engine.stop(bank, pad)
}

#[tauri::command]
fn stop_all(engine: State<'_, Engine>) {
    engine.stop_all()
}

/// Fade everything that is playing out over the configured time, then stop it.
#[tauri::command]
fn fade_all(app: AppHandle, shared: State<'_, Shared>, engine: State<'_, Engine>) {
    let secs = {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        state.settings.fade_secs
    };
    engine.fade_all(secs);
}

#[tauri::command]
fn get_fade_secs(app: AppHandle, shared: State<'_, Shared>) -> f32 {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    state.settings.fade_secs
}

#[tauri::command]
fn set_fade_secs(app: AppHandle, shared: State<'_, Shared>, secs: f32) -> Result<f32, String> {
    if !secs.is_finite() || !(0.5..=30.0).contains(&secs) {
        return Err("Fade length must be between 0.5 and 30 seconds".into());
    }
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    state.settings.fade_secs = secs;
    save_settings(&app, &state.settings)?;
    Ok(secs)
}

/// Empty a pad. For a favorite this also forgets the saved song.
#[tauri::command]
fn clear_pad(
    app: AppHandle,
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    bank: Bank,
    pad: usize,
) -> Result<(), String> {
    engine.clear(bank, pad)?;
    if bank == Bank::Fav {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        if let Some(slot) = state.settings.favorites.get_mut(pad) {
            *slot = None;
        }
        save_settings(&app, &state.settings)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct AudioDevices {
    devices: Vec<playout_audio::DeviceEntry>,
    /// The saved choice (None = system default).
    selected: Option<String>,
}

#[tauri::command(async)]
fn list_audio_devices(app: AppHandle, shared: State<'_, Shared>) -> AudioDevices {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    AudioDevices {
        devices: playout_audio::list_output_devices(state.settings.audio_device.as_deref()),
        selected: state.settings.audio_device.clone(),
    }
}

/// Text listing every device the system reports (for troubleshooting a missing output).
#[tauri::command(async)]
fn get_audio_diagnostics() -> String {
    playout_audio::describe_devices()
}

/// Switch the output device (None = system default). Stops anything playing.
#[tauri::command(async)]
fn set_audio_device(
    app: AppHandle,
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    name: Option<String>,
) -> Result<(), String> {
    let name = name.filter(|n| !n.trim().is_empty());
    {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        state.settings.audio_device = name.clone();
        save_settings(&app, &state.settings)?;
    }
    engine.set_device(name);
    Ok(())
}

#[tauri::command]
fn restart_audio(engine: State<'_, Engine>) {
    engine.restart_audio()
}

// ---------- recording ----------

/// Characters Windows does not allow in file names are replaced; names are kept short.
fn clean_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    cleaned.trim_matches('.').chars().take(80).collect::<String>().trim().to_string()
}

/// `<dir>/11-08-2026 Sermon.wav`; a number is added if that file already exists.
fn recording_path(dir: &Path, name: &str, stamp: &str) -> PathBuf {
    let name = clean_file_name(name);
    let base = if name.is_empty() { stamp.to_string() } else { format!("{stamp} {name}") };
    let mut path = dir.join(format!("{base}.wav"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base} ({n}).wav"));
        n += 1;
    }
    path
}

fn record_dir_of(app: &AppHandle, settings: &Settings) -> PathBuf {
    match settings.record_dir.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(d) => PathBuf::from(d),
        None => app
            .path()
            .document_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("Church Recordings"),
    }
}

#[derive(Serialize)]
struct RecordSettings {
    dir: String,
    device: Option<String>,
}

#[tauri::command]
fn get_record_settings(app: AppHandle, shared: State<'_, Shared>) -> RecordSettings {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    RecordSettings {
        dir: record_dir_of(&app, &state.settings).display().to_string(),
        device: state.settings.record_device.clone(),
    }
}

#[derive(Serialize)]
struct InputDevices {
    devices: Vec<playout_audio::DeviceEntry>,
    selected: Option<String>,
}

#[tauri::command(async)]
fn list_input_devices(app: AppHandle, shared: State<'_, Shared>) -> InputDevices {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    InputDevices {
        devices: playout_audio::list_input_devices(state.settings.record_device.as_deref()),
        selected: state.settings.record_device.clone(),
    }
}

#[tauri::command]
fn set_record_device(app: AppHandle, shared: State<'_, Shared>, name: Option<String>) -> Result<(), String> {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    state.settings.record_device = name.filter(|n| !n.trim().is_empty());
    save_settings(&app, &state.settings)
}

#[tauri::command]
fn set_record_dir(app: AppHandle, shared: State<'_, Shared>, path: String) -> Result<String, String> {
    let path = path.trim().to_string();
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    if !path.is_empty() {
        fs::create_dir_all(&path).map_err(|e| format!("Cannot use that folder: {e}"))?;
    }
    state.settings.record_dir = (!path.is_empty()).then_some(path);
    save_settings(&app, &state.settings)?;
    Ok(record_dir_of(&app, &state.settings).display().to_string())
}

#[tauri::command(async)]
fn start_recording(
    app: AppHandle,
    shared: State<'_, Shared>,
    recorder: State<'_, Recorder>,
    name: Option<String>,
) -> Result<String, String> {
    let (dir, device) = {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        (record_dir_of(&app, &state.settings), state.settings.record_device.clone())
    };
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let stamp = chrono::Local::now().format("%m-%d-%Y").to_string();
    let path = recording_path(&dir, name.as_deref().unwrap_or(""), &stamp);
    recorder.start(device.as_deref(), path.clone())?;
    Ok(path.display().to_string())
}

#[derive(Debug, Clone, Default, Serialize)]
struct LevelStatus {
    /// "idle", "working", "done" or "skipped"/"error" (see `message`).
    state: &'static str,
    progress: f32,
    file: Option<String>,
    message: Option<String>,
    report: Option<playout_audio::level::LevelReport>,
}

struct LevelJob(Mutex<LevelStatus>);

fn set_level(job: &LevelJob, f: impl FnOnce(&mut LevelStatus)) {
    f(&mut job.0.lock().unwrap());
}

/// Level `path` in the background and record the outcome in `LevelJob`.
fn spawn_level(app: AppHandle, path: PathBuf, target: f64) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    {
        let job = app.state::<LevelJob>();
        if path.with_file_name(format!(
            "{} (part 2).wav",
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        ))
        .exists()
        {
            set_level(&job, |l| {
                *l = LevelStatus {
                    state: "skipped",
                    file: name.clone(),
                    message: Some("Recording has several parts, so it was not leveled".into()),
                    ..Default::default()
                }
            });
            return;
        }
        set_level(&job, |l| *l = LevelStatus { state: "working", file: name.clone(), ..Default::default() });
    }
    std::thread::spawn(move || {
        let out = playout_audio::level::leveled_path(&path);
        let result = {
            let job = app.state::<LevelJob>();
            let mut last = -1.0f32;
            playout_audio::level::level_wav(&path, &out, target, &mut |p| {
                if p - last >= 0.01 || p >= 1.0 {
                    last = p;
                    set_level(&job, |l| l.progress = p);
                }
            })
        };
        let job = app.state::<LevelJob>();
        set_level(&job, |l| {
            *l = match result {
                Ok(r) => LevelStatus {
                    state: "done",
                    progress: 1.0,
                    file: out.file_name().map(|n| n.to_string_lossy().into_owned()),
                    message: None,
                    report: Some(r),
                },
                Err(e) => LevelStatus { state: "error", file: name.clone(), message: Some(e), ..Default::default() },
            }
        });
    });
}

#[tauri::command(async)]
fn stop_recording(app: AppHandle, shared: State<'_, Shared>, recorder: State<'_, Recorder>) -> Option<String> {
    let path = recorder.stop()?;
    let (auto, target) = {
        let state = shared.lock().unwrap();
        (state.settings.auto_level, state.settings.target_lufs)
    };
    if auto {
        spawn_level(app, path.clone(), target);
    } else {
        set_level(&app.state::<LevelJob>(), |l| *l = LevelStatus { state: "idle", ..Default::default() });
    }
    Some(path.display().to_string())
}

#[derive(Serialize)]
struct LevelSettings {
    auto: bool,
    target: f64,
}

#[tauri::command]
fn get_level_settings(app: AppHandle, shared: State<'_, Shared>) -> LevelSettings {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    LevelSettings { auto: state.settings.auto_level, target: state.settings.target_lufs }
}

#[tauri::command]
fn set_level_settings(app: AppHandle, shared: State<'_, Shared>, auto: bool, target: f64) -> Result<(), String> {
    if !(-30.0..=-10.0).contains(&target) {
        return Err("Choose a target between -30 and -10 LUFS".into());
    }
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    state.settings.auto_level = auto;
    state.settings.target_lufs = target;
    save_settings(&app, &state.settings)
}

#[tauri::command]
fn get_level_status(job: State<'_, LevelJob>) -> LevelStatus {
    job.0.lock().unwrap().clone()
}

#[derive(Serialize)]
struct RecInfo {
    #[serde(flatten)]
    status: RecStatus,
    free_bytes: Option<u64>,
}

/// Recording state for the UI: time, levels, file size and free disk space.
#[tauri::command]
fn get_rec_status(app: AppHandle, shared: State<'_, Shared>, recorder: State<'_, Recorder>) -> RecInfo {
    let status = recorder.status();
    let dir = {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        record_dir_of(&app, &state.settings)
    };
    let existing = dir.ancestors().find(|p| p.exists()).map(Path::to_path_buf);
    let free_bytes = existing.and_then(|p| fs2::available_space(p).ok());
    RecInfo { status, free_bytes }
}

#[tauri::command]
fn open_recordings_folder(app: AppHandle, shared: State<'_, Shared>) -> Result<(), String> {
    let dir = {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        record_dir_of(&app, &state.settings)
    };
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    std::process::Command::new("explorer").arg(&dir).spawn().map_err(|e| e.to_string())?;
    #[cfg(not(windows))]
    let _ = dir;
    Ok(())
}

// ---------- favorites ----------

fn visible_fav_slots(s: &Settings) -> Vec<usize> {
    let l = layout_of(s);
    (0..l.fav_rows)
        .flat_map(|r| (0..l.columns).map(move |c| r * MAX_GRID + c))
        .collect()
}

/// Decode saved favorites that are on screen and not loaded yet, a few at a time.
/// Used at start-up, after a grid change, and to retry failures.
fn preload_favorites(app: &AppHandle) {
    let shared = app.state::<Shared>();
    let engine = app.state::<Engine>();
    let work: Vec<(usize, Favorite)> = {
        let mut state = shared.lock().unwrap();
        ensure_init(app, &mut state);
        visible_fav_slots(&state.settings)
            .into_iter()
            .filter_map(|slot| {
                state.settings.favorites[slot]
                    .clone()
                    .filter(|_| !engine.is_busy_or_loaded(Bank::Fav, slot))
                    .map(|f| (slot, f))
            })
            .collect()
    };
    for (slot, _) in &work {
        engine.set_loading(Bank::Fav, *slot, true);
    }
    let engine: &Engine = &engine;
    for chunk in work.chunks(4) {
        std::thread::scope(|scope| {
            for (slot, fav) in chunk {
                scope.spawn(move || match decode_file(Path::new(&fav.path)) {
                    Ok(d) => {
                        if engine
                            .load(Bank::Fav, *slot, Arc::new(d), fav.title.clone(), fav.sub.clone())
                            .is_err()
                        {
                            engine.set_loading(Bank::Fav, *slot, false);
                        }
                    }
                    Err(e) => engine.set_error(Bank::Fav, *slot, fav.title.clone(), e),
                });
            }
        });
    }
}

/// Retry favorites that are missing or failed (e.g. after choosing a different music folder).
#[tauri::command(async)]
fn reload_favorites(app: AppHandle) {
    preload_favorites(&app);
}

// ---------- pad layout ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Layout {
    columns: usize,
    rows: usize,
    fav_rows: usize,
    overlap: bool,
}

fn layout_of(s: &Settings) -> Layout {
    Layout {
        columns: s.pad_columns.clamp(1, MAX_GRID),
        rows: s.pad_rows.clamp(1, MAX_GRID),
        fav_rows: s.fav_rows.min(MAX_FAV_ROWS),
        overlap: s.overlap,
    }
}

fn apply_layout(engine: &Engine, l: &Layout) -> Result<(), String> {
    engine.set_pad_count(l.columns * l.rows)?;
    engine.set_overlap(l.overlap);
    Ok(())
}

/// Saved layout; also configures the engine to match (called once by the UI at start-up).
#[tauri::command]
fn get_layout(app: AppHandle, shared: State<'_, Shared>, engine: State<'_, Engine>) -> Layout {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    let l = layout_of(&state.settings);
    let _ = apply_layout(&engine, &l);
    if state.settings.audio_device.is_some() {
        engine.set_device(state.settings.audio_device.clone());
    }
    l
}

#[tauri::command]
fn set_layout(
    app: AppHandle,
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    columns: usize,
    rows: usize,
    fav_rows: usize,
    overlap: bool,
) -> Result<Layout, String> {
    if !(1..=MAX_GRID).contains(&columns) || !(1..=MAX_GRID).contains(&rows) {
        return Err(format!("Columns and rows must be between 1 and {MAX_GRID}"));
    }
    if fav_rows > MAX_FAV_ROWS {
        return Err(format!("Favorite rows must be between 0 and {MAX_FAV_ROWS}"));
    }
    let l = {
        let mut state = shared.lock().unwrap();
        ensure_init(&app, &mut state);
        let l = Layout { columns, rows, fav_rows, overlap };
        apply_layout(&engine, &l)?;
        state.settings.pad_columns = columns;
        state.settings.pad_rows = rows;
        state.settings.fav_rows = fav_rows;
        state.settings.overlap = overlap;
        save_settings(&app, &state.settings)?;
        l
    };
    // Favorites that just became visible need loading.
    let handle = app.clone();
    std::thread::spawn(move || preload_favorites(&handle));
    Ok(l)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Shared::new(AppState::default()))
        .manage(ScanLock(Mutex::new(())))
        .manage(Recorder::new())
        .manage(LevelJob(Mutex::new(LevelStatus { state: "idle", ..Default::default() })))
        .on_window_event(|window, event| {
            // Closing the app mid-recording still finishes the WAV file properly.
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                window.app_handle().state::<Recorder>().stop();
            }
        })
        .manage(Engine::new(DEFAULT_COLUMNS * DEFAULT_ROWS, FAV_SLOTS))
        .setup(|app| {
            // Load saved favorites into memory in the background so they fire instantly.
            let handle = app.handle().clone();
            std::thread::spawn(move || preload_favorites(&handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_scan_progress,
            rescan,
            set_music_dir,
            set_page_pattern,
            set_read_tags,
            get_page_pattern,
            search,
            get_pads,
            load_pad,
            play_pad,
            stop_pad,
            stop_all,
            fade_all,
            get_fade_secs,
            set_fade_secs,
            clear_pad,
            restart_audio,
            list_audio_devices,
            set_audio_device,
            get_audio_diagnostics,
            get_record_settings,
            list_input_devices,
            set_record_device,
            get_level_settings,
            set_level_settings,
            get_level_status,
            set_record_dir,
            start_recording,
            stop_recording,
            get_rec_status,
            open_recordings_folder,
            reload_favorites,
            get_layout,
            set_layout
        ])
        .run(tauri::generate_context!())
        .expect("error while running Church Playout");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            id: 0,
            path: "/m/Hymnal/Amazing Grace 123.mp3".into(),
            songbook: "Hymnal".into(),
            title: "Amazing Grace".into(),
            page: Some(123),
            file_name: "Amazing Grace 123.mp3".into(),
            ext: "mp3".into(),
            tag_title: None,
            artist: None,
            album: None,
            duration_secs: None,
        }
    }

    #[test]
    fn recording_names_are_safe_and_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("rec-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(clean_file_name("  Sermon: Part 1/2? "), "Sermon Part 1 2");
        assert_eq!(clean_file_name("..."), "");
        let p = recording_path(&dir, "", "11-08-2026");
        assert_eq!(p.file_name().unwrap(), "11-08-2026.wav");
        fs::write(&p, b"x").unwrap();
        let p2 = recording_path(&dir, "Sermon", "11-08-2026");
        assert_eq!(p2.file_name().unwrap(), "11-08-2026 Sermon.wav");
        fs::write(&p2, b"x").unwrap();
        let p3 = recording_path(&dir, "Sermon", "11-08-2026");
        assert_eq!(p3.file_name().unwrap(), "11-08-2026 Sermon (2).wav");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pad_text_uses_tags_when_present() {
        let mut t = track();
        assert_eq!(pad_label(&t), "Amazing Grace");
        assert_eq!(pad_sub(&t), "Hymnal \u{b7} p. 123");
        t.tag_title = Some("  Amazing Grace (Newton) ".into());
        t.artist = Some("Choir".into());
        assert_eq!(pad_label(&t), "Amazing Grace (Newton)");
        assert_eq!(pad_sub(&t), "Choir \u{b7} Hymnal \u{b7} p. 123");
        t.tag_title = Some("   ".into());
        t.page = None;
        assert_eq!(pad_label(&t), "Amazing Grace");
        assert_eq!(pad_sub(&t), "Choir \u{b7} Hymnal");
    }

    #[test]
    fn old_settings_files_still_load() {
        let s: Settings = serde_json::from_str(r#"{"music_dir":"/m","pad_columns":3}"#).unwrap();
        assert_eq!(s.music_dir.as_deref(), Some("/m"));
        assert_eq!((s.pad_columns, s.pad_rows, s.fav_rows, s.overlap), (3, DEFAULT_ROWS, DEFAULT_FAV_ROWS, true));
        assert!(s.favorites.is_empty()); // padded to FAV_SLOTS by ensure_init
    }

    #[test]
    fn favorites_round_trip_and_visible_slots_follow_columns() {
        let mut s = Settings::default();
        s.favorites[9] = Some(Favorite { path: "/m/A 5.mp3".into(), title: "A".into(), sub: "Book \u{b7} p. 5".into() });
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.favorites.len(), FAV_SLOTS);
        assert_eq!(back.favorites[9].as_ref().unwrap().title, "A");

        s.pad_columns = 3;
        s.fav_rows = 2;
        assert_eq!(visible_fav_slots(&s), vec![0, 1, 2, 8, 9, 10]);
        s.pad_columns = 8;
        s.fav_rows = 1;
        assert_eq!(visible_fav_slots(&s), vec![0, 1, 2, 3, 4, 5, 6, 7]);
        s.fav_rows = 0;
        assert!(visible_fav_slots(&s).is_empty());
        s.fav_rows = 9; // out-of-range values from a hand-edited file are clamped
        s.pad_columns = 99;
        assert_eq!(visible_fav_slots(&s).len(), FAV_SLOTS);
    }
}
