//! Loudness leveling: measure a recording (EBU R128 / ITU BS.1770 integrated loudness), apply
//! the gain that brings it to a target such as -16 LUFS, and keep peaks under a true-peak
//! ceiling with a look-ahead limiter. The original file is never touched.

use crate::format::{RecFormat, Sink};
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
/// The "even out loud parts" stage mirrors Audacity's Limiter: threshold -6 dB, soft knee 2 dB,
/// 1 ms look-ahead, 20 ms release, then make-up gain so the peaks land at -1 dB.
pub const SMOOTH_THRESHOLD_DB: f64 = -6.0;
pub const SMOOTH_OUTPUT_DB: f64 = -1.0;
const SMOOTH_KNEE_DB: f64 = 2.0;
const SMOOTH_LOOKAHEAD_SECS: f64 = 0.001;
const SMOOTH_RELEASE_SECS: f64 = 0.020;
/// Safety ceiling after the smoothing stage (sample peak), a little under its -1 dB output.
const SMOOTH_SAFETY_DB: f64 = -1.5;
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
    /// The extra peak-smoothing stage was applied (loudness then ends above the target).
    pub smoothed: bool,
}

/// Look-ahead peak limiter: the gain is the moving average of a sliding minimum over the
/// look-ahead window, so it is already down when a peak arrives and ramps smoothly, then
/// recovers slowly.
struct Limiter {
    ch: usize,
    n: usize,
    threshold_db: f64,
    knee_db: f64,
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
    fn new(ch: usize, rate: u32, threshold_db: f64, knee_db: f64, lookahead_secs: f64, release_secs: f64) -> Self {
        let n = ((lookahead_secs * rate as f64) as usize).max(1);
        Self {
            ch,
            n,
            threshold_db,
            knee_db,
            release: (1.0 - (-1.0 / (release_secs * rate as f64)).exp()) as f32,
            pending: VecDeque::new(),
            window_min: VecDeque::new(),
            history: VecDeque::new(),
            history_sum: 0.0,
            pushed: 0,
            g_prev: 1.0,
            min_gain: 1.0,
        }
    }

    /// Gain (linear, <= 1) that brings a peak of this size under the threshold, with a soft knee.
    fn target_gain(&self, peak: f32) -> f32 {
        if peak <= 0.0 {
            return 1.0;
        }
        let level = 20.0 * (peak as f64).log10();
        let w = self.knee_db;
        let x = level - self.threshold_db + w / 2.0;
        let out = if x <= 0.0 {
            return 1.0;
        } else if w > 0.0 && x < w {
            level - x * x / (2.0 * w)
        } else {
            self.threshold_db
        };
        10f64.powf((out - level) / 20.0).min(1.0) as f32
    }

