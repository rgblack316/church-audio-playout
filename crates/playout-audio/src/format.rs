//! Audio file formats for recording and leveling: 24-bit WAV, 32-bit float WAV and MP3 (128-320 kbps).

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufWriter, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecFormat {
    /// 24-bit integer WAV (lossless).
    #[default]
    Wav24,
    /// 32-bit float WAV (lossless, keeps peaks above full scale).
    Wav32f,
    /// MP3 at the given bitrate in kbps (one of `MP3_RATES`).
    Mp3(u16),
}

/// MP3 bitrates offered, best first. All are valid at 32, 44.1 and 48 kHz.
pub const MP3_RATES: [u16; 6] = [320, 256, 224, 192, 160, 128];
pub const MP3_DEFAULT: u16 = 256;

/// The offered bitrate closest to `kbps`.
pub fn nearest_mp3_rate(kbps: f64) -> u16 {
    *MP3_RATES.iter().min_by(|a, b| (**a as f64 - kbps).abs().total_cmp(&(**b as f64 - kbps).abs())).unwrap()
}

fn bitrate_of(kbps: u16) -> Bitrate {
    match kbps {
        320 => Bitrate::Kbps320,
        224 => Bitrate::Kbps224,
        192 => Bitrate::Kbps192,
        160 => Bitrate::Kbps160,
        128 => Bitrate::Kbps128,
        _ => Bitrate::Kbps256,
    }
}

impl RecFormat {
    pub fn key(self) -> &'static str {
        match self {
            RecFormat::Wav24 => "wav24",
            RecFormat::Wav32f => "wav32f",
            RecFormat::Mp3(MP3_DEFAULT) => "mp3",
            RecFormat::Mp3(320) => "mp3-320",
            RecFormat::Mp3(224) => "mp3-224",
            RecFormat::Mp3(192) => "mp3-192",
            RecFormat::Mp3(160) => "mp3-160",
            RecFormat::Mp3(128) => "mp3-128",
            RecFormat::Mp3(_) => "mp3",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "wav24" => Some(RecFormat::Wav24),
            "wav32f" => Some(RecFormat::Wav32f),
            "mp3" => Some(RecFormat::Mp3(MP3_DEFAULT)),
            _ => {
                let kbps: u16 = key.strip_prefix("mp3-")?.parse().ok()?;
                MP3_RATES.contains(&kbps).then_some(RecFormat::Mp3(kbps))
            }
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            RecFormat::Wav24 | RecFormat::Wav32f => "wav",
            RecFormat::Mp3(_) => "mp3",
        }
    }

    /// WAV files cannot pass 4 GiB, so recordings in them are split into parts.
    pub fn is_wav(self) -> bool {
        !matches!(self, RecFormat::Mp3(_))
    }
}

type Wav = hound::WavWriter<BufWriter<File>>;

enum Kind {
    Wav24(Wav),
    Wav32(Wav),
    Mp3 { enc: mp3lame_encoder::Encoder, file: BufWriter<File>, buf: Vec<u8>, clamped: Vec<f32> },
}

/// Writes interleaved f32 samples in one of the formats.
pub struct Sink {
    kind: Kind,
    channels: usize,
}

