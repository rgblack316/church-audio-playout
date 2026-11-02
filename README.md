# Church Playout

Windows church-service music playout app (Tauri 2 + Rust).

## Status
- Phase 1 (done): scan a music folder, treat each top-level folder as a songbook,
  read the trailing number of each file name as the page number, search by page
  and/or title within one songbook or all of them.
- Phase 2 (done): audio playback. Each pad decodes its file into memory (instant start,
  exact length) and all pads mix into the default Windows output device. A countdown shows
  time remaining and turns red in the last 10 seconds. MP3, WAV, FLAC, OGG and M4A/AAC are
  decoded directly; WMA (and anything else symphonia cannot read) falls back to Windows
  Media Foundation. Files longer than 45 minutes are refused.
- Phase 3/4 (done): search loads songs onto pads (double-click, or Enter for the top result);
  pad grid is configurable up to 8 columns x 8 rows (Settings, default 4 x 2); an overlap
  setting lets pads play together or makes each new pad stop the others. Saved between launches.
- Phase 5 (done): favorites bar under the pad grid (0-2 rows, same number of columns as the pad
  grid, named A1, A2, B1...). Favorites are remembered between launches and pre-loaded into memory
  at start-up so they fire instantly. A favorite whose file is missing shows the song title and
  the error in red; it is retried after a rescan. Songs can be put on any pad or favorite by
  double-click (selected pad), Enter in search (top result), or by dragging a row onto it.
- Phase 5b (done): the file browser shows the full file name (with extension) and, when the files
  have them, title / artist / album / length from their tags (ID3, Vorbis, MP4, FLAC and WMA).
  Tag columns appear only when a result has that tag, and tags are searchable. Tag data is cached
  (path + modified time + size) so rescans only read new or changed files; it can be turned off in
  Settings. WMA decoding on Windows now reports exactly which Media Foundation step failed.
- Next: keyboard shortcuts, polish.

## Using it
Click a pad or favorite to select it, double-click a song in the results to load it there (the
selection then moves to the next empty slot), or drag a song onto any pad or favorite. Play/Stop on each pad; Stop all in the header. If the audio
device is missing or changes, use **Retry audio** in the red banner.

## Music folder layout
```
Music/
  Hymnal 1991/Amazing Grace 123.mp3
  Praise Songs/Amazing Grace 12.flac
```
Formats scanned: mp3, wav, flac, ogg, m4a, aac, wma.
Files without a trailing number still appear (page shown as "—") and are listed under Settings.

## Build the installer (no local setup)
Push to GitHub. The **Build Windows installer** workflow produces
`church-playout-windows` (an NSIS `.exe`) under the run's Artifacts.
The installer bundles the WebView2 bootstrapper; the church PC needs nothing else.

## Develop locally (optional)
Install Rust, then: `cargo install tauri-cli --version "^2" --locked` and `cd src-tauri && cargo tauri dev`.
Core logic tests run anywhere: `cargo test -p playout-core`.
