//! Safe wrappers over the C bridge in `native/avbridge.c`, which talks to
//! libavformat / libavcodec / libswresample directly (no ffmpeg process).

use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Once;

const ERR_LEN: usize = 512;

#[repr(C)]
struct AvbMediaInfo {
    duration: f64,
    has_video: c_int,
    sample_rate: c_int,
    channels: c_int,
    bit_rate: i64,
    codec: [c_char; 32],
}

#[repr(C)]
struct AvbDecoder {
    _private: [u8; 0],
}

#[repr(C)]
struct AvbRemuxer {
    _private: [u8; 0],
}

extern "C" {
    fn avb_init();
    fn avb_probe(path: *const c_char, info: *mut AvbMediaInfo, err: *mut c_char) -> c_int;
    fn avb_decoder_open(path: *const c_char, err: *mut c_char) -> *mut AvbDecoder;
    fn avb_decoder_read(dec: *mut AvbDecoder, out: *mut f32, max_frames: c_int, err: *mut c_char) -> c_int;
    fn avb_decoder_close(dec: *mut AvbDecoder);
    fn avb_remuxer_open(
        input: *const c_char,
        output: *const c_char,
        encoder_options: *const c_char,
        err: *mut c_char,
    ) -> *mut AvbRemuxer;
    fn avb_remuxer_encoder(mux: *const AvbRemuxer) -> *const c_char;
    fn avb_remuxer_write(mux: *mut AvbRemuxer, samples: *const f32, frames: c_int, err: *mut c_char) -> c_int;
    fn avb_remuxer_finish(mux: *mut AvbRemuxer, err: *mut c_char) -> c_int;
    fn avb_remuxer_close(mux: *mut AvbRemuxer);
}

fn init() {
    static INIT: Once = Once::new();
    // SAFETY: only sets the libav log level.
    INIT.call_once(|| unsafe { avb_init() });
}

fn c_path(path: &Path) -> Result<CString> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("caminho com caracteres inválidos: {}", path.display()))?;
    CString::new(s).map_err(|_| anyhow!("caminho inválido: {s}"))
}

struct ErrBuf([c_char; ERR_LEN]);

impl ErrBuf {
    fn new() -> Self {
        Self([0; ERR_LEN])
    }

    fn ptr(&mut self) -> *mut c_char {
        self.0.as_mut_ptr()
    }

