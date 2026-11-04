use super::*;
use crate::mixer::{Bank, Mixer};
use std::{io::Write, sync::Arc};

fn wav_bytes(rate: u32, channels: u16, frames: u32, f: impl Fn(u32) -> i16) -> Vec<u8> {
    let data_len = frames * channels as u32 * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    v.extend_from_slice(&(channels * 2).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        for _ in 0..channels {
            v.extend_from_slice(&f(i).to_le_bytes());
        }
    }
    v
}

fn decoded(rate: u32, channels: u16, samples: Vec<i16>) -> Arc<Decoded> {
    Arc::new(Decoded { channels, sample_rate: rate, samples })
}

#[test]
fn decodes_stereo_wav() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.wav");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&wav_bytes(8000, 2, 8000, |i| (i % 100) as i16 * 100))
        .unwrap();
    let d = decode_file(&path).unwrap();
    assert_eq!((d.channels, d.sample_rate, d.frames()), (2, 8000, 8000));
    assert!((d.duration_secs() - 1.0).abs() < 1e-9);
}

#[test]
fn decodes_mono_wav_and_rejects_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mono.wav");
    std::fs::write(&path, wav_bytes(22050, 1, 2205, |_| 1000)).unwrap();
    let d = decode_file(&path).unwrap();
    assert_eq!((d.channels, d.frames()), (1, 2205));

    let bad = dir.path().join("bad.mp3");
    std::fs::write(&bad, b"this is not audio at all").unwrap();
    assert!(decode_file(&bad).is_err());
    assert!(decode_file(&dir.path().join("missing.mp3")).is_err());
}

#[test]
fn mixer_plays_to_end_then_rewinds() {
    let mut m = Mixer::new(2, 0);
    // 4 mono frames at the output rate: full-scale positive.
    m.pads[0].decoded = Some(decoded(100, 1, vec![16384; 4]));
    m.pads[0].playing = true;
    let mut buf = vec![0.0f32; 8 * 2]; // 8 stereo frames
    m.render(2, 100.0, &mut buf);
    // first 4 frames carry audio (0.5), the rest are silence
    for f in 0..4 {
        assert!((buf[f * 2] - 0.5).abs() < 1e-4 && (buf[f * 2 + 1] - 0.5).abs() < 1e-4, "frame {f}");
    }
    for f in 4..8 {
        assert_eq!((buf[f * 2], buf[f * 2 + 1]), (0.0, 0.0), "frame {f}");
    }
    assert!(!m.pads[0].playing);
    assert_eq!(m.pads[0].pos, 0.0);
}

#[test]
fn mixer_sums_pads_clamps_and_resamples() {
    let mut m = Mixer::new(2, 0);
    m.pads[0].decoded = Some(decoded(100, 1, vec![30000; 10]));
    m.pads[1].decoded = Some(decoded(100, 1, vec![30000; 10]));
    m.pads[0].playing = true;
    m.pads[1].playing = true;
    let mut buf = vec![0.0f32; 4 * 2];
    m.render(2, 100.0, &mut buf);
    assert!(buf.iter().all(|v| *v == 1.0), "two loud pads must clamp, got {buf:?}");

    // 50 Hz source into a 100 Hz output: position advances at half speed.
    let mut m = Mixer::new(1, 0);
    m.pads[0].decoded = Some(decoded(50, 1, vec![0, 20000, 0, 0, 0, 0, 0, 0]));
    m.pads[0].playing = true;
    let mut buf = vec![0.0f32; 4];
    m.render(1, 100.0, &mut buf);
    assert!((m.pads[0].pos - 2.0).abs() < 1e-9);
    // frame 1 is halfway between samples 0 and 1 (linear interpolation)
    assert!((buf[1] - (10000.0 / 32768.0)).abs() < 1e-4, "{buf:?}");
}

const M: Bank = Bank::Main;
const F: Bank = Bank::Fav;

#[test]
fn engine_pad_bookkeeping_without_audio_device() {
    let e = Engine::new(3, 2);
    let d = decoded(1000, 2, vec![0; 2000 * 3]); // 3 seconds stereo
    assert!(e.check_loadable(M, 0).is_ok());
    assert!(e.check_loadable(M, 9).is_err());
    e.load(M, 1, d, "Amazing Grace".into(), "Hymnal · p. 123".into()).unwrap();
    let s = e.snapshot();
    assert_eq!((s.pads.len(), s.favs.len()), (3, 2));
    assert!(!s.pads[0].loaded && s.pads[1].loaded);
    assert_eq!(s.pads[1].label, "Amazing Grace");
    assert!((s.pads[1].duration_secs - 3.0).abs() < 1e-9);
    assert_eq!(s.pads[1].position_secs, 0.0);
    assert!(e.play(M, 0).is_err()); // empty pad or no device: either way an error
    e.clear(M, 1).unwrap();
    assert!(!e.snapshot().pads[1].loaded);
}

#[test]
fn resize_keeps_loaded_pads_and_refuses_when_playing() {
    let e = Engine::new(4, 2);
    e.load(M, 1, decoded(1000, 1, vec![0; 3000]), "One".into(), String::new()).unwrap();
    e.set_pad_count(8).unwrap();
    assert_eq!(e.snapshot().pads.len(), 8);
    assert!(e.snapshot().pads[1].loaded);
    e.load(M, 6, decoded(1000, 1, vec![0; 3000]), "Six".into(), String::new()).unwrap();
    e.start(M, 6).unwrap();
    assert!(e.set_pad_count(4).is_err(), "pad 7 is playing");
    assert_eq!(e.snapshot().pads.len(), 8);
    e.stop(M, 6);
    e.set_pad_count(4).unwrap();
    assert_eq!(e.snapshot().pads.len(), 4);
    assert!(e.snapshot().pads[1].loaded);
    assert_eq!(e.snapshot().favs.len(), 2, "favorites are not affected by the grid size");
}

