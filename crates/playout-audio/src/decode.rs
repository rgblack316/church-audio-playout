//! Decode an audio file fully into memory as 16-bit PCM (mono or stereo).
//!
//! symphonia handles MP3, WAV, FLAC, OGG, M4A/AAC. When it fails (e.g. WMA) and the
//! `mf` feature is on, Windows Media Foundation is tried as a fallback.

use std::{fs::File, path::Path};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_NULL},
    errors::Error,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};

/// Refuse files longer than this (they are held fully in memory).
pub const MAX_MINUTES: u64 = 45;

#[derive(Debug)]
pub struct Decoded {
    /// 1 or 2.
    pub channels: u16,
    pub sample_rate: u32,
    /// Interleaved 16-bit samples.
    pub samples: Vec<i16>,
}

impl Decoded {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }
}

pub fn decode_file(path: &Path) -> Result<Decoded, String> {
    let first = match decode_symphonia(path) {
        Ok(d) => return Ok(d),
        Err(e) => e,
    };
    fallback(path, first)
}

#[cfg(feature = "mf")]
fn fallback(path: &Path, first: String) -> Result<Decoded, String> {
    crate::mf::decode(path)
        .and_then(check_length)
        .map_err(|second| format!("{first}; Windows decoder also failed: {second}"))
}

#[cfg(not(feature = "mf"))]
fn fallback(_path: &Path, first: String) -> Result<Decoded, String> {
    Err(first)
}

pub(crate) fn check_length(d: Decoded) -> Result<Decoded, String> {
    if d.samples.is_empty() {
        return Err("no audio found in file".into());
    }
    if d.duration_secs() > (MAX_MINUTES * 60) as f64 {
        return Err(format!("file is longer than {MAX_MINUTES} minutes"));
    }
    Ok(d)
}

/// Keep the first two channels of interleaved multichannel audio.
pub(crate) fn push_stereo(out: &mut Vec<i16>, interleaved: &[i16], channels: usize) {
    match channels {
        0 => {}
        1 | 2 => out.extend_from_slice(interleaved),
        n => {
            for frame in interleaved.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}

fn decode_symphonia(path: &Path) -> Result<Decoded, String> {
    let file = File::open(path).map_err(|e| format!("cannot open file: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("unsupported format ({e})"))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no audio track".to_string())?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("unsupported codec ({e})"))?;

    let mut samples: Vec<i16> = Vec::new();
    let mut out_channels = 0u16;
    let mut rate = 0u32;
    let mut sbuf: Option<SampleBuffer<i16>> = None;
    let max_samples_per_sec_factor = MAX_MINUTES * 60;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => {
                if samples.is_empty() {
                    return Err(format!("read error ({e})"));
                }
                break;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                let ch = spec.channels.count();
                if out_channels == 0 {
                    out_channels = if ch == 1 { 1 } else { 2 };
                    rate = spec.rate;
                }
                let needed = buf.frames() * ch;
                if sbuf.as_ref().map_or(true, |b| b.capacity() < needed) {
                    sbuf = Some(SampleBuffer::<i16>::new(buf.capacity() as u64, spec));
                }
                let sb = sbuf.as_mut().unwrap();
                sb.copy_interleaved_ref(buf);
                push_stereo(&mut samples, sb.samples(), ch);
                if samples.len() as u64 > rate as u64 * 2 * max_samples_per_sec_factor {
                    return Err(format!("file is longer than {MAX_MINUTES} minutes"));
                }
            }
            Err(Error::DecodeError(_)) => continue, // skip a damaged packet
            Err(e) => {
                if samples.is_empty() {
                    return Err(format!("decode error ({e})"));
                }
                break;
            }
        }
    }

    check_length(Decoded { channels: out_channels.max(1), sample_rate: rate, samples })
}
