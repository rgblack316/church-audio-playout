//! Recording an input device (for example the RODECaster's USB mix) to a 24-bit WAV file.
//!
//! The audio callback only copies samples into a channel; a separate writer thread does all
//! the disk work, so a slow disk can never disturb playback. The WAV header is refreshed every
//! couple of seconds, so a crash or power loss leaves a playable file.

use crate::{label_devices, parse_choice, DeviceEntry};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

/// A WAV file may not pass 4 GiB; start a new part a little before that.
const PART_LIMIT_BYTES: u64 = 3_800_000_000;
/// Refresh the WAV header this often (seconds of audio) so a crash loses almost nothing.
const FLUSH_EVERY_SECS: u64 = 2;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Input devices that can record, one entry per stereo pair on multichannel devices.
pub fn list_input_devices(keep: Option<&str>) -> Vec<DeviceEntry> {
    let host = cpal::default_host();
    let found: Vec<(String, u16)> = host
        .devices()
        .map(|it| {
            it.filter_map(|d| {
                let name = d.name().ok()?;
                let ch = d
                    .supported_input_configs()
                    .ok()
                    .and_then(|c| c.map(|c| c.channels()).max())
                    .or_else(|| d.default_input_config().ok().map(|c| c.channels()))
                    .unwrap_or(0);
                (ch > 0).then_some((name, ch))
            })
            .collect()
        })
        .unwrap_or_default();
    label_devices(found, keep, false)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecStatus {
    pub recording: bool,
    pub elapsed_secs: f64,
    /// Loudest sample since the last status call, left and right (linear, 1.0 = full scale).
    pub peak_l: f32,
    pub peak_r: f32,
    /// File being written (or the last one written).
    pub path: Option<String>,
    pub bytes: u64,
    pub device: String,
    pub sample_rate: u32,
    pub error: Option<String>,
}

struct Shared {
    running: AtomicBool,
    frames: AtomicU64,
    bytes: AtomicU64,
    peak_l: AtomicU32,
    peak_r: AtomicU32,
    error: Mutex<Option<String>>,
    path: Mutex<PathBuf>,
}

fn raise(slot: &AtomicU32, value: f32) {
    let mut cur = slot.load(Ordering::Relaxed);
    while value > f32::from_bits(cur) {
        match slot.compare_exchange_weak(cur, value.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
}

/// Take channels (first, first + 1) of an interleaved buffer as stereo. A one-channel device is
/// copied to both sides; if the device has no such pair, channels 1-2 are used.
pub(crate) fn stereo_pair(data: &[f32], channels: usize, first: usize, out: &mut Vec<f32>) {
    out.clear();
    let ch = channels.max(1);
    if ch == 1 {
        for &s in data {
            out.push(s);
            out.push(s);
        }
        return;
    }
    let first = if first + 1 < ch { first } else { 0 };
    for frame in data.chunks_exact(ch) {
        out.push(frame[first]);
        out.push(frame[first + 1]);
    }
}

pub fn part_path(base: &Path, part: u32) -> PathBuf {
    if part == 1 {
        return base.to_path_buf();
    }
    let stem = base.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    base.with_file_name(format!("{stem} (part {part}).wav"))
}

fn open_wav(path: &Path, rate: u32) -> Result<hound::WavWriter<std::io::BufWriter<std::fs::File>>, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    }
    let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
    hound::WavWriter::create(path, spec).map_err(|e| format!("Could not create {}: {e}", path.display()))
}

/// Receive stereo f32 blocks and write them as 24-bit WAV until the sender is dropped.
/// `part_limit` is the size at which a new file is started.
fn write_loop(rx: Receiver<Vec<f32>>, base: PathBuf, rate: u32, shared: &Shared, part_limit: u64) -> Result<(), String> {
    let mut part = 1u32;
    let mut writer = open_wav(&base, rate)?;
    let mut part_bytes = 0u64;
    let mut since_flush = 0u64;
    let flush_frames = rate as u64 * FLUSH_EVERY_SECS;
    for block in rx {
        for s in &block {
            let v = (s.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
            writer.write_sample(v).map_err(|e| format!("Writing the recording failed: {e}"))?;
        }
        let frames = (block.len() / 2) as u64;
        let bytes = block.len() as u64 * 3;
        part_bytes += bytes;
        since_flush += frames;
        shared.frames.fetch_add(frames, Ordering::Relaxed);
        shared.bytes.fetch_add(bytes, Ordering::Relaxed);
        if part_bytes >= part_limit {
            writer.finalize().map_err(|e| format!("Finishing the recording failed: {e}"))?;
            part += 1;
            let next = part_path(&base, part);
            writer = open_wav(&next, rate)?;
            *lock(&shared.path) = next;
            part_bytes = 0;
            since_flush = 0;
        } else if since_flush >= flush_frames {
            writer.flush().map_err(|e| format!("Writing the recording failed: {e}"))?;
            since_flush = 0;
        }
    }
    writer.finalize().map_err(|e| format!("Finishing the recording failed: {e}"))
}

struct Active {
    shared: Arc<Shared>,
    stop: Sender<()>,
    join: Option<JoinHandle<()>>,
    device: String,
    rate: u32,
}

#[derive(Default)]
pub struct Recorder {
    active: Mutex<Option<Active>>,
    /// Result of the most recent recording (file or error), shown after it ends.
    last: Mutex<RecStatus>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_recording(&self) -> bool {
        lock(&self.active).as_ref().is_some_and(|a| a.shared.running.load(Ordering::Relaxed))
    }

    /// Start recording `choice` (None = default input; "Name@@3" = channels 3-4) into `path`.
    pub fn start(&self, choice: Option<&str>, path: PathBuf) -> Result<(), String> {
        let mut active = lock(&self.active);
        if let Some(a) = active.as_ref() {
            if a.shared.running.load(Ordering::Relaxed) {
                return Err("Already recording".into());
            }
        }
        if let Some(mut old) = active.take() {
            if let Some(j) = old.join.take() {
                let _ = j.join();
            }
        }
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            frames: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            peak_l: AtomicU32::new(0),
            peak_r: AtomicU32::new(0),
            error: Mutex::new(None),
            path: Mutex::new(path.clone()),
        });
        let (stop_tx, stop_rx) = channel::<()>();
        let (ready_tx, ready_rx) = channel::<Result<(String, u32), String>>();
        let choice = choice.map(str::to_string);
        let thread_shared = shared.clone();
        let stop_for_errors = stop_tx.clone();
        let join = std::thread::Builder::new()
            .name("record".into())
            .spawn(move || run(choice, path, thread_shared, stop_rx, stop_for_errors, ready_tx))
            .map_err(|e| format!("Could not start recording: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok((device, rate))) => {
                *lock(&self.last) = RecStatus::default();
                *active = Some(Active { shared, stop: stop_tx, join: Some(join), device, rate });
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => Err("The input device did not respond".into()),
        }
    }

    /// Stop and finish the file. Returns the path of the (first part of the) recording.
    pub fn stop(&self) -> Option<PathBuf> {
        let mut active = lock(&self.active);
        let mut a = active.take()?;
        let _ = a.stop.send(());
        if let Some(j) = a.join.take() {
            let _ = j.join();
        }
        let status = status_of(&a, false);
        let path = lock(&a.shared.path).clone();
        *lock(&self.last) = status;
        Some(path)
    }

    pub fn status(&self) -> RecStatus {
        let mut active = lock(&self.active);
        let finished = active.as_ref().is_some_and(|a| !a.shared.running.load(Ordering::Relaxed));
        if finished {
            // The recording ended by itself (disk full, device unplugged): keep its result.
            if let Some(mut a) = active.take() {
                if let Some(j) = a.join.take() {
                    let _ = j.join();
                }
                *lock(&self.last) = status_of(&a, false);
            }
        }
        match active.as_ref() {
            Some(a) => status_of(a, true),
            None => lock(&self.last).clone(),
        }
    }
}

fn status_of(a: &Active, recording: bool) -> RecStatus {
    let s = &a.shared;
    RecStatus {
        recording,
        elapsed_secs: s.frames.load(Ordering::Relaxed) as f64 / a.rate.max(1) as f64,
        peak_l: f32::from_bits(s.peak_l.swap(0, Ordering::Relaxed)),
        peak_r: f32::from_bits(s.peak_r.swap(0, Ordering::Relaxed)),
        path: Some(lock(&s.path).display().to_string()),
        bytes: s.bytes.load(Ordering::Relaxed),
        device: a.device.clone(),
        sample_rate: a.rate,
        error: lock(&s.error).clone(),
    }
}

/// The recording thread: owns the input stream (not Send on every platform).
fn run(
    choice: Option<String>,
    path: PathBuf,
    shared: Arc<Shared>,
    stop_rx: Receiver<()>,
    stop_for_errors: Sender<()>,
    ready: Sender<Result<(String, u32), String>>,
) {
    let (tx, rx) = channel::<Vec<f32>>();
    let opened = open_input(choice.as_deref(), tx, shared.clone(), stop_for_errors);
    let (stream, name, rate) = match opened {
        Ok(v) => v,
        Err(e) => {
            shared.running.store(false, Ordering::Relaxed);
            let _ = ready.send(Err(e));
            return;
        }
    };
    let writer_shared = shared.clone();
    let writer = std::thread::Builder::new()
        .name("record-writer".into())
        .spawn(move || write_loop(rx, path, rate, &writer_shared, PART_LIMIT_BYTES));
    let writer = match writer {
        Ok(w) => w,
        Err(e) => {
            shared.running.store(false, Ordering::Relaxed);
            let _ = ready.send(Err(format!("Could not start recording: {e}")));
            return;
        }
    };
    if let Err(e) = stream.play() {
        shared.running.store(false, Ordering::Relaxed);
        drop(stream);
        let _ = writer.join();
        let _ = ready.send(Err(format!("Could not start the input: {e}")));
        return;
    }
    keep_awake(true);
    let _ = ready.send(Ok((name, rate)));

    // Run until Stop, or until the stream or the disk reports an error.
    let _ = stop_rx.recv();
    drop(stream); // drops the sender; the writer drains what is left and finishes the file
    if let Ok(Err(e)) = writer.join() {
        *lock(&shared.error) = Some(e);
    }
    keep_awake(false);
    shared.running.store(false, Ordering::Relaxed);
}

fn open_input(
    choice: Option<&str>,
    tx: Sender<Vec<f32>>,
    shared: Arc<Shared>,
    stop: Sender<()>,
) -> Result<(cpal::Stream, String, u32), String> {
    let host = cpal::default_host();
    let (name, first) = choice.map(parse_choice).map(|(n, f)| (Some(n), f)).unwrap_or((None, 0));
    let device = match name {
        Some(n) => host
            .devices()
            .map_err(|e| format!("Could not list audio devices: {e}"))?
            .find(|d| d.name().map(|dn| dn == n).unwrap_or(false))
            .ok_or_else(|| format!("Input device \"{n}\" was not found"))?,
        None => host.default_input_device().ok_or("No audio input device found")?,
    };
    let default = device.default_input_config().map_err(|e| format!("Could not read the input format: {e}"))?;
    let needed = (first + 2) as u16;
    let supported = if default.channels() >= needed || first == 0 {
        default
    } else {
        let rate = default.sample_rate();
        device
            .supported_input_configs()
            .map_err(|e| format!("Could not read the input formats: {e}"))?
            .filter(|c| c.channels() >= needed)
            .min_by_key(|c| c.channels())
            .map(|c| if rate >= c.min_sample_rate() && rate <= c.max_sample_rate() { c.with_sample_rate(rate) } else { c.with_max_sample_rate() })
            .ok_or_else(|| format!("That input has no channels {}\u{2013}{}", first + 1, first + 2))?
    };
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate.0;
    let mut label = device.name().unwrap_or_else(|_| "Input".into());
    if first > 0 {
        label = format!("{label} (channels {}\u{2013}{})", first + 1, first + 2);
    }
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, first, tx, shared, stop),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, first, tx, shared, stop),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, first, tx, shared, stop),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, first, tx, shared, stop),
        other => Err(format!("Unsupported input sample format: {other:?}")),
    }?;
    Ok((stream, label, rate))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    first: usize,
    tx: Sender<Vec<f32>>,
    shared: Arc<Shared>,
    stop: Sender<()>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let mut floats: Vec<f32> = Vec::new();
    let mut pair: Vec<f32> = Vec::new();
    let err_shared = shared.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                floats.clear();
                floats.extend(data.iter().map(|&s| cpal::Sample::to_sample::<f32>(s)));
                stereo_pair(&floats, channels, first, &mut pair);
                let (mut l, mut r) = (0.0f32, 0.0f32);
                for f in pair.chunks_exact(2) {
                    l = l.max(f[0].abs());
                    r = r.max(f[1].abs());
                }
                raise(&shared.peak_l, l);
                raise(&shared.peak_r, r);
                let _ = tx.send(pair.clone());
            },
            move |err| {
                *lock(&err_shared.error) = Some(format!("The input stopped: {err}"));
                let _ = stop.send(());
            },
            None,
        )
        .map_err(|e| input_error(&e.to_string()))
}

