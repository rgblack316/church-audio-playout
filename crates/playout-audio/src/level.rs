//! Loudness leveling: measure a recording (EBU R128 / ITU BS.1770 integrated loudness), apply
//! the gain that brings it to a target such as -16 LUFS, and keep peaks under a true-peak
//! ceiling with a look-ahead limiter. The original file is never touched.

use ebur128::{EbuR128, Mode};
use serde::Serialize;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

/// True-peak ceiling of the leveled file.
pub const TRUE_PEAK_CEILING_DB: f64 = -1.0;
/// The limiter works on sample peaks, so it keeps a little extra room below the true-peak ceiling.
const SAMPLE_CEILING_DB: f64 = TRUE_PEAK_CEILING_DB - 1.5;
const LOOKAHEAD_SECS: f64 = 0.008;
const RELEASE_SECS: f64 = 0.15;
/// More gain than this means the recording is silent or the input was muted.
const MAX_GAIN_DB: f64 = 36.0;
const CHUNK_FRAMES: usize = 8192;

#[derive(Debug, Clone, Serialize)]
pub struct LevelReport {
    pub output: PathBuf,
    /// Integrated loudness of the original, in LUFS.
    pub measured_lufs: f64,
    pub target_lufs: f64,
    /// Gain applied before limiting, in dB.
    pub gain_db: f64,
    /// Measured on the leveled file.
    pub final_lufs: f64,
    pub true_peak_db: f64,
    /// True if the limiter had to turn peaks down (loudness may then sit a little under target).
    pub limited: bool,
}

/// Look-ahead peak limiter: the gain is the moving average of a sliding minimum over the
/// look-ahead window, so it is already down when a peak arrives and ramps smoothly, then
/// recovers slowly.
struct Limiter {
    ch: usize,
    n: usize,
    ceiling: f32,
    release: f32,
    pending: VecDeque<f32>,
    window_min: VecDeque<(u64, f32)>,
    history: VecDeque<f32>,
    history_sum: f64,
    pushed: u64,
    g_prev: f32,
    min_gain: f32,
}

impl Limiter {
    fn new(ch: usize, rate: u32) -> Self {
        let n = ((LOOKAHEAD_SECS * rate as f64) as usize).max(1);
        Self {
            ch,
            n,
            ceiling: 10f64.powf(SAMPLE_CEILING_DB / 20.0) as f32,
            release: (1.0 - (-1.0 / (RELEASE_SECS * rate as f64)).exp()) as f32,
            pending: VecDeque::new(),
            window_min: VecDeque::new(),
            history: VecDeque::new(),
            history_sum: 0.0,
            pushed: 0,
            g_prev: 1.0,
            min_gain: 1.0,
        }
    }

    fn push(&mut self, frame: &[f32], out: &mut Vec<f32>) {
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let t = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
        let idx = self.pushed;
        self.pushed += 1;
        while self.window_min.back().is_some_and(|&(_, v)| v >= t) {
            self.window_min.pop_back();
        }
        self.window_min.push_back((idx, t));
        self.pending.extend(frame.iter().copied());
        if idx < self.n as u64 {
            return;
        }
        let j = idx - self.n as u64;
        while self.window_min.front().is_some_and(|&(i, _)| i < j) {
            self.window_min.pop_front();
        }
        let m = self.window_min.front().map_or(1.0, |&(_, v)| v);
        if self.history.is_empty() {
            // start-up: assume the first value held before the file began, so a peak at 0:00 is limited too
            self.history.extend(std::iter::repeat(m).take(self.n));
            self.history_sum = m as f64 * self.n as f64;
        }
        self.history.push_back(m);
        self.history_sum += m as f64;
        if self.history.len() > self.n + 1 {
            self.history_sum -= self.history.pop_front().unwrap_or(1.0) as f64;
        }
        let avg = (self.history_sum / (self.n + 1) as f64) as f32;
        let g = avg.min(self.g_prev + self.release * (1.0 - self.g_prev));
        self.g_prev = g;
        self.min_gain = self.min_gain.min(g);
        for _ in 0..self.ch {
            out.push(self.pending.pop_front().unwrap_or(0.0) * g);
        }
    }

    /// Emit the frames still waiting for their look-ahead.
    fn finish(&mut self, out: &mut Vec<f32>) {
        let silence = vec![0.0f32; self.ch];
        for _ in 0..self.n {
            self.push(&silence, out);
        }
    }
}

/// Reads a WAV (16/24/32-bit integer or 32-bit float) as f32 chunks. A file cut short by a crash
/// simply ends where the data ends.
struct WavFrames {
    samples: Box<dyn Iterator<Item = f32>>,
    ch: usize,
}

