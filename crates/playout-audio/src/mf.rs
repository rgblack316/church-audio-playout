//! Windows Media Foundation fallback decoder (handles WMA and other Windows codecs).

use crate::decode::{push_stereo, Decoded};
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

pub fn decode(path: &Path) -> Result<Decoded, String> {
    let (raw, rate, ch) = unsafe { read_all(path) }.map_err(|e| e.to_string())?;
    let mut samples = Vec::with_capacity(raw.len());
    push_stereo(&mut samples, &raw, ch as usize);
    Ok(Decoded { channels: if ch == 1 { 1 } else { 2 }, sample_rate: rate, samples })
}

unsafe fn read_all(path: &Path) -> windows::core::Result<(Vec<i16>, u32, u32)> {
    // Ignore the result: COM may already be initialised on this thread.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    let result = read_stream(path);
    let _ = MFShutdown();
    result
}

unsafe fn read_stream(path: &Path) -> windows::core::Result<(Vec<i16>, u32, u32)> {
    let wide = wide_path(path);
    let reader = MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), None)?;
    let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

    // Ask for 16-bit PCM; the reader inserts the right decoder.
    let wanted = MFCreateMediaType()?;
    wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    wanted.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    reader.SetCurrentMediaType(stream, None, &wanted)?;

    let actual = reader.GetCurrentMediaType(stream)?;
    let rate = actual.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
    let channels = actual.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?;

    let mut out: Vec<i16> = Vec::new();
    loop {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        reader.ReadSample(
            stream,
            0,
            None,
            Some(&mut flags as *mut u32),
            None,
            Some(&mut sample as *mut Option<IMFSample>),
        )?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        if let Some(sample) = sample {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut data as *mut *mut u8, None, Some(&mut len as *mut u32))?;
            if !data.is_null() {
                let bytes = std::slice::from_raw_parts(data, len as usize);
                out.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
            }
            buffer.Unlock()?;
        }
        // Stop reading absurdly long files early; decode::check_length reports the error.
        if out.len() as u64 > rate as u64 * channels as u64 * (crate::decode::MAX_MINUTES * 60 + 5) {
            break;
        }
    }
    Ok((out, rate, channels))
}