impl Sink {
    pub fn create(format: RecFormat, path: &Path, rate: u32, channels: usize) -> Result<Sink, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        }
        let create_err = |e: &dyn std::fmt::Display| format!("Could not create {}: {e}", path.display());
        let kind = match format {
            RecFormat::Wav24 | RecFormat::Wav32f => {
                let float = format == RecFormat::Wav32f;
                let spec = hound::WavSpec {
                    channels: channels as u16,
                    sample_rate: rate,
                    bits_per_sample: if float { 32 } else { 24 },
                    sample_format: if float { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
                };
                let w = hound::WavWriter::create(path, spec).map_err(|e| create_err(&e))?;
                if float {
                    Kind::Wav32(w)
                } else {
                    Kind::Wav24(w)
                }
            }
            RecFormat::Mp3(kbps) => {
                let mut b = Builder::new().ok_or("Could not start the MP3 encoder")?;
                let e = |e: mp3lame_encoder::BuildError| format!("MP3 encoder: {e:?}");
                b.set_num_channels(channels as u8).map_err(e)?;
                b.set_sample_rate(rate).map_err(e)?;
                // These bitrates need a rate MPEG-1 supports; anything else is converted to 48 kHz.
                let out_rate = if matches!(rate, 32_000 | 44_100 | 48_000) { rate } else { 48_000 };
                b.set_output_sample_rate(NonZeroU32::new(out_rate)).map_err(e)?;
                b.set_brate(bitrate_of(kbps)).map_err(e)?;
                b.set_quality(Quality::Good).map_err(e)?;
                b.set_mode(if channels == 1 { mp3lame_encoder::Mode::Mono } else { mp3lame_encoder::Mode::JointStereo }).map_err(e)?;
                let enc = b.build().map_err(e)?;
                let file = BufWriter::new(File::create(path).map_err(|e| create_err(&e))?);
                Kind::Mp3 { enc, file, buf: Vec::new(), clamped: Vec::new() }
            }
        };
        Ok(Sink { kind, channels })
    }

    /// Write interleaved samples. Returns the bytes this added to the file (approximate for MP3).
    pub fn write(&mut self, samples: &[f32]) -> Result<u64, String> {
        let fail = |e: &dyn std::fmt::Display| format!("Writing the recording failed: {e}");
        match &mut self.kind {
            Kind::Wav24(w) => {
                for s in samples {
                    w.write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32).map_err(|e| fail(&e))?;
                }
                Ok(samples.len() as u64 * 3)
            }
            Kind::Wav32(w) => {
                for s in samples {
                    w.write_sample(*s).map_err(|e| fail(&e))?;
                }
                Ok(samples.len() as u64 * 4)
            }
            Kind::Mp3 { enc, file, buf, clamped } => {
                clamped.clear();
                clamped.extend(samples.iter().map(|s| s.clamp(-1.0, 1.0)));
                buf.clear();
                buf.reserve(mp3lame_encoder::max_required_buffer_size(clamped.len() / self.channels.max(1)));
                let r = if self.channels == 1 { enc.encode_to_vec(MonoPcm(clamped.as_slice()), buf) } else { enc.encode_to_vec(InterleavedPcm(clamped.as_slice()), buf) };
                r.map_err(|e| format!("MP3 encoder: {e:?}"))?;
                file.write_all(buf).map_err(|e| fail(&e))?;
                Ok(buf.len() as u64)
            }
        }
    }

    /// Push what is written so far to disk so a crash loses almost nothing.
    pub fn flush(&mut self) -> Result<(), String> {
        let fail = |e: &dyn std::fmt::Display| format!("Writing the recording failed: {e}");
        match &mut self.kind {
            Kind::Wav24(w) | Kind::Wav32(w) => w.flush().map_err(|e| fail(&e)),
            Kind::Mp3 { file, .. } => file.flush().map_err(|e| fail(&e)),
        }
    }

    pub fn finish(self) -> Result<(), String> {
        let fail = |e: &dyn std::fmt::Display| format!("Finishing the recording failed: {e}");
        match self.kind {
            Kind::Wav24(w) | Kind::Wav32(w) => w.finalize().map_err(|e| fail(&e)),
            Kind::Mp3 { mut enc, mut file, mut buf, .. } => {
                buf.clear();
                buf.reserve(8192);
                enc.flush_to_vec::<FlushNoGap>(&mut buf).map_err(|e| format!("MP3 encoder: {e:?}"))?;
                file.write_all(&buf).map_err(|e| fail(&e))?;
                file.flush().map_err(|e| fail(&e))
            }
        }
    }
}

/// `Name.wav` + `-leveled` keeps the extension: `Name-leveled.wav` / `Name-leveled.mp3`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "wav".into());
    path.with_file_name(format!("{stem}{suffix}.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_old_mp3_key_is_256() {
        for f in [RecFormat::Wav24, RecFormat::Wav32f].into_iter().chain(MP3_RATES.iter().map(|k| RecFormat::Mp3(*k))) {
            assert_eq!(RecFormat::from_key(f.key()), Some(f));
        }
        assert_eq!(RecFormat::from_key("mp3"), Some(RecFormat::Mp3(256)));
        assert_eq!(RecFormat::from_key("mp3-144"), None);
        assert_eq!(RecFormat::from_key("mp3-abc"), None);
    }

    #[test]
    fn nearest_rate_snaps() {
        assert_eq!(nearest_mp3_rate(131.0), 128);
        assert_eq!(nearest_mp3_rate(250.0), 256);
        assert_eq!(nearest_mp3_rate(400.0), 320);
        assert_eq!(nearest_mp3_rate(100.0), 128);
    }

    #[test]
    fn every_bitrate_makes_a_file_of_about_that_size() {
        let dir = tempfile::tempdir().unwrap();
        let secs = 10usize;
        let block: Vec<f32> = (0..48_000 * secs)
            .flat_map(|i| {
                let v = (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.4;
                [v, v]
            })
            .collect();
        for kbps in MP3_RATES {
            let path = dir.path().join(format!("t{kbps}.mp3"));
            let mut s = Sink::create(RecFormat::Mp3(kbps), &path, 48_000, 2).unwrap();
            s.write(&block).unwrap();
            s.finish().unwrap();
            let size = std::fs::metadata(&path).unwrap().len();
            let expected = kbps as u64 * 1000 / 8 * secs as u64;
            assert!(size > expected * 85 / 100 && size < expected * 115 / 100, "{kbps}: {size} vs {expected}");
        }
    }
}