impl WavFrames {
    fn open(path: &Path) -> Result<(Self, hound::WavSpec, u32), String> {
        let reader = hound::WavReader::open(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        let spec = reader.spec();
        if !(1..=2).contains(&spec.channels) {
            return Err(format!("Leveling supports mono and stereo files (this one has {} channels)", spec.channels));
        }
        let frames = reader.duration();
        let samples: Box<dyn Iterator<Item = f32>> = match spec.sample_format {
            hound::SampleFormat::Float => Box::new(reader.into_samples::<f32>().map_while(Result::ok)),
            hound::SampleFormat::Int => {
                let scale = 2f32.powi(spec.bits_per_sample as i32 - 1);
                Box::new(reader.into_samples::<i32>().map_while(Result::ok).map(move |s| s as f32 / scale))
            }
        };
        Ok((Self { samples, ch: spec.channels as usize }, spec, frames))
    }

    fn next_chunk(&mut self, buf: &mut Vec<f32>) -> bool {
        buf.clear();
        buf.extend((&mut self.samples).take(CHUNK_FRAMES * self.ch));
        let whole = buf.len() / self.ch * self.ch;
        buf.truncate(whole);
        !buf.is_empty()
    }
}

fn to_db(linear: f64) -> f64 {
    if linear > 0.0 {
        20.0 * linear.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// Level `input` to `target_lufs`, writing a 24-bit WAV to `output`. `progress` gets 0.0..=1.0.
pub fn level_wav(
    input: &Path,
    output: &Path,
    target_lufs: f64,
    progress: &mut dyn FnMut(f32),
) -> Result<LevelReport, String> {
    if !(-40.0..=-6.0).contains(&target_lufs) {
        return Err("The loudness target must be between -40 and -6 LUFS".into());
    }
    let (mut frames, spec, total) = WavFrames::open(input)?;
    let ch = frames.ch;
    let total = total.max(1) as f32;

    // Pass 1: measure.
    let mut meter = EbuR128::new(ch as u32, spec.sample_rate, Mode::I).map_err(|e| format!("Loudness meter: {e}"))?;
    let mut buf = Vec::new();
    let mut done = 0f32;
    while frames.next_chunk(&mut buf) {
        meter.add_frames_f32(&buf).map_err(|e| format!("Loudness meter: {e}"))?;
        done += (buf.len() / ch) as f32;
        progress(0.45 * done / total);
    }
    let measured = meter.loudness_global().map_err(|e| format!("Loudness meter: {e}"))?;
    if !measured.is_finite() {
        return Err("The recording is silent or too quiet to measure".into());
    }
    let gain_db = target_lufs - measured;
    if gain_db > MAX_GAIN_DB {
        return Err(format!("The recording is extremely quiet ({measured:.1} LUFS); was the input muted?"));
    }
    let gain = 10f32.powf(gain_db as f32 / 20.0);

    // Pass 2: apply gain and limiter, measure the result as it is written.
    let (mut frames, _, _) = WavFrames::open(input)?;
    let out_spec = hound::WavSpec { channels: ch as u16, sample_rate: spec.sample_rate, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
    let temp = output.with_extension("wav.part");
    let mut writer = hound::WavWriter::create(&temp, out_spec).map_err(|e| format!("Could not create {}: {e}", output.display()))?;
    let mut check = EbuR128::new(ch as u32, spec.sample_rate, Mode::I | Mode::TRUE_PEAK).map_err(|e| format!("Loudness meter: {e}"))?;
    let mut limiter = Limiter::new(ch, spec.sample_rate);
    let mut out: Vec<f32> = Vec::new();
    let write = |samples: &[f32], writer: &mut hound::WavWriter<_>, check: &mut EbuR128| -> Result<(), String> {
        check.add_frames_f32(samples).map_err(|e| format!("Loudness meter: {e}"))?;
        for s in samples {
            writer
                .write_sample((s * 8_388_607.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32)
                .map_err(|e| format!("Writing {} failed: {e}", output.display()))?;
        }
        Ok(())
    };
    let mut done = 0f32;
    while frames.next_chunk(&mut buf) {
        out.clear();
        for frame in buf.chunks_exact(ch) {
            let scaled: [f32; 2] = [frame[0] * gain, frame[ch - 1] * gain];
            limiter.push(&scaled[..ch], &mut out);
        }
        write(&out, &mut writer, &mut check)?;
        done += (buf.len() / ch) as f32;
        progress(0.45 + 0.55 * done / total);
    }
    out.clear();
    limiter.finish(&mut out);
    write(&out, &mut writer, &mut check)?;
    writer.finalize().map_err(|e| format!("Finishing {} failed: {e}", output.display()))?;
    std::fs::rename(&temp, output).map_err(|e| format!("Could not save {}: {e}", output.display()))?;

    let final_lufs = check.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let peak = (0..ch as u32).filter_map(|c| check.true_peak(c).ok()).fold(0.0f64, f64::max);
    progress(1.0);
    Ok(LevelReport {
        output: output.to_path_buf(),
        measured_lufs: measured,
        target_lufs,
        gain_db,
        final_lufs,
        true_peak_db: to_db(peak),
        limited: limiter.min_gain < 0.99,
    })
}

/// `Name.wav` -> `Name-leveled.wav` next to it.
pub fn leveled_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    input.with_file_name(format!("{stem}-leveled.wav"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like test signal: a tone whose level swells and falls, with a few sharp peaks.
    fn write_test_wav(path: &Path, rate: u32, channels: u16, secs: f32, level: f32, peaks: bool) {
        let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let frames = (rate as f32 * secs) as usize;
        for i in 0..frames {
            let t = i as f32 / rate as f32;
            let env = 0.6 + 0.4 * (t * 1.3).sin();
            let mut v = level * env * ((t * 2.0 * std::f32::consts::PI * 220.0).sin() + 0.5 * (t * 2.0 * std::f32::consts::PI * 1700.0).sin()) / 1.5;
            if peaks && i % (rate as usize * 2) < 20 {
                v += level * 6.0; // short, loud spikes
            }
            let s = (v.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
            for _ in 0..channels {
                w.write_sample(s).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn quiet_recording_is_brought_to_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("quiet.wav");
        write_test_wav(&input, 48_000, 2, 20.0, 0.03, false);
        let out = leveled_path(&input);
        assert_eq!(out.file_name().unwrap(), "quiet-leveled.wav");
        let mut last = 0.0;
        let r = level_wav(&input, &out, -16.0, &mut |p| {
            assert!(p >= last, "progress never goes backwards");
            last = p;
        })
        .unwrap();
        assert_eq!(last, 1.0);
        assert!(r.measured_lufs < -25.0, "measured {}", r.measured_lufs);
        assert!((r.final_lufs + 16.0).abs() < 0.3, "final {} LUFS", r.final_lufs);
        assert!(r.true_peak_db <= TRUE_PEAK_CEILING_DB + 0.05, "peak {}", r.true_peak_db);
        assert!(!r.limited);
        assert!(!out.with_extension("wav.part").exists(), "temp file is gone");
        // original untouched, output is a 24-bit stereo WAV of the same length
        let a = hound::WavReader::open(&input).unwrap();
        let b = hound::WavReader::open(&out).unwrap();
        assert_eq!((b.spec().channels, b.spec().bits_per_sample), (2, 24));
        assert_eq!(a.duration(), b.duration(), "same length, nothing lost to the limiter delay");
    }

    #[test]
    fn peaks_stay_under_the_true_peak_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("peaky.wav");
        write_test_wav(&input, 48_000, 2, 12.0, 0.05, true);
        let out = dir.path().join("peaky-out.wav");
        let r = level_wav(&input, &out, -16.0, &mut |_| {}).unwrap();
        assert!(r.limited, "the spikes need limiting");
        assert!(r.true_peak_db <= TRUE_PEAK_CEILING_DB + 0.05, "true peak {} dB", r.true_peak_db);
        // measure again from the written file, independently of the leveler's own numbers
        let mut rd = hound::WavReader::open(&out).unwrap();
        let sample_peak = rd.samples::<i32>().map(|s| s.unwrap().abs()).max().unwrap() as f64 / 8_388_608.0;
        assert!(to_db(sample_peak) <= -1.0, "sample peak {} dBFS", to_db(sample_peak));
    }

    #[test]
    fn mono_files_and_loud_files_work_too() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("loud.wav");
        write_test_wav(&input, 44_100, 1, 8.0, 0.5, false);
        let r = level_wav(&input, &dir.path().join("o.wav"), -19.0, &mut |_| {}).unwrap();
        assert!(r.gain_db < 0.0, "a loud file is turned down");
        assert!((r.final_lufs + 19.0).abs() < 0.3, "final {}", r.final_lufs);
    }

    #[test]
    fn silence_and_bad_input_give_clear_errors() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("silent.wav");
        write_test_wav(&input, 48_000, 2, 3.0, 0.0, false);
        let err = level_wav(&input, &dir.path().join("o.wav"), -16.0, &mut |_| {}).unwrap_err();
        assert!(err.contains("silent"), "{err}");
        assert!(level_wav(&input, &dir.path().join("o.wav"), 5.0, &mut |_| {}).is_err());
        assert!(level_wav(&dir.path().join("missing.wav"), &dir.path().join("o.wav"), -16.0, &mut |_| {}).is_err());
    }

    #[test]
    fn a_file_cut_short_by_a_crash_can_still_be_leveled() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("cut.wav");
        write_test_wav(&input, 48_000, 2, 6.0, 0.03, false);
        let mut bytes = std::fs::read(&input).unwrap();
        bytes.truncate(bytes.len() - 100_001); // data chunk now ends mid-frame, header still claims more
        std::fs::write(&input, bytes).unwrap();
        let r = level_wav(&input, &dir.path().join("o.wav"), -16.0, &mut |_| {}).unwrap();
        assert!((r.final_lufs + 16.0).abs() < 0.5, "final {}", r.final_lufs);
    }
}
