//! Audio engine: one output stream on the default Windows device, many pads mixed into it.

pub mod decode;
pub mod mixer;
#[cfg(feature = "mf")]
mod mf;

pub use decode::{decode_file, Decoded};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use mixer::Mixer;
use serde::Serialize;
use std::sync::{
    mpsc::{channel, Sender},
    Arc, Mutex, MutexGuard,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AudioStatus {
    pub ok: bool,
    pub device: String,
    pub sample_rate: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PadInfo {
    pub index: usize,
    pub loaded: bool,
    pub label: String,
    pub sub: String,
    pub duration_secs: f64,
    pub position_secs: f64,
    pub playing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub audio: AudioStatus,
    pub overlap: bool,
    pub pads: Vec<PadInfo>,
}

enum Ctl {
    Restart,
    Shutdown,
}

pub struct Engine {
    mixer: Arc<Mutex<Mixer>>,
    status: Arc<Mutex<AudioStatus>>,
    ctl: Mutex<Sender<Ctl>>,
}

impl Engine {
    /// Create the engine and start the audio thread. Never fails: if there is no usable
    /// output device the problem is reported in `snapshot().audio` and can be retried
    /// with `restart_audio`.
    pub fn new(pad_count: usize) -> Self {
        let mixer = Arc::new(Mutex::new(Mixer::new(pad_count)));
        let status = Arc::new(Mutex::new(AudioStatus {
            error: Some("Audio is starting…".into()),
            ..Default::default()
        }));
        let (tx, rx) = channel::<Ctl>();
        {
            let mixer = mixer.clone();
            let status = status.clone();
            // The cpal stream is not Send on every platform, so one thread owns it.
            let _ = std::thread::Builder::new().name("audio".into()).spawn(move || {
                let mut stream = open(&mixer, &status);
                loop {
                    match rx.recv() {
                        Ok(Ctl::Restart) => {
                            drop(stream.take());
                            stream = open(&mixer, &status);
                        }
                        Ok(Ctl::Shutdown) | Err(_) => break,
                    }
                }
                drop(stream);
            });
        }
        Engine { mixer, status, ctl: Mutex::new(tx) }
    }

    /// Re-open the default output device (e.g. after plugging in a USB interface).
    pub fn restart_audio(&self) {
        {
            let mut m = lock(&self.mixer);
            for p in m.pads.iter_mut() {
                p.playing = false;
                p.pos = 0.0;
            }
        }
        lock(&self.status).error = Some("Audio is restarting…".into());
        lock(&self.status).ok = false;
        let _ = lock(&self.ctl).send(Ctl::Restart);
    }

    pub fn set_overlap(&self, overlap: bool) {
        lock(&self.mixer).overlap = overlap;
    }

    pub fn set_pad_count(&self, n: usize) {
        lock(&self.mixer).resize(n.clamp(1, 64));
    }

    /// Fail fast (before a slow decode) if the pad cannot accept a new file.
    pub fn check_loadable(&self, pad: usize) -> Result<(), String> {
        let m = lock(&self.mixer);
        match m.pads.get(pad) {
            None => Err("No such pad".into()),
            Some(p) if p.playing => Err("That pad is playing; stop it first".into()),
            Some(_) => Ok(()),
        }
    }

    pub fn load(&self, pad: usize, decoded: Arc<Decoded>, label: String, sub: String) -> Result<(), String> {
        let mut m = lock(&self.mixer);
        let p = m.pads.get_mut(pad).ok_or("No such pad")?;
        if p.playing {
            return Err("That pad is playing; stop it first".into());
        }
        p.decoded = Some(decoded);
        p.label = label;
        p.sub = sub;
        p.pos = 0.0;
        Ok(())
    }

    pub fn clear(&self, pad: usize) -> Result<(), String> {
        let mut m = lock(&self.mixer);
        let p = m.pads.get_mut(pad).ok_or("No such pad")?;
        if p.playing {
            return Err("That pad is playing; stop it first".into());
        }
        p.decoded = None;
        p.label.clear();
        p.sub.clear();
        p.pos = 0.0;
        Ok(())
    }

    /// Start a pad from the beginning. With overlap off, every other pad is stopped first.
    pub fn play(&self, pad: usize) -> Result<(), String> {
        if !lock(&self.status).ok {
            return Err("Audio output is not available".into());
        }
        let mut m = lock(&self.mixer);
        if pad >= m.pads.len() {
            return Err("No such pad".into());
        }
        if m.pads[pad].decoded.is_none() {
            return Err("That pad is empty".into());
        }
        if !m.overlap {
            for (i, p) in m.pads.iter_mut().enumerate() {
                if i != pad {
                    p.playing = false;
                    p.pos = 0.0;
                }
            }
        }
        let p = &mut m.pads[pad];
        p.pos = 0.0;
        p.playing = true;
        Ok(())
    }

    /// Stop a pad and rewind it to the start.
    pub fn stop(&self, pad: usize) {
        if let Some(p) = lock(&self.mixer).pads.get_mut(pad) {
            p.playing = false;
            p.pos = 0.0;
        }
    }

    pub fn stop_all(&self) {
        for p in lock(&self.mixer).pads.iter_mut() {
            p.playing = false;
            p.pos = 0.0;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let audio = lock(&self.status).clone();
        let m = lock(&self.mixer);
        let pads = m
            .pads
            .iter()
            .enumerate()
            .map(|(index, p)| {
                let (duration, position) = match &p.decoded {
                    Some(d) => (d.duration_secs(), (p.pos / d.sample_rate.max(1) as f64).min(d.duration_secs())),
                    None => (0.0, 0.0),
                };
                PadInfo {
                    index,
                    loaded: p.decoded.is_some(),
                    label: p.label.clone(),
                    sub: p.sub.clone(),
                    duration_secs: duration,
                    position_secs: position,
                    playing: p.playing,
                }
            })
            .collect();
        Snapshot { audio, overlap: m.overlap, pads }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = lock(&self.ctl).send(Ctl::Shutdown);
    }
}

fn open(mixer: &Arc<Mutex<Mixer>>, status: &Arc<Mutex<AudioStatus>>) -> Option<cpal::Stream> {
    match try_open(mixer, status) {
        Ok((stream, st)) => {
            *lock(status) = st;
            Some(stream)
        }
        Err(e) => {
            *lock(status) = AudioStatus { ok: false, error: Some(e), ..Default::default() };
            None
        }
    }
}

fn try_open(
    mixer: &Arc<Mutex<Mixer>>,
    status: &Arc<Mutex<AudioStatus>>,
) -> Result<(cpal::Stream, AudioStatus), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "No audio output device found".to_string())?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("Could not read the audio device format: {e}"))?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let name = device.name().unwrap_or_else(|_| "Default output".into());

    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, mixer.clone(), status.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, mixer.clone(), status.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, mixer.clone(), status.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, mixer.clone(), status.clone()),
        other => Err(format!("Unsupported audio sample format: {other:?}")),
    }?;
    stream.play().map_err(|e| format!("Could not start audio output: {e}"))?;

    Ok((
        stream,
        AudioStatus { ok: true, device: name, sample_rate: config.sample_rate.0, error: None },
    ))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    status: Arc<Mutex<AudioStatus>>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate.0 as f64;
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                scratch.resize(data.len(), 0.0);
                lock(&mixer).render(channels, rate, &mut scratch);
                for (out, s) in data.iter_mut().zip(scratch.iter()) {
                    *out = T::from_sample(*s);
                }
            },
            move |err| {
                let mut s = lock(&status);
                s.ok = false;
                s.error = Some(format!("Audio stream error: {err}"));
            },
            None,
        )
        .map_err(|e| format!("Could not open audio output: {e}"))
}

#[cfg(test)]
mod tests;