#[test]
fn overlap_off_stops_pads_in_both_banks() {
    let e = Engine::new(2, 2);
    for (b, i) in [(M, 0), (M, 1), (F, 0), (F, 1)] {
        e.load(b, i, decoded(1000, 1, vec![0; 3000]), format!("{b:?}{i}"), String::new()).unwrap();
    }
    e.start(M, 0).unwrap();
    e.start(F, 0).unwrap();
    let s = e.snapshot();
    assert!(s.pads[0].playing && s.favs[0].playing, "overlap is on by default");
    e.set_overlap(false);
    e.start(F, 1).unwrap();
    let s = e.snapshot();
    assert!(!s.pads[0].playing && !s.favs[0].playing && s.favs[1].playing);
    assert!(!s.overlap);
    e.start(M, 1).unwrap();
    assert!(!e.snapshot().favs[1].playing && e.snapshot().pads[1].playing);
    e.stop_all();
    assert!(e.snapshot().pads.iter().chain(e.snapshot().favs.iter()).all(|p| !p.playing));
}

#[test]
fn favorites_loading_and_error_state() {
    let e = Engine::new(1, 3);
    assert!(!e.is_busy_or_loaded(F, 2));
    e.set_loading(F, 2, true);
    assert!(e.snapshot().favs[2].loading);
    assert!(e.is_busy_or_loaded(F, 2), "a pad being loaded must not be loaded twice");
    e.set_error(F, 2, "Missing Song".into(), "cannot open file".into());
    let p = e.snapshot().favs[2].clone();
    assert!(!p.loading && !p.loaded);
    assert_eq!((p.label.as_str(), p.error.as_str()), ("Missing Song", "cannot open file"));
    assert!(!e.is_busy_or_loaded(F, 2), "an errored favorite can be retried");
    e.load(F, 2, decoded(1000, 1, vec![0; 1000]), "Song".into(), "Book".into()).unwrap();
    let p = e.snapshot().favs[2].clone();
    assert!(p.loaded && p.error.is_empty());
    assert!(e.is_busy_or_loaded(F, 2));
}

#[test]
fn pcm_depth_conversion() {
    use crate::decode::pcm_bytes_to_i16;
    let conv = |bytes: &[u8], bits| {
        let mut v = Vec::new();
        pcm_bytes_to_i16(bytes, bits, &mut v).map(|_| v)
    };
    assert_eq!(conv(&[0x34, 0x12, 0xFF, 0x7F], 16).unwrap(), vec![0x1234, 0x7FFF]);
    // 24-bit: keep the top 16 bits
    assert_eq!(conv(&[0x00, 0x34, 0x12, 0x99, 0xFF, 0x7F], 24).unwrap(), vec![0x1234, 0x7FFF]);
    // 32-bit: keep the top 16 bits
    assert_eq!(conv(&[0, 0, 0x34, 0x12], 32).unwrap(), vec![0x1234]);
    // 8-bit unsigned: 128 is silence
    assert_eq!(conv(&[128, 255, 0], 8).unwrap(), vec![0, 127 << 8, -128 << 8]);
    assert!(conv(&[0; 4], 12).is_err());
}

#[test]
fn unknown_output_device_falls_back_instead_of_failing() {
    let e = Engine::new(1, 1);
    e.set_device(Some("No Such Device 12345".into()));
    // Give the audio thread a moment; on machines without audio it reports an error instead.
    let mut snap = e.snapshot();
    for _ in 0..50 {
        snap = e.snapshot();
        if snap.audio.ok || snap.audio.error.as_deref().is_some_and(|m| !m.contains("restarting")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    if snap.audio.ok {
        assert_eq!(snap.audio.missing_device.as_deref(), Some("No Such Device 12345"));
    } else {
        assert!(snap.audio.error.is_some());
    }
    e.set_device(None);
}

#[test]
fn linux_device_names_become_readable_labels() {
    use crate::{alsa_label, label_devices};
    assert_eq!(alsa_label("sysdefault:CARD=PCH").as_deref(), Some("PCH"));
    assert_eq!(alsa_label("hdmi:CARD=NVidia,DEV=2").as_deref(), Some("NVidia \u{2014} HDMI 3"));
    assert_eq!(alsa_label("pipewire").as_deref(), Some("PipeWire (system mixer)"));
    assert_eq!(alsa_label("hw:CARD=PCH,DEV=0"), None);
    assert_eq!(alsa_label("surround40:CARD=PCH,DEV=0"), None);
    assert_eq!(alsa_label("null"), None);
    let names: Vec<String> = ["hw:CARD=PCH,DEV=0", "default", "pulse", "pipewire", "sysdefault:CARD=USB_Mixer", "front:CARD=PCH,DEV=0"]
        .iter().map(|s| s.to_string()).collect();
    let list = label_devices(names.clone(), None, true);
    let labels: Vec<&str> = list.iter().map(|d| d.label.as_str()).collect();
    assert_eq!(labels, ["PipeWire (system mixer)", "PulseAudio (system mixer)", "System default (ALSA)", "USB Mixer"]);
    // A saved technical name stays selectable; other platforms are left untouched.
    assert!(label_devices(names.clone(), Some("hw:CARD=PCH,DEV=0"), true).iter().any(|d| d.name == "hw:CARD=PCH,DEV=0"));
    assert_eq!(label_devices(vec!["Speakers".into(), "Headphones".into()], None, false)[0].label, "Headphones");
    // Nothing recognisable: show everything rather than an empty list.
    assert_eq!(label_devices(vec!["hw:CARD=X,DEV=0".into()], None, true).len(), 1);
}
