//! Optional audio clean-up that runs before leveling, gain and limiting.
//!
//! Every stage here keeps the audio exactly as long as it was and never
//! looks at the picture: the number of frames that goes in is the number that
//! comes out, so the audio stays aligned with the video.
//!
//! * [`Declipper`] rebuilds the flat tops of clipped peaks with a smooth curve.
//! * [`HighPass`] removes DC offset and inaudible rumble below 20 Hz.

use serde::{Deserialize, Serialize};

/// Which clean-up stages are on. Everything is off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CleanupSettings {
    /// Remove DC offset and rumble below 20 Hz.
    pub highpass: bool,
    /// Rebuild clipped peaks.
    pub declip: bool,
}

impl CleanupSettings {
    pub fn any(self) -> bool {
        self.highpass || self.declip
    }
}

/// Corner of the high-pass filter, well under anything audible.
const HIGHPASS_HZ: f64 = 20.0;
/// A sample at or above this magnitude counts as sitting on the clip level.
const CLIP_LEVEL: f32 = 0.99;
/// Shortest flat top that is treated as clipping.
const MIN_RUN: usize = 3;
/// Longest flat top that is repaired (about 1 ms at 48 kHz); longer ones are
/// real signal or damage that interpolation cannot invent back.
const MAX_RUN: usize = 48;
/// Samples needed before a run (to read the slope going in).
const CONTEXT: usize = 2;
/// A rebuilt peak never rises above this magnitude; the limiter handles the rest.
const MAX_REBUILT: f32 = 2.0;

pub struct Cleanup {
    channels: usize,
    declip: Option<Declipper>,
    highpass: Option<HighPass>,
}

impl Cleanup {
    pub fn new(settings: CleanupSettings, channels: usize, sample_rate: u32) -> Self {
        Self {
            channels,
            declip: settings.declip.then(|| Declipper::new(channels)),
            highpass: settings.highpass.then(|| HighPass::new(channels, sample_rate)),
        }
    }

    /// Appends the cleaned audio for interleaved `block` to `out`. Stages may
    /// hold a few frames back; [`flush`](Self::flush) returns them, so over a
    /// whole stream the frame count is unchanged.
    pub fn process(&mut self, block: &[f32], out: &mut Vec<f32>) {
        let start = out.len();
        match self.declip.as_mut() {
            Some(declip) => declip.process(block, out),
            None => out.extend_from_slice(block),
        }
        self.filter(out, start);
    }

    /// Appends whatever is still held back.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let start = out.len();
        if let Some(declip) = self.declip.as_mut() {
            declip.flush(out);
        }
        self.filter(out, start);
    }

    /// Samples rebuilt so far (None when declipping is off).
    pub fn declipped_samples(&self) -> Option<u64> {
        self.declip.as_ref().map(|d| d.repaired)
    }

    fn filter(&mut self, out: &mut [f32], start: usize) {
        if let Some(highpass) = self.highpass.as_mut() {
            highpass.process(&mut out[start..], self.channels);
        }
    }
}

/// Second-order Butterworth high-pass, one filter per channel.
struct HighPass {
    b: [f64; 3],
    a: [f64; 2],
    /// Transposed direct form II state, two values per channel.
    state: Vec<[f64; 2]>,
}

impl HighPass {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let w0 = std::f64::consts::TAU * HIGHPASS_HZ / f64::from(sample_rate);
        let alpha = w0.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let cos = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            state: vec![[0.0; 2]; channels],
        }
    }

    fn process(&mut self, block: &mut [f32], channels: usize) {
        for frame in block.chunks_exact_mut(channels) {
            for (sample, z) in frame.iter_mut().zip(self.state.iter_mut()) {
                let x = f64::from(*sample);
                let y = self.b[0] * x + z[0];
                z[0] = self.b[1] * x - self.a[0] * y + z[1];
                z[1] = self.b[2] * x - self.a[1] * y;
                *sample = y as f32;
            }
        }
    }
}

