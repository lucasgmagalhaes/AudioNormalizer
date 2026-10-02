//! Look-ahead brickwall true-peak limiter.
//!
//! Detection: each frame's peak is its largest sample *or* inter-sample value,
//! estimated by 4x oversampling (windowed-sinc interpolation, as in
//! ITU-R BS.1770 annex 2), so the ceiling holds for the reconstructed
//! waveform and not just the samples.
//!
//! Gain computer: per-frame required gain -> sliding-window minimum over the
//! look-ahead -> release smoothing (which can only lower the gain) -> box
//! filter of the same length. Because the box filter averages values that are
//! all at or below the gain required at the peak, the peak is guaranteed to
//! land under the ceiling while the gain ramps smoothly over the look-ahead.
//!
//! Work is done per block: detection runs over contiguous per-channel
//! history (so the interpolation filter vectorizes), and the gain and delay
//! lines are fixed-size rings.

use std::collections::VecDeque;

const LOOKAHEAD_SECONDS: f64 = 0.005;
const RELEASE_SECONDS: f64 = 0.080;

/// Interpolation filter length (history frames) for true-peak detection.
const TP_TAPS: usize = 12;
/// Frames between a sample entering and its true peak being known.
const TP_DELAY: usize = TP_TAPS / 2;
/// History index, inside a TP_TAPS window, of the frame being detected.
const TP_CENTER: usize = TP_TAPS - 1 - TP_DELAY;
/// Oversampling factor; phases 1..OVERSAMPLE are interpolated.
const OVERSAMPLE: usize = 4;

pub struct Limiter {
    channels: usize,
    ceiling: f32,
    lookahead: usize,
    release_coef: f64,
    /// Interpolation coefficients for the in-between phases.
    tp_coeffs: [[f32; TP_TAPS]; OVERSAMPLE - 1],
    /// Per channel: the last TP_TAPS - 1 samples, then the current block.
    history: Vec<Vec<f32>>,
    /// Per-frame peak of the current block.
    peaks: Vec<f32>,
    /// Scratch for one interpolation phase over the block.
    interpolated: Vec<f32>,
    /// Detected frames still to drop: the zero history they came from.
    prefill: usize,
    frame_index: u64,
    /// (frame index, required gain), gains strictly increasing front to back.
    minimum: VecDeque<(u64, f64)>,
    release_state: f64,
    /// Box filter ring over the look-ahead.
    window: Vec<f64>,
    window_pos: usize,
    window_sum: f64,
    /// Delay line ring of `lookahead` interleaved frames.
    delay: Vec<f32>,
    delay_head: usize,
    delay_len: usize,
    /// Lowest gain applied so far (linear).
    pub min_gain: f64,
}

impl Limiter {
    /// `ceiling` is the linear true-peak ceiling.
    pub fn new(channels: usize, sample_rate: u32, ceiling: f32) -> Self {
        let lookahead = ((sample_rate as f64 * LOOKAHEAD_SECONDS).round() as usize).max(1);
        Self {
            channels,
            ceiling,
            lookahead,
            release_coef: (-1.0 / (RELEASE_SECONDS * sample_rate as f64)).exp(),
            tp_coeffs: interpolation_coeffs(),
            history: vec![vec![0.0; TP_TAPS - 1]; channels],
            peaks: Vec::new(),
            interpolated: Vec::new(),
            prefill: TP_DELAY,
            frame_index: 0,
            minimum: VecDeque::with_capacity(lookahead + 1),
            release_state: 1.0,
            window: vec![1.0; lookahead],
            window_pos: 0,
            window_sum: lookahead as f64,
            delay: vec![0.0; lookahead * channels],
            delay_head: 0,
            delay_len: 0,
            min_gain: 1.0,
        }
    }

    /// Process interleaved frames. `output` is replaced with however many
    /// frames leave the delay line (the first calls return fewer frames).
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        output.clear();
        let frames = input.len() / self.channels;
        if frames == 0 {
            return;
        }
        output.reserve(frames * self.channels);

        for (ch, history) in self.history.iter_mut().enumerate() {
            history.truncate(TP_TAPS - 1);
            history.extend(input.iter().skip(ch).step_by(self.channels).take(frames));
        }
        self.detect_peaks(frames);

        for frame in 0..frames {
            if self.prefill > 0 {
                self.prefill -= 1;
                continue;
            }
            let peak = self.peaks[frame];
            self.push_frame(frame, peak, output);
        }

