//! Windows Media Foundation fallback decoder (handles WMA and other Windows codecs).
//!
//! Follows Microsoft's "Source Reader" recipe: open the file, ask for 16-bit PCM, read back
//! what the decoder actually produces (converting if it differs), then pull samples until
//! end of stream.

use crate::decode::{pcm_bytes_to_i16, push_stereo, Decoded};
use std::path::Path;
use windows::{
    core::PCWSTR,
    Win32::{
        Media::MediaFoundation::*,
        System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
    },
};

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Describe a failed Media Foundation call: which step, the Windows message and code,
/// and a hint for the codes that usually mean "Windows lacks the codec".
fn step(name: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| {
        let code = e.code().0 as u32;
        let hint = match code {
            // MF_E_UNSUPPORTED_BYTESTREAM_TYPE, MF_E_TOPO_CODEC_NOT_FOUND, MF_E_INVALIDMEDIATYPE
            0xC00D36C4 | 0xC00D5212 | 0xC00D36B4 => {
                " (Windows has no decoder for this file; on Windows N editions install the Media Feature Pack)"
            }
            _ => "",
        };
        format!("{name} failed: {} [0x{code:08X}]{hint}", e.message())
    }
}

pub fn decode(path: &Path) -> Result<Decoded, String> {
    let (raw, rate, ch) = unsafe { read_all(path) }?;
    let mut samples = Vec::with_capacity(raw.len());
    push_stereo(&mut samples, &raw, ch as usize);
    Ok(Decoded { channels: if ch == 1 { 1 } else { 2 }, sample_rate: rate, samples })
}

unsafe fn read_all(path: &Path) -> Result<(Vec<i16>, u32, u32), String> {
    // Ignore the result: COM may already be initialised on this thread.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(step("MFStartup"))?;
    let result = read_stream(path);
    let _ = MFShutdown();
    result
}

unsafe fn read_stream(path: &Path) -> Result<(Vec<i16>, u32, u32), String> {
    let wide = wide_path(path);
    let reader = MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), None)
        .map_err(step("Opening the file"))?;
    let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

    // Ask for uncompressed PCM; the reader inserts the right decoder.
    let wanted = MFCreateMediaType().map_err(step("MFCreateMediaType"))?;
    wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(step("SetGUID(major type)"))?;
    wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(step("SetGUID(subtype)"))?;
    // Same request the first working build made: 16-bit PCM. Whatever the decoder really
    // produces is read back below and converted, so this is a preference, not a requirement.
    wanted
        .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
        .map_err(step("SetUINT32(bits per sample)"))?;
    reader
        .SetCurrentMediaType(stream, None, &wanted)
        .map_err(step("Selecting the PCM decoder"))?;

    let actual = reader.GetCurrentMediaType(stream).map_err(step("GetCurrentMediaType"))?;
    let rate = actual.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).map_err(step("Reading sample rate"))?;
    let channels = actual.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).map_err(step("Reading channel count"))?;
    let bits = actual.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(16);
    if rate == 0 || channels == 0 {
        return Err("Windows decoder reported an empty audio format".into());
    }

    let mut out: Vec<i16> = Vec::new();
    loop {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        reader
            .ReadSample(
                stream,
                0,
                None,
                Some(&mut flags as *mut u32),
                None,
                Some(&mut sample as *mut Option<IMFSample>),
            )
            .map_err(step("Reading audio"))?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        if flags & (MF_SOURCE_READERF_ERROR.0 as u32) != 0 {
            return Err("Windows decoder reported a read error".into());
        }
        if let Some(sample) = sample {
            let buffer = sample.ConvertToContiguousBuffer().map_err(step("ConvertToContiguousBuffer"))?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut data as *mut *mut u8, None, Some(&mut len as *mut u32))
                .map_err(step("Locking audio buffer"))?;
            let converted = if data.is_null() {
                Ok(())
            } else {
                pcm_bytes_to_i16(std::slice::from_raw_parts(data, len as usize), bits, &mut out)
            };
            let _ = buffer.Unlock();
            converted?;
        }
        // Stop reading absurdly long files early; decode::check_length reports the error.
        if out.len() as u64 > rate as u64 * channels as u64 * (crate::decode::MAX_MINUTES * 60 + 5) {
            break;
        }
    }
    Ok((out, rate, channels))
}