/// Plain-language version of the common Windows audio errors.
fn input_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let hint = if lower.contains("8889000a") {
        " \u{2014} another program is using this input exclusively. Close other audio programs (Audacity, OBS, a recorder app), \
          or turn off \"Allow applications to take exclusive control\" for the device in Windows Sound settings, or pick a different input."
    } else if lower.contains("80070005") {
        " \u{2014} Windows is blocking microphone access. Turn on Settings > Privacy > Microphone > \"Let desktop apps access your microphone\"."
    } else if lower.contains("88890008") {
        " \u{2014} the input does not support this format. Pick a different input or channel pair."
    } else {
        ""
    };
    format!("Could not open the input: {raw}{hint}")
}

#[cfg(test)]
mod hint_tests {
    use super::input_error;

    #[test]
    fn known_windows_errors_get_a_hint() {
        assert!(input_error("A Backend-specific error has occurred: 0x8889000A").contains("exclusively"));
        assert!(input_error("denied 0x80070005").contains("Privacy"));
        assert_eq!(input_error("weird"), "Could not open the input: weird");
    }
}

/// Keep Windows from sleeping while a recording runs. The setting belongs to the calling
/// thread, which is why the recording thread (alive for the whole recording) does it.
fn keep_awake(on: bool) {
    #[cfg(all(windows, feature = "mf"))]
    unsafe {
        use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        let _ = SetThreadExecutionState(if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
    }
    #[cfg(not(all(windows, feature = "mf")))]
    let _ = on;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_requested_channel_pair() {
        let mut out = Vec::new();
        // 4-channel frames: [1,2,3,4] and [5,6,7,8]
        let data = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        stereo_pair(&data, 4, 0, &mut out);
        assert_eq!(out, [1.0, 2.0, 5.0, 6.0]);
        stereo_pair(&data, 4, 2, &mut out);
        assert_eq!(out, [3.0, 4.0, 7.0, 8.0]);
        stereo_pair(&data, 4, 3, &mut out); // no pair starting at channel 4: fall back to 1-2
        assert_eq!(out, [1.0, 2.0, 5.0, 6.0]);
        stereo_pair(&[0.5, -0.5], 1, 0, &mut out); // mono is copied to both sides
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
    }

    fn shared(path: &Path) -> Shared {
        Shared {
            running: AtomicBool::new(true),
            frames: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            peak_l: AtomicU32::new(0),
            peak_r: AtomicU32::new(0),
            error: Mutex::new(None),
            path: Mutex::new(path.to_path_buf()),
        }
    }

    #[test]
    fn writes_a_readable_24_bit_wav_and_splits_into_parts() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("Sermon.wav");
        let sh = shared(&base);
        let (tx, rx) = channel::<Vec<f32>>();
        // 3 blocks of 100 stereo frames; a tiny part limit forces a split after the second block
        for i in 0..3 {
            tx.send((0..200).map(|n| ((n + i * 200) as f32 / 1000.0).sin() * 0.5).collect()).unwrap();
        }
        drop(tx);
        write_loop(rx, base.clone(), 48_000, &sh, 1_000).unwrap();
        assert_eq!(sh.frames.load(Ordering::Relaxed), 300);

        let first = hound::WavReader::open(&base).unwrap();
        assert_eq!((first.spec().channels, first.spec().bits_per_sample, first.spec().sample_rate), (2, 24, 48_000));
        let total: u32 = (1..=3)
            .map(|p| part_path(&base, p))
            .filter(|p| p.exists())
            .map(|p| hound::WavReader::open(p).unwrap().duration())
            .sum();
        assert_eq!(total, 300, "no audio lost across parts");
        assert!(part_path(&base, 2).exists(), "second part created");
        // values survive the 24-bit round trip
        let mut r = hound::WavReader::open(&base).unwrap();
        let s0 = r.samples::<i32>().next().unwrap().unwrap();
        assert_eq!(s0, ((0.0f32 / 1000.0).sin() * 0.5 * 8_388_607.0) as i32);
    }

    #[test]
    fn unfinished_file_is_still_readable_after_a_flush() {
        // Simulates a crash: write enough audio to trigger a header flush, then never finalize.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash.wav");
        let mut w = open_wav(&path, 100).unwrap();
        for _ in 0..1000 {
            w.write_sample(1000i32).unwrap();
        }
        w.flush().unwrap();
        std::mem::forget(w); // process "dies" here
        let r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.duration(), 500);
    }

    #[test]
    fn peaks_keep_the_loudest_value() {
        let slot = AtomicU32::new(0);
        raise(&slot, 0.3);
        raise(&slot, 0.1);
        raise(&slot, 0.7);
        assert_eq!(f32::from_bits(slot.load(Ordering::Relaxed)), 0.7);
    }
}