    fn error(&self) -> anyhow::Error {
        // SAFETY: the bridge always NUL-terminates (snprintf) and the buffer
        // starts zeroed, so there is a terminator within ERR_LEN.
        let msg = unsafe { CStr::from_ptr(self.0.as_ptr()) }.to_string_lossy();
        if msg.is_empty() {
            anyhow!("erro desconhecido na biblioteca de mídia")
        } else {
            anyhow!("{msg}")
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
}

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration: f64,
    pub has_video: bool,
    pub audio: AudioInfo,
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    init();
    let path = c_path(path)?;
    let mut err = ErrBuf::new();
    let mut raw = AvbMediaInfo {
        duration: 0.0,
        has_video: 0,
        sample_rate: 0,
        channels: 0,
        bit_rate: 0,
        codec: [0; 32],
    };
    // SAFETY: valid NUL-terminated path, writable struct and error buffer.
    if unsafe { avb_probe(path.as_ptr(), &mut raw, err.ptr()) } < 0 {
        return Err(err.error());
    }
    // SAFETY: the bridge NUL-terminates `codec` (snprintf into 32 bytes).
    let codec = unsafe { CStr::from_ptr(raw.codec.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(MediaInfo {
        duration: raw.duration.max(0.0),
        has_video: raw.has_video != 0,
        audio: AudioInfo {
            codec,
            sample_rate: raw.sample_rate as u32,
            channels: raw.channels as u32,
        },
    })
}

/// Decodes the first audio track to interleaved f32 PCM.
pub struct PcmDecoder {
    ptr: NonNull<AvbDecoder>,
    channels: usize,
}

impl PcmDecoder {
    const BLOCK_FRAMES: usize = 8192;

    pub fn open(path: &Path, info: &MediaInfo) -> Result<Self> {
        init();
        let path = c_path(path)?;
        let mut err = ErrBuf::new();
        // SAFETY: valid path and error buffer; null is handled below.
        let raw = unsafe { avb_decoder_open(path.as_ptr(), err.ptr()) };
        let ptr = NonNull::new(raw).ok_or_else(|| err.error())?;
        Ok(Self { ptr, channels: info.audio.channels as usize })
    }

    /// Replace `out` with the next block of frames; `false` at end of stream.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        out.resize(Self::BLOCK_FRAMES * self.channels, 0.0);
        let mut err = ErrBuf::new();
        // SAFETY: `out` holds BLOCK_FRAMES frames of `channels` samples, which
        // matches the channel count the decoder was opened with.
        let n = unsafe {
            avb_decoder_read(self.ptr.as_ptr(), out.as_mut_ptr(), Self::BLOCK_FRAMES as c_int, err.ptr())
        };
        if n < 0 {
            out.clear();
            return Err(err.error());
        }
        out.truncate(n as usize * self.channels);
        Ok(n > 0)
    }
}

impl Drop for PcmDecoder {
    fn drop(&mut self) {
        // SAFETY: pointer came from avb_decoder_open and is closed once.
        unsafe { avb_decoder_close(self.ptr.as_ptr()) }
    }
}

/// Writes `output`: every stream of `input` stream-copied, except the first
/// audio track which is re-encoded from the PCM handed to `write`.
pub struct Remuxer {
    ptr: NonNull<AvbRemuxer>,
    channels: usize,
}

impl Remuxer {
    /// `encoder_options`: private encoder options as "key=value:key=value".
    pub fn open(input: &Path, output: &Path, info: &MediaInfo, encoder_options: &str) -> Result<Self> {
        init();
        let input = c_path(input)?;
        let output = c_path(output)?;
        let options = CString::new(encoder_options).map_err(|_| anyhow!("opções inválidas"))?;
        let mut err = ErrBuf::new();
        // SAFETY: valid NUL-terminated strings and error buffer; null is handled below.
        let raw = unsafe { avb_remuxer_open(input.as_ptr(), output.as_ptr(), options.as_ptr(), err.ptr()) };
        let ptr = NonNull::new(raw).ok_or_else(|| err.error())?;
        Ok(Self { ptr, channels: info.audio.channels as usize })
    }

    /// Name of the audio encoder the bridge picked (e.g. "aac").
    pub fn encoder(&self) -> String {
        // SAFETY: returns a static codec name (or "") owned by libavcodec.
        unsafe { CStr::from_ptr(avb_remuxer_encoder(self.ptr.as_ptr())) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        if !samples.len().is_multiple_of(self.channels) {
            bail!("bloco de áudio com número de amostras inválido");
        }
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return Ok(());
        }
        let mut err = ErrBuf::new();
        // SAFETY: `samples` holds exactly `frames` interleaved frames.
        let ret = unsafe {
            avb_remuxer_write(self.ptr.as_ptr(), samples.as_ptr(), frames as c_int, err.ptr())
        };
        if ret < 0 {
            return Err(err.error());
        }
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        let mut err = ErrBuf::new();
        // SAFETY: pointer is live until Drop runs after this call.
        if unsafe { avb_remuxer_finish(self.ptr.as_ptr(), err.ptr()) } < 0 {
            return Err(err.error());
        }
        Ok(())
    }
}

impl Drop for Remuxer {
    fn drop(&mut self) {
        // SAFETY: pointer came from avb_remuxer_open and is closed once.
        unsafe { avb_remuxer_close(self.ptr.as_ptr()) }
    }
}

pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}
