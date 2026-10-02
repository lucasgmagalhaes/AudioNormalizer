//! Safe wrappers over the C bridge in `native/avbridge.c`, which talks to
//! libavformat / libavcodec / libswresample directly (no ffmpeg process).

use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Once};

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
struct AvbAudioCache {
    _private: [u8; 0],
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
    fn avb_cache_free(cache: *mut AvbAudioCache);
    #[cfg(test)]
    fn avb_cache_bytes(cache: *const AvbAudioCache) -> i64;
    fn avb_decoder_open(path: *const c_char, record_limit: i64, err: *mut c_char) -> *mut AvbDecoder;
    fn avb_decoder_open_cache(cache: *const AvbAudioCache, err: *mut c_char) -> *mut AvbDecoder;
    fn avb_decoder_take_cache(dec: *mut AvbDecoder) -> *mut AvbAudioCache;
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
    fn avb_remuxer_monitor_format(mux: *const AvbRemuxer, sample_rate: *mut c_int, channels: *mut c_int) -> c_int;
    fn avb_remuxer_read_monitor(mux: *mut AvbRemuxer, out: *mut f32, max_frames: c_int) -> c_int;
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

/// The first audio track's compressed packets held in memory, so it can be
/// decoded again without reading the file.
pub struct AudioCache {
    ptr: NonNull<AvbAudioCache>,
}

impl AudioCache {
    #[cfg(test)]
    pub fn bytes(&self) -> u64 {
        // SAFETY: live handle.
        unsafe { avb_cache_bytes(self.ptr.as_ptr()) }.max(0) as u64
    }
}

// SAFETY: a taken cache is never mutated again; decoders only take new
// references to its packets (atomic refcounts).
unsafe impl Send for AudioCache {}
unsafe impl Sync for AudioCache {}

impl Drop for AudioCache {
    fn drop(&mut self) {
        // SAFETY: owned handle, freed once; decoders reading it hold an Arc.
        unsafe { avb_cache_free(self.ptr.as_ptr()) }
    }
}

/// Where decoders read the audio from.
#[derive(Clone)]
pub enum AudioSource<'a> {
    File(&'a Path),
    Cache(Arc<AudioCache>),
}

impl AudioSource<'_> {
    pub fn open(&self, info: &MediaInfo) -> Result<PcmDecoder> {
        match self {
            AudioSource::File(path) => PcmDecoder::open(path, info, 0),
            AudioSource::Cache(cache) => PcmDecoder::open_cache(cache.clone(), info),
        }
    }
}

/// Decodes the first audio track to interleaved f32 PCM.
pub struct PcmDecoder {
    ptr: NonNull<AvbDecoder>,
    channels: usize,
    /// Keeps the cache alive while decoding from it.
    _source: Option<Arc<AudioCache>>,
}

impl PcmDecoder {
    const BLOCK_FRAMES: usize = 8192;

    /// With `record_limit` > 0 the compressed packets are recorded too (up
    /// to that many bytes); see [`PcmDecoder::take_cache`].
    pub fn open(path: &Path, info: &MediaInfo, record_limit: u64) -> Result<Self> {
        init();
        let path = c_path(path)?;
        let mut err = ErrBuf::new();
        // SAFETY: valid path and error buffer; null is handled below.
        let raw = unsafe { avb_decoder_open(path.as_ptr(), record_limit.min(i64::MAX as u64) as i64, err.ptr()) };
        let ptr = NonNull::new(raw).ok_or_else(|| err.error())?;
        Ok(Self { ptr, channels: info.audio.channels as usize, _source: None })
    }

    pub fn open_cache(cache: Arc<AudioCache>, info: &MediaInfo) -> Result<Self> {
        init();
        let mut err = ErrBuf::new();
        // SAFETY: the cache is kept alive by `_source` for the decoder's life.
        let raw = unsafe { avb_decoder_open_cache(cache.ptr.as_ptr(), err.ptr()) };
        let ptr = NonNull::new(raw).ok_or_else(|| err.error())?;
        Ok(Self { ptr, channels: info.audio.channels as usize, _source: Some(cache) })
    }

    /// The recording, once the whole track was read without exceeding the
    /// limit.
    pub fn take_cache(&mut self) -> Option<AudioCache> {
        // SAFETY: live handle; ownership of the returned cache moves to us.
        NonNull::new(unsafe { avb_decoder_take_cache(self.ptr.as_ptr()) }).map(|ptr| AudioCache { ptr })
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

// SAFETY: the libav contexts behind the handle are only ever used by one
// thread at a time (the handle is moved, never shared).
unsafe impl Send for PcmDecoder {}

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
    monitor_channels: usize,
}

/// Sample rate and channel count of the monitor PCM.
#[derive(Debug, Clone, Copy)]
pub struct MonitorFormat {
    pub sample_rate: u32,
    pub channels: u32,
}

// SAFETY: see PcmDecoder.
unsafe impl Send for Remuxer {}

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
        let mut remuxer = Self { ptr, channels: info.audio.channels as usize, monitor_channels: 0 };
        remuxer.monitor_channels = remuxer.monitor_format().map_or(0, |f| f.channels as usize);
        Ok(remuxer)
    }

    /// Format of the encoded audio decoded back, when a decoder exists.
    pub fn monitor_format(&self) -> Option<MonitorFormat> {
        let (mut rate, mut channels) = (0, 0);
        // SAFETY: live handle, valid out-pointers.
        let ok = unsafe { avb_remuxer_monitor_format(self.ptr.as_ptr(), &mut rate, &mut channels) } == 0;
        (ok && rate > 0 && channels > 0).then_some(MonitorFormat { sample_rate: rate as u32, channels: channels as u32 })
    }

    /// Replace `out` with the next block of decoded-back audio produced so
    /// far; returns `false` when nothing is pending.
    pub fn read_monitor(&mut self, out: &mut Vec<f32>) -> bool {
        const BLOCK_FRAMES: usize = 8192;
        if self.monitor_channels == 0 {
            out.clear();
            return false;
        }
        out.resize(BLOCK_FRAMES * self.monitor_channels, 0.0);
        // SAFETY: `out` holds BLOCK_FRAMES frames of the monitor channel count.
        let n = unsafe { avb_remuxer_read_monitor(self.ptr.as_ptr(), out.as_mut_ptr(), BLOCK_FRAMES as c_int) };
        out.truncate(n.max(0) as usize * self.monitor_channels);
        n > 0
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

    /// Flush and write the trailer. Monitor audio produced by the flush can
    /// still be read afterwards; the file is closed when the handle drops.
    pub fn finish(&mut self) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_extensions_and_rejects_nul_paths() {
        assert_eq!(extension(Path::new("movie.MP4")), "mp4");
        assert_eq!(extension(Path::new("no-extension")), "");
        assert!(c_path(Path::new("invalid\0path")).is_err());
    }
}