    fn push(&mut self, frame: &[f32], out: &mut Vec<f32>) {
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let t = self.target_gain(peak);
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

/// What `open_source` learned about the input file.
struct SourceInfo {
    channels: usize,
    rate: u32,
    /// Length in frames (an estimate when the container does not say).
    frames: u64,
    float: bool,
    mp3: bool,
}

/// Reads a WAV (16/24/32-bit integer or 32-bit float) or any file the player can decode (MP3 and
/// others) as f32 chunks. A WAV cut short by a crash simply ends where the data ends.
enum Source {
    Wav { samples: Box<dyn Iterator<Item = f32>>, ch: usize },
    Decoded(Decoded),
}

struct Decoded {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    pending: std::collections::VecDeque<f32>,
    ch: usize,
    done: bool,
}

fn is_wav(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"))
}

fn open_source(path: &Path) -> Result<(Source, SourceInfo), String> {
    if is_wav(path) {
        let reader = hound::WavReader::open(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        let spec = reader.spec();
        if !(1..=2).contains(&spec.channels) {
            return Err(format!("Leveling supports mono and stereo files (this one has {} channels)", spec.channels));
        }
        let frames = reader.duration() as u64;
        let samples: Box<dyn Iterator<Item = f32>> = match spec.sample_format {
            hound::SampleFormat::Float => Box::new(reader.into_samples::<f32>().map_while(Result::ok)),
            hound::SampleFormat::Int => {
                let scale = 2f32.powi(spec.bits_per_sample as i32 - 1);
                Box::new(reader.into_samples::<i32>().map_while(Result::ok).map(move |s| s as f32 / scale))
            }
        };
        let info = SourceInfo {
            channels: spec.channels as usize,
            rate: spec.sample_rate,
            frames,
            float: spec.sample_format == hound::SampleFormat::Float,
            mp3: false,
        };
        return Ok((Source::Wav { samples, ch: info.channels }, info));
    }
    use symphonia::core::{codecs::DecoderOptions, formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint};
    let file = std::fs::File::open(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, MediaSourceStream::new(Box::new(file), Default::default()), &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let format = probed.format;
    let track = format.default_track().ok_or("The file has no audio")?;
    let params = track.codec_params.clone();
    let channels = params.channels.map(|c| c.count()).unwrap_or(2);
    if !(1..=2).contains(&channels) {
        return Err(format!("Leveling supports mono and stereo files (this one has {channels} channels)"));
    }
    let rate = params.sample_rate.ok_or("The file does not say its sample rate")?;
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("Could not decode {}: {e}", path.display()))?;
    let info = SourceInfo { channels, rate, frames: params.n_frames.unwrap_or(rate as u64 * 3600), float: false, mp3: path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3")) };
    let track_id = track.id;
    Ok((Source::Decoded(Decoded { format, decoder, track_id, pending: Default::default(), ch: channels, done: false }), info))
}

impl Source {
    fn next_chunk(&mut self, buf: &mut Vec<f32>) -> bool {
        buf.clear();
        match self {
            Source::Wav { samples, ch } => {
                buf.extend(samples.take(CHUNK_FRAMES * *ch));
                let whole = buf.len() / *ch * *ch;
                buf.truncate(whole);
            }
            Source::Decoded(d) => {
                use symphonia::core::{audio::SampleBuffer, errors::Error};
                while d.pending.len() < CHUNK_FRAMES * d.ch && !d.done {
                    let packet = match d.format.next_packet() {
                        Ok(p) => p,
                        Err(_) => {
                            d.done = true; // end of file (or an unreadable tail)
                            break;
                        }
                    };
                    if packet.track_id() != d.track_id {
                        continue;
                    }
                    match d.decoder.decode(&packet) {
                        Ok(audio) => {
                            let mut sb = SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
                            sb.copy_interleaved_ref(audio);
                            d.pending.extend(sb.samples().iter().copied());
                        }
                        Err(Error::DecodeError(_)) => continue, // skip a damaged frame
                        Err(_) => d.done = true,
                    }
                }
                let take = (d.pending.len().min(CHUNK_FRAMES * d.ch) / d.ch) * d.ch;
                buf.extend(d.pending.drain(..take));
            }
        }
        !buf.is_empty()
    }
}

/// 4x oversampling peak meter (windowed-sinc, 24 taps), in safe code. It replaces the
/// true-peak mode of the loudness crate, whose stereo interpolator crashed on Windows.
struct TruePeak {
    taps: [[f32; 12]; 4],
    hist: Vec<[f32; 12]>,
    peak: f32,
}

impl TruePeak {
    fn new(ch: usize) -> Self {
        let mut taps = [[0f32; 12]; 4];
        for (phase, row) in taps.iter_mut().enumerate() {
            let frac = phase as f64 / 4.0;
            let mut sum = 0.0;
            for (k, t) in row.iter_mut().enumerate() {
                // tap k weights the sample k-5 positions before the interpolation point
                let x = (5.0 - k as f64) + frac - 0.0;
                let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                let w = 0.5 + 0.5 * (std::f64::consts::PI * x / 6.0).cos();
                let v = sinc * if x.abs() <= 6.0 { w } else { 0.0 };
                *t = v as f32;
                sum += v;
            }
            for t in row.iter_mut() {
                *t = (*t as f64 / sum) as f32;
            }
        }
        Self { taps, hist: vec![[0.0; 12]; ch], peak: 0.0 }
    }

    fn add(&mut self, frames: &[f32]) {
        let ch = self.hist.len();
        for frame in frames.chunks_exact(ch) {
            for (c, &x) in frame.iter().enumerate() {
                let h = &mut self.hist[c];
                h.rotate_left(1);
                h[11] = x;
                self.peak = self.peak.max(x.abs());
                // h[11] is the newest sample; the interpolation point sits between h[5] and h[6]
                for row in &self.taps {
                    let v: f32 = row.iter().zip(h.iter()).map(|(t, s)| t * s).sum();
                    self.peak = self.peak.max(v.abs());
                }
            }
        }
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
pub fn level_file(
    input: &Path,
    output: &Path,
    target_lufs: f64,
    smooth: bool,
    progress: &mut dyn FnMut(f32),
) -> Result<LevelReport, String> {
    if !(-40.0..=-6.0).contains(&target_lufs) {
        return Err("The loudness target must be between -40 and -6 LUFS".into());
    }
    let (mut frames, info) = open_source(input)?;
    let ch = info.channels;
    let total = info.frames.max(1) as f32;
    let rate = info.rate;
    let mut format = if info.mp3 {
        RecFormat::Mp3(crate::format::MP3_DEFAULT) // bitrate refined after pass 1
    } else if info.float {
        RecFormat::Wav32f
    } else {
        RecFormat::Wav24
    };

    // Pass 1: measure.
    let mut meter = EbuR128::new(ch as u32, rate, Mode::I).map_err(|e| format!("Loudness meter: {e}"))?;
    let mut buf = Vec::new();
    let mut done = 0f32;
    while frames.next_chunk(&mut buf) {
        meter.add_frames_f32(&buf).map_err(|e| format!("Loudness meter: {e}"))?;
        done += (buf.len() / ch) as f32;
        progress(0.45 * done / total);
    }
    if info.mp3 && done > 0.0 {
        // Keep the original's bitrate: estimate it from file size over decoded duration.
        if let Ok(md) = std::fs::metadata(input) {
            let kbps = md.len() as f64 * 8.0 / (done as f64 / rate as f64) / 1000.0;
            format = RecFormat::Mp3(crate::format::nearest_mp3_rate(kbps));
        }
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
    let (mut frames, _) = open_source(input)?;
    let temp = PathBuf::from(format!("{}.part", output.display()));
    let mut writer = Sink::create(format, &temp, rate, ch)?;
    let mut check = EbuR128::new(ch as u32, rate, Mode::I).map_err(|e| format!("Loudness meter: {e}"))?;
    let mut tp = TruePeak::new(ch);
    let (safety_db, safety_knee) = if smooth { (SMOOTH_SAFETY_DB, 0.0) } else { (SAMPLE_CEILING_DB, 0.0) };
    let mut limiter = Limiter::new(ch, rate, safety_db, safety_knee, LOOKAHEAD_SECS, RELEASE_SECS);
    let mut smoother = smooth.then(|| Limiter::new(ch, rate, SMOOTH_THRESHOLD_DB, SMOOTH_KNEE_DB, SMOOTH_LOOKAHEAD_SECS, SMOOTH_RELEASE_SECS));
    let makeup = 10f32.powf((SMOOTH_OUTPUT_DB - SMOOTH_THRESHOLD_DB) as f32 / 20.0);
    let mut stage: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let mut write = |samples: &[f32], writer: &mut Sink, check: &mut EbuR128| -> Result<(), String> {
        tp.add(samples);
        check.add_frames_f32(samples).map_err(|e| format!("Loudness meter: {e}"))?;
        writer.write(samples).map(|_| ())
    };
    let mut done = 0f32;
    while frames.next_chunk(&mut buf) {
        out.clear();
        for frame in buf.chunks_exact(ch) {
            let scaled: [f32; 2] = [frame[0] * gain, frame[ch - 1] * gain];
            match smoother.as_mut() {
                Some(sm) => {
                    stage.clear();
                    sm.push(&scaled[..ch], &mut stage);
                    for f in stage.chunks_exact(ch) {
                        let up: [f32; 2] = [f[0] * makeup, f[ch - 1] * makeup];
                        limiter.push(&up[..ch], &mut out);
                    }
                }
                None => limiter.push(&scaled[..ch], &mut out),
            }
        }
        write(&out, &mut writer, &mut check)?;
        done += (buf.len() / ch) as f32;
        progress(0.45 + 0.55 * done / total);
    }
    out.clear();
    if let Some(sm) = smoother.as_mut() {
        stage.clear();
        sm.finish(&mut stage);
        for f in stage.chunks_exact(ch) {
            let up: [f32; 2] = [f[0] * makeup, f[ch - 1] * makeup];
            limiter.push(&up[..ch], &mut out);
        }
    }
    limiter.finish(&mut out);
    write(&out, &mut writer, &mut check)?;
    writer.finish()?;
    std::fs::rename(&temp, output).map_err(|e| format!("Could not save {}: {e}", output.display()))?;

    let final_lufs = check.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let peak = tp.peak as f64;
    progress(1.0);
    Ok(LevelReport {
        output: output.to_path_buf(),
        measured_lufs: measured,
        target_lufs,
        gain_db,
        final_lufs,
        true_peak_db: to_db(peak),
        limited: limiter.min_gain < 0.99 || smoother.as_ref().is_some_and(|sm| sm.min_gain < 0.99),
        smoothed: smooth,
    })
}

/// `Name.wav` -> `Name-leveled.wav`, `Name.mp3` -> `Name-leveled.mp3`, next to it.
pub fn leveled_path(input: &Path) -> PathBuf {
    let mp3 = input.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"));
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    input.with_file_name(format!("{stem}-leveled.{}", if mp3 { "mp3" } else { "wav" }))
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
    fn true_peak_meter_sees_peaks_between_samples() {
        // fs/4 sine at 45 degrees: every sample is +-0.707 but the waveform reaches 1.0
        let data: Vec<f32> = (0..400).map(|i| (std::f32::consts::FRAC_PI_2 * i as f32 + std::f32::consts::FRAC_PI_4).sin()).collect();
        let mut tp = TruePeak::new(1);
        tp.add(&data);
        assert!(tp.peak > 0.97 && tp.peak < 1.03, "true peak {}", tp.peak);
    }

    #[test]
    fn quiet_recording_is_brought_to_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("quiet.wav");
        write_test_wav(&input, 48_000, 2, 20.0, 0.03, false);
        let out = leveled_path(&input);
        assert_eq!(out.file_name().unwrap(), "quiet-leveled.wav");
        let mut last = 0.0;
        let r = level_file(&input, &out, -16.0, false, &mut |p| {
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
        let r = level_file(&input, &out, -16.0, false, &mut |_| {}).unwrap();
        assert!(r.limited, "the spikes need limiting");
        assert!(r.true_peak_db <= TRUE_PEAK_CEILING_DB + 0.05, "true peak {} dB", r.true_peak_db);
        // measure again from the written file, independently of the leveler's own numbers
        let mut rd = hound::WavReader::open(&out).unwrap();
        let sample_peak = rd.samples::<i32>().map(|s| s.unwrap().abs()).max().unwrap() as f64 / 8_388_608.0;
        assert!(to_db(sample_peak) <= -1.0, "sample peak {} dBFS", to_db(sample_peak));
    }

    /// Crest factor (peak over RMS, dB) of a written file.
    fn crest_db(path: &Path) -> f64 {
        let mut rd = hound::WavReader::open(path).unwrap();
        let v: Vec<f64> = rd.samples::<i32>().map(|s| s.unwrap() as f64 / 8_388_608.0).collect();
        let peak = v.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        let rms = (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
        20.0 * (peak / rms).log10()
    }

    #[test]
    fn smoothing_evens_out_loud_parts_like_the_audacity_limiter() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("speech.wav");
        write_test_wav(&input, 48_000, 2, 12.0, 0.05, true);
        let plain = dir.path().join("plain.wav");
        let smooth = dir.path().join("smooth.wav");
        let a = level_file(&input, &plain, -16.0, false, &mut |_| {}).unwrap();
        let b = level_file(&input, &smooth, -16.0, true, &mut |_| {}).unwrap();
        assert!(b.smoothed && !a.smoothed);
        assert!(crest_db(&smooth) + 2.0 < crest_db(&plain), "crest {} vs {}", crest_db(&smooth), crest_db(&plain));
        assert!(b.final_lufs > a.final_lufs + 1.0, "makeup gain makes it louder: {} vs {}", b.final_lufs, a.final_lufs);
        assert!(b.true_peak_db <= -0.4, "peaks stay under full scale: {}", b.true_peak_db);
        let (pa, pb) = (hound::WavReader::open(&plain).unwrap().duration(), hound::WavReader::open(&smooth).unwrap().duration());
        assert_eq!(pa, pb, "same length");
    }

    #[test]
    fn soft_knee_gain_curve_matches_the_settings() {
        let l = Limiter::new(1, 48_000, SMOOTH_THRESHOLD_DB, SMOOTH_KNEE_DB, SMOOTH_LOOKAHEAD_SECS, SMOOTH_RELEASE_SECS);
        let db = |peak_db: f64| 20.0 * (l.target_gain(10f64.powf(peak_db / 20.0) as f32) as f64).log10();
        assert_eq!(db(-8.0), 0.0, "below the knee: untouched");
        assert!(db(-6.0) < -0.1 && db(-6.0) > -0.5, "inside the knee: {}", db(-6.0));
        assert!((db(-5.0) - (-1.0)).abs() < 0.01, "above the knee a peak is held at -6: {}", db(-5.0));
        assert!((db(0.0) - (-6.0)).abs() < 0.01, "full scale comes down to -6: {}", db(0.0));
    }

    /// Integrated loudness of any readable file, measured independently of the leveler.
    fn measure(path: &Path) -> f64 {
        let (mut src, info) = open_source(path).unwrap();
        let mut m = EbuR128::new(info.channels as u32, info.rate, Mode::I).unwrap();
        let mut buf = Vec::new();
        while src.next_chunk(&mut buf) {
            m.add_frames_f32(&buf).unwrap();
        }
        m.loudness_global().unwrap()
    }

    fn speech_like(secs: f32, level: f32) -> Vec<f32> {
        (0..(48_000.0 * secs) as usize)
            .flat_map(|i| {
                let t = i as f32 / 48_000.0;
                let env = 0.6 + 0.4 * (t * 1.3).sin();
                let v = level * env * ((t * std::f32::consts::TAU * 220.0).sin() + 0.5 * (t * std::f32::consts::TAU * 1700.0).sin()) / 1.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn float_wav_in_gives_float_wav_out() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("f.wav");
        let mut sink = Sink::create(RecFormat::Wav32f, &input, 48_000, 2).unwrap();
        sink.write(&speech_like(15.0, 0.03)).unwrap();
        sink.finish().unwrap();
        let out = leveled_path(&input);
        let r = level_file(&input, &out, -16.0, false, &mut |_| {}).unwrap();
        let spec = hound::WavReader::open(&out).unwrap().spec();
        assert_eq!((spec.bits_per_sample, spec.sample_format), (32, hound::SampleFormat::Float));
        assert!((r.final_lufs + 16.0).abs() < 0.3, "final {}", r.final_lufs);
        assert!((measure(&out) + 16.0).abs() < 0.3, "re-measured {}", measure(&out));
    }

    #[test]
    fn leveled_mp3_keeps_the_original_bitrate() {
        let dir = tempfile::tempdir().unwrap();
        for kbps in [128u16, 192, 320] {
            let input = dir.path().join(format!("k{kbps}.mp3"));
            let mut sink = Sink::create(RecFormat::Mp3(kbps), &input, 48_000, 2).unwrap();
            sink.write(&speech_like(12.0, 0.03)).unwrap();
            sink.finish().unwrap();
            let out = leveled_path(&input);
            level_file(&input, &out, -16.0, false, &mut |_| {}).unwrap();
            let (a, b) = (std::fs::metadata(&input).unwrap().len() as f64, std::fs::metadata(&out).unwrap().len() as f64);
            assert!((b / a - 1.0).abs() < 0.15, "{kbps}: input {a} output {b}");
        }
    }

    #[test]
    fn mp3_in_gives_leveled_mp3_out() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("talk.mp3");
        let mut sink = Sink::create(RecFormat::Mp3(256), &input, 48_000, 2).unwrap();
        sink.write(&speech_like(15.0, 0.03)).unwrap();
        sink.finish().unwrap();
        let out = leveled_path(&input);
        assert_eq!(out.file_name().unwrap(), "talk-leveled.mp3");
        let mut last = 0.0;
        let r = level_file(&input, &out, -16.0, true, &mut |p| {
            assert!(p >= last);
            last = p;
        })
        .unwrap();
        assert_eq!(last, 1.0);
        assert!(r.measured_lufs < -25.0, "measured {}", r.measured_lufs);
        let size = std::fs::metadata(&out).unwrap().len();
        assert!(size > 256_000 / 8 * 12 && size < 256_000 / 8 * 18, "256k file of about 15 s: {size} bytes");
        assert!(measure(&out) > measure(&input) + 8.0, "leveled copy is much louder: {} vs {}", measure(&out), measure(&input));
        assert!(!PathBuf::from(format!("{}.part", out.display())).exists(), "temp file is gone");
    }

    #[test]
    fn mono_files_and_loud_files_work_too() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("loud.wav");
        write_test_wav(&input, 44_100, 1, 8.0, 0.5, false);
        let r = level_file(&input, &dir.path().join("o.wav"), -19.0, false, &mut |_| {}).unwrap();
        assert!(r.gain_db < 0.0, "a loud file is turned down");
        assert!((r.final_lufs + 19.0).abs() < 0.3, "final {}", r.final_lufs);
    }

    #[test]
    fn silence_and_bad_input_give_clear_errors() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("silent.wav");
        write_test_wav(&input, 48_000, 2, 3.0, 0.0, false);
        let err = level_file(&input, &dir.path().join("o.wav"), -16.0, false, &mut |_| {}).unwrap_err();
        assert!(err.contains("silent"), "{err}");
        assert!(level_file(&input, &dir.path().join("o.wav"), 5.0, false, &mut |_| {}).is_err());
        assert!(level_file(&dir.path().join("missing.wav"), &dir.path().join("o.wav"), -16.0, false, &mut |_| {}).is_err());
    }

    #[test]
    fn a_file_cut_short_by_a_crash_can_still_be_leveled() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("cut.wav");
        write_test_wav(&input, 48_000, 2, 6.0, 0.03, false);
        let mut bytes = std::fs::read(&input).unwrap();
        bytes.truncate(bytes.len() - 100_001); // data chunk now ends mid-frame, header still claims more
        std::fs::write(&input, bytes).unwrap();
        let r = level_file(&input, &dir.path().join("o.wav"), -16.0, false, &mut |_| {}).unwrap();
        assert!((r.final_lufs + 16.0).abs() < 0.5, "final {}", r.final_lufs);
    }
}
