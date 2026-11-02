#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use playout_core::{scan, Library, Problem, Query, ScanOptions, Track, DEFAULT_PAGE_PATTERN};
use serde::{Deserialize, Serialize};
use playout_audio::{decode_file, Engine, Snapshot};
use std::{fs, path::Path, path::PathBuf, sync::Arc, sync::Mutex};
use tauri::{AppHandle, Manager, State};

const DEFAULT_SEARCH_LIMIT: usize = 500;
const DEFAULT_COLUMNS: usize = 4;
const DEFAULT_ROWS: usize = 2;
const MAX_GRID: usize = 8;

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
}

fn default_columns() -> usize {
    DEFAULT_COLUMNS
}
fn default_rows() -> usize {
    DEFAULT_ROWS
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
        }
    }
}

#[derive(Default)]
struct AppState {
    initialized: bool,
    settings: Settings,
    library: Library,
}

type Shared = Mutex<AppState>;

#[derive(Serialize)]
struct Summary {
    music_dir: Option<String>,
    track_count: usize,
    songbooks: Vec<String>,
    without_page: Vec<Track>,
    unreadable: Vec<Problem>,
}

fn summarize(state: &AppState) -> Summary {
    Summary {
        music_dir: state.settings.music_dir.clone(),
        track_count: state.library.tracks.len(),
        songbooks: state.library.songbooks(),
        without_page: state.library.without_page().into_iter().cloned().collect(),
        unreadable: state.library.problems.clone(),
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
        state.initialized = true;
    }
}

fn run_scan(state: &mut AppState) -> Result<(), String> {
    state.library = Library::default();
    if let Some(dir) = state.settings.music_dir.clone() {
        let opts = ScanOptions {
            page_pattern: state.settings.page_pattern.clone(),
            ..ScanOptions::default()
        };
        state.library = scan(Path::new(&dir), &opts)?;
    }
    Ok(())
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
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    run_scan(&mut state)?;
    Ok(summarize(&state))
}

#[tauri::command(async)]
fn set_music_dir(
    app: AppHandle,
    shared: State<'_, Shared>,
    path: String,
) -> Result<Summary, String> {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    let previous = state.settings.music_dir.clone();
    state.settings.music_dir = Some(path);
    if let Err(e) = run_scan(&mut state) {
        state.settings.music_dir = previous;
        let _ = run_scan(&mut state);
        return Err(e);
    }
    save_settings(&app, &state.settings)?;
    Ok(summarize(&state))
}

#[tauri::command(async)]
fn set_page_pattern(
    app: AppHandle,
    shared: State<'_, Shared>,
    pattern: String,
) -> Result<Summary, String> {
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    let previous = state.settings.page_pattern.clone();
    state.settings.page_pattern = if pattern.trim().is_empty() {
        default_pattern()
    } else {
        pattern
    };
    if let Err(e) = run_scan(&mut state) {
        state.settings.page_pattern = previous;
        let _ = run_scan(&mut state);
        return Err(e);
    }
    save_settings(&app, &state.settings)?;
    Ok(summarize(&state))
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

#[tauri::command]
fn get_pads(engine: State<'_, Engine>) -> Snapshot {
    engine.snapshot()
}

/// Decode a library track fully into memory and put it on a pad.
#[tauri::command(async)]
fn load_pad(
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    pad: usize,
    track_id: u32,
) -> Result<(), String> {
    let track = {
        let state = shared.lock().unwrap();
        state.library.get(track_id).cloned().ok_or("Track not found; try searching again")?
    };
    engine.check_loadable(pad)?;
    let decoded = decode_file(Path::new(&track.path))
        .map_err(|e| format!("Cannot play \"{}\": {e}", track.file_name))?;
    let sub = match track.page {
        Some(p) => format!("{} \u{b7} p. {}", track.songbook, p),
        None => track.songbook.clone(),
    };
    engine.load(pad, Arc::new(decoded), track.title.clone(), sub)
}

#[tauri::command]
fn play_pad(engine: State<'_, Engine>, pad: usize) -> Result<(), String> {
    engine.play(pad)
}

#[tauri::command]
fn stop_pad(engine: State<'_, Engine>, pad: usize) {
    engine.stop(pad)
}

#[tauri::command]
fn stop_all(engine: State<'_, Engine>) {
    engine.stop_all()
}

#[tauri::command]
fn clear_pad(engine: State<'_, Engine>, pad: usize) -> Result<(), String> {
    engine.clear(pad)
}

#[tauri::command]
fn restart_audio(engine: State<'_, Engine>) {
    engine.restart_audio()
}

// ---------- pad layout ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Layout {
    columns: usize,
    rows: usize,
    overlap: bool,
}

fn layout_of(s: &Settings) -> Layout {
    Layout {
        columns: s.pad_columns.clamp(1, MAX_GRID),
        rows: s.pad_rows.clamp(1, MAX_GRID),
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
    l
}

#[tauri::command]
fn set_layout(
    app: AppHandle,
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    columns: usize,
    rows: usize,
    overlap: bool,
) -> Result<Layout, String> {
    if !(1..=MAX_GRID).contains(&columns) || !(1..=MAX_GRID).contains(&rows) {
        return Err(format!("Columns and rows must be between 1 and {MAX_GRID}"));
    }
    let mut state = shared.lock().unwrap();
    ensure_init(&app, &mut state);
    let l = Layout { columns, rows, overlap };
    apply_layout(&engine, &l)?;
    state.settings.pad_columns = columns;
    state.settings.pad_rows = rows;
    state.settings.overlap = overlap;
    save_settings(&app, &state.settings)?;
    Ok(l)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Shared::new(AppState::default()))
        .manage(Engine::new(DEFAULT_COLUMNS * DEFAULT_ROWS))
        .invoke_handler(tauri::generate_handler![
            get_state,
            rescan,
            set_music_dir,
            set_page_pattern,
            get_page_pattern,
            search,
            get_pads,
            load_pad,
            play_pad,
            stop_pad,
            stop_all,
            clear_pad,
            restart_audio,
            get_layout,
            set_layout
        ])
        .run(tauri::generate_context!())
        .expect("error while running Church Playout");
}
