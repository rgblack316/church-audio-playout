//! Pad state and the sample-mixing routine used by the audio callback.

use crate::decode::Decoded;
use std::sync::Arc;

pub struct Pad {
    pub decoded: Option<Arc<Decoded>>,
    pub label: String,
    pub sub: String,
    /// Playback position in source frames.
    pub pos: f64,
    pub playing: bool,
    pub gain: f32,
}

impl Pad {
    fn empty() -> Self {
        Self { decoded: None, label: String::new(), sub: String::new(), pos: 0.0, playing: false, gain: 1.0 }
    }
}

pub struct Mixer {
    pub pads: Vec<Pad>,
    /// When false, starting a pad stops all the others.
    pub overlap: bool,
}

impl Mixer {
    pub fn new(pad_count: usize) -> Self {
        Self { pads: (0..pad_count).map(|_| Pad::empty()).collect(), overlap: true }
    }

    pub fn resize(&mut self, pad_count: usize) {
        self.pads.truncate(pad_count);
        while self.pads.len() < pad_count {
            self.pads.push(Pad::empty());
        }
    }

    /// Fill `buf` (interleaved, `out_channels` wide) with the sum of all playing pads.
    pub fn render(&mut self, out_channels: usize, out_rate: f64, buf: &mut [f32]) {
        buf.fill(0.0);
        let oc = out_channels.max(1);
        let frames = buf.len() / oc;

        for pad in self.pads.iter_mut() {
            if !pad.playing {
                continue;
            }
            let Pad { decoded, pos, playing, gain, .. } = pad;
            let dec = match decoded {
                Some(d) => d,
                None => {
                    *playing = false;
                    continue;
                }
            };
            let total = dec.frames();
            let src_ch = dec.channels as usize;
            let step = dec.sample_rate as f64 / out_rate;
            let s = &dec.samples;
            let g = *gain / 32768.0;

            for i in 0..frames {
                let idx = *pos as usize;
                if idx >= total {
                    *playing = false;
                    *pos = 0.0;
                    break;
                }
                let frac = (*pos - idx as f64) as f32;
                let idx1 = (idx + 1).min(total - 1);
                let (l, r) = if src_ch == 1 {
                    let a = s[idx] as f32;
                    let b = s[idx1] as f32;
                    let v = a + (b - a) * frac;
                    (v, v)
                } else {
                    let (la, ra) = (s[idx * 2] as f32, s[idx * 2 + 1] as f32);
                    let (lb, rb) = (s[idx1 * 2] as f32, s[idx1 * 2 + 1] as f32);
                    (la + (lb - la) * frac, ra + (rb - ra) * frac)
                };
                if oc == 1 {
                    buf[i] += (l + r) * 0.5 * g;
                } else {
                    buf[i * oc] += l * g;
                    buf[i * oc + 1] += r * g;
                }
                *pos += step;
            }
        }

        for v in buf.iter_mut() {
            *v = v.clamp(-1.0, 1.0);
        }
    }
}
