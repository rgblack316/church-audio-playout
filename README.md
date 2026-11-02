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
- Next: configurable pad grid and overlap setting, favorites bar, keyboard shortcuts.

## Using it
Click a pad to select it, double-click a song in the results to load it there (the selection
then moves to the next empty pad). Play/Stop on each pad; Stop all in the header. If the audio
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