        for history in &mut self.history {
            history.drain(..frames);
        }
    }

    /// Drain the delay line so the output length matches the input length.
    pub fn flush(&mut self, output: &mut Vec<f32>) {
        let silence = vec![0.0; (TP_DELAY + self.lookahead - 1) * self.channels];
        self.process(&silence, output);
    }

    /// Fills `peaks[j]` with the true peak (across channels) of the frame
    /// detected when block frame `j` arrives, i.e. history index `j + TP_CENTER`.
    /// The filter is applied tap by tap across the whole block so each inner
    /// loop is a plain multiply-add over contiguous slices (vectorized).
    fn detect_peaks(&mut self, frames: usize) {
        self.peaks.clear();
        self.peaks.resize(frames, 0.0);
        for history in &self.history {
            let samples = &history[TP_CENTER..TP_CENTER + frames];
            for (peak, x) in self.peaks.iter_mut().zip(samples) {
                *peak = peak.max(x.abs());
            }
            for phase in &self.tp_coeffs {
                self.interpolated.clear();
                self.interpolated.resize(frames, 0.0);
                for (tap, &c) in phase.iter().enumerate() {
                    for (acc, x) in self.interpolated.iter_mut().zip(&history[tap..tap + frames]) {
                        *acc += x * c;
                    }
                }
                for (peak, acc) in self.peaks.iter_mut().zip(&self.interpolated) {
                    *peak = peak.max(acc.abs());
                }
            }
        }
    }

    /// Runs the gain computer for the frame at history index
    /// `frame + TP_CENTER` and emits the frame leaving the delay line.
    fn push_frame(&mut self, frame: usize, peak: f32, output: &mut Vec<f32>) {
        let required = if peak > self.ceiling {
            (self.ceiling / peak) as f64
        } else {
            1.0
        };

        // Sliding minimum over the last `lookahead` frames.
        let index = self.frame_index;
        self.frame_index += 1;
        while self.minimum.back().is_some_and(|&(_, g)| g >= required) {
            self.minimum.pop_back();
        }
        self.minimum.push_back((index, required));
        while self
            .minimum
            .front()
            .is_some_and(|&(i, _)| i + self.lookahead as u64 <= index)
        {
            self.minimum.pop_front();
        }
        let target = self.minimum.front().map_or(1.0, |&(_, g)| g);

        // Instant attack, exponential release; never rises above `target`.
        self.release_state = if target < self.release_state {
            target
        } else {
            target + (self.release_state - target) * self.release_coef
        };

        // Box filter over the look-ahead window.
        self.window_sum += self.release_state - self.window[self.window_pos];
        self.window[self.window_pos] = self.release_state;
        self.window_pos += 1;
        if self.window_pos == self.lookahead {
            self.window_pos = 0;
        }
        let gain = (self.window_sum / self.lookahead as f64).min(1.0);

        // Delay line: the gain lines up with the frame that entered
        // lookahead - 1 frames ago.
        let channels = self.channels;
        let mut slot = self.delay_head + self.delay_len;
        if slot >= self.lookahead {
            slot -= self.lookahead;
        }
        let slot = slot * channels;
        for (ch, history) in self.history.iter().enumerate() {
            self.delay[slot + ch] = history[frame + TP_CENTER];
        }
        self.delay_len += 1;
        if self.delay_len == self.lookahead {
            self.min_gain = self.min_gain.min(gain);
            let gain = gain as f32;
            let ceiling = self.ceiling;
            let oldest = self.delay_head * channels;
            // Clamp guards against floating-point rounding in the running sum.
            output.extend(
                self.delay[oldest..oldest + channels]
                    .iter()
                    .map(|s| (s * gain).clamp(-ceiling, ceiling)),
            );
            self.delay_head += 1;
            if self.delay_head == self.lookahead {
                self.delay_head = 0;
            }
            self.delay_len -= 1;
        }
    }
}