/// Rebuilds runs of clipped samples with a cubic Hermite curve through the
/// samples on either side, matching their slopes. Only a few trailing frames
/// are held back (when a run touches the end of a block) so runs that span two
/// blocks are repaired whole.
struct Declipper {
    channels: usize,
    /// Interleaved frames not yet written out.
    held: Vec<f32>,
    /// Per channel: frame (in `held`) where scanning resumes.
    resume: Vec<usize>,
    repaired: u64,
}

impl Declipper {
    fn new(channels: usize) -> Self {
        Self { channels, held: Vec::new(), resume: vec![0; channels], repaired: 0 }
    }

    fn process(&mut self, block: &[f32], out: &mut Vec<f32>) {
        self.held.extend_from_slice(block);
        self.settle(false, out);
    }

    fn flush(&mut self, out: &mut Vec<f32>) {
        self.settle(true, out);
    }

    fn settle(&mut self, last: bool, out: &mut Vec<f32>) {
        let frames = self.held.len() / self.channels;
        // The last frames stay behind as the slope context of a run that may start next.
        let mut cut = if last { frames } else { frames.saturating_sub(CONTEXT) };
        let mut resume = vec![frames; self.channels];
        for (ch, slot) in resume.iter_mut().enumerate() {
            if let Some(start) = self.repair_channel(ch, frames, last) {
                *slot = start;
                cut = cut.min(start.saturating_sub(CONTEXT));
            }
        }
        out.extend_from_slice(&self.held[..cut * self.channels]);
        self.held.drain(..cut * self.channels);
        for (slot, start) in self.resume.iter_mut().zip(resume) {
            *slot = start - cut;
        }
        if last {
            out.append(&mut self.held);
            self.resume.fill(0);
        }
    }

    /// Repairs the finished runs of one channel. Returns where an unfinished
    /// run (one that touches the end of the data) begins, so the caller can
    /// wait for more audio; None when everything was settled.
    fn repair_channel(&mut self, ch: usize, frames: usize, last: bool) -> Option<usize> {
        let stride = self.channels;
        let at = |i: usize| i * stride + ch;
        let mut i = self.resume[ch].min(frames);
        while i < frames {
            let first = self.held[at(i)];
            if first.abs() < CLIP_LEVEL {
                i += 1;
                continue;
            }
            let start = i;
            let positive = first > 0.0;
            while i < frames && self.held[at(i)].abs() >= CLIP_LEVEL && (self.held[at(i)] > 0.0) == positive {
                i += 1;
            }
            let end = i;
            let len = end - start;
            if len > MAX_RUN {
                continue;
            }
            // Two samples after the run are needed to read the slope coming out.
            if end + 1 >= frames {
                return if last { None } else { Some(start) };
            }
            if len >= MIN_RUN && start >= CONTEXT {
                self.rebuild(ch, start, end, positive);
            }
        }
        None
    }

    fn rebuild(&mut self, ch: usize, start: usize, end: usize, positive: bool) {
        let stride = self.channels;
        let at = |i: usize| i * stride + ch;
        let (before, edge) = (self.held[at(start - 1)], self.held[at(start - 2)]);
        let (after, next) = (self.held[at(end)], self.held[at(end + 1)]);
        let span = (end - start + 1) as f32;
        let (slope_in, slope_out) = ((before - edge) * span, (next - after) * span);
        let sign = if positive { 1.0 } else { -1.0 };
        for i in start..end {
            let t = (i - start + 1) as f32 / span;
            let (t2, t3) = (t * t, t * t * t);
            let rebuilt = (2.0 * t3 - 3.0 * t2 + 1.0) * before
                + (t3 - 2.0 * t2 + t) * slope_in
                + (-2.0 * t3 + 3.0 * t2) * after
                + (t3 - t2) * slope_out;
            // Clipping only ever removes the top of a peak: never lower it.
            let magnitude = (rebuilt * sign).clamp(self.held[at(i)].abs(), MAX_REBUILT);
            self.held[at(i)] = magnitude * sign;
        }
        self.repaired += (end - start) as u64;
    }
}

#[cfg(test)]
mod tests;