/// Hann-windowed sinc taps for the fractional positions 1/4, 2/4 and 3/4
/// between history frame `TP_CENTER` and the next one.
fn interpolation_coeffs() -> [[f32; TP_TAPS]; OVERSAMPLE - 1] {
    let center = TP_CENTER as f64;
    let half_span = TP_DELAY as f64;
    let mut coeffs = [[0.0f32; TP_TAPS]; OVERSAMPLE - 1];
    for (p, phase) in coeffs.iter_mut().enumerate() {
        let position = center + (p + 1) as f64 / OVERSAMPLE as f64;
        let mut sum = 0.0;
        for (tap, c) in phase.iter_mut().enumerate() {
            let d = position - tap as f64;
            let sinc = if d.abs() < 1e-12 {
                1.0
            } else {
                (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d)
            };
            let window = if d.abs() < half_span {
                0.5 * (1.0 + (std::f64::consts::PI * d / half_span).cos())
            } else {
                0.0
            };
            *c = (sinc * window) as f32;
            sum += sinc * window;
        }
        // Unity gain at DC.
        for c in phase.iter_mut() {
            *c /= sum as f32;
        }
    }
    coeffs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(limiter: &mut Limiter, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        let mut all = Vec::new();
        for chunk in input.chunks(1000) {
            limiter.process(chunk, &mut out);
            all.extend_from_slice(&out);
        }
        limiter.flush(&mut out);
        all.extend_from_slice(&out);
        all
    }

    /// Peak of the 8x oversampled signal (independent reference).
    fn reference_true_peak(signal: &[f32]) -> f32 {
        let mut peak = 0.0f32;
        for i in 8..signal.len().saturating_sub(8) {
            for step in 0..8 {
                let t = i as f64 + step as f64 / 8.0;
                let mut acc = 0.0;
                for (j, &x) in signal.iter().enumerate().take(i + 9).skip(i - 8) {
                    let d = t - j as f64;
                    let sinc = if d.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d) };
                    acc += x as f64 * sinc;
                }
                peak = peak.max(acc.abs() as f32);
            }
        }
        peak
    }

    #[test]
    fn preserves_length_and_alignment() {
        let mut limiter = Limiter::new(2, 48_000, 0.9);
        let input: Vec<f32> = (0..20_000).map(|i| ((i as f32) * 0.001).sin() * 0.5).collect();
        let output = run(&mut limiter, &input);
        assert_eq!(output.len(), input.len());
        // Below the ceiling the limiter must be fully transparent.
        for (a, b) in input.iter().zip(&output) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn empty_input_produces_no_output() {
        let mut limiter = Limiter::new(1, 48_000, 0.9);
        let mut output = vec![1.0];
        limiter.process(&[], &mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn block_size_does_not_change_the_result() {
        let input: Vec<f32> = (0..30_000).map(|i| ((i as f32) * 0.07).sin() * (1.0 + (i % 5000) as f32 / 2000.0)).collect();
        let mut whole = Limiter::new(2, 48_000, 0.7);
        let mut expected = Vec::new();
        whole.process(&input, &mut expected);
        let mut tail = Vec::new();
        whole.flush(&mut tail);
        expected.extend(tail);

        let mut chunked = Limiter::new(2, 48_000, 0.7);
        assert_eq!(run(&mut chunked, &input), expected);
    }

    #[test]
    fn never_exceeds_ceiling() {
        let ceiling = 0.5;
        let mut limiter = Limiter::new(1, 48_000, ceiling);
        let mut input: Vec<f32> = (0..48_000).map(|i| ((i as f32) * 0.05).sin() * 0.3).collect();
        for i in (1000..48_000).step_by(3777) {
            input[i] = 1.8; // isolated spikes
        }
        for s in input.iter_mut().skip(20_000).take(2000) {
            *s *= 3.0; // loud burst
        }
        let output = run(&mut limiter, &input);
        assert_eq!(output.len(), input.len());
        assert!(output.iter().all(|s| s.abs() <= ceiling + 1e-6));
        assert!(limiter.min_gain < 0.3);
    }

    #[test]
    fn catches_inter_sample_peaks() {
        // A tone at fs/4 sampled 45 degrees off its crest: every sample is
        // at 0.707 of the real peak, which lies between samples.
        let ceiling = 0.5;
        let input: Vec<f32> = (0..9600)
            .map(|i| (std::f32::consts::FRAC_PI_2 * i as f32 + std::f32::consts::FRAC_PI_4).sin() * 0.69)
            .collect();
        assert!(input.iter().all(|s| s.abs() < ceiling)); // samples alone look safe
        assert!(reference_true_peak(&input) > 0.65);

        let mut limiter = Limiter::new(1, 48_000, ceiling);
        let output = run(&mut limiter, &input);
        let tp = reference_true_peak(&output[2000..]);
        assert!(tp <= ceiling * 1.02, "true peak {tp} over ceiling {ceiling}");
    }
}
