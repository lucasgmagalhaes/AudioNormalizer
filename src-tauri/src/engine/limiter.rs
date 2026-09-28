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

use std::collections::VecDeque;

const LOOKAHEAD_SECONDS: f64 = 0.005;
const RELEASE_SECONDS: f64 = 0.080;

/// Interpolation filter length (history frames) for true-peak detection.
const TP_TAPS: usize = 12;
/// Frames between a sample entering and its true peak being known.
const TP_DELAY: usize = TP_TAPS / 2;
/// Oversampling factor; phases 1..OVERSAMPLE are interpolated.
const OVERSAMPLE: usize = 4;

pub struct Limiter {
    channels: usize,
    ceiling: f32,
    lookahead: usize,
    release_coef: f64,
    /// Interpolation coefficients for the in-between phases.
    tp_coeffs: [[f32; TP_TAPS]; OVERSAMPLE - 1],
    /// Last TP_TAPS interleaved input frames.
    history: VecDeque<f32>,
    frames_in: u64,
    frame_index: u64,
    /// (frame index, required gain), gains strictly increasing front to back.
    minimum: VecDeque<(u64, f64)>,
    release_state: f64,
    window: VecDeque<f64>,
    window_sum: f64,
    /// Delayed interleaved samples waiting for their gain.
    delay: VecDeque<f32>,
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
            history: std::iter::repeat_n(0.0, TP_TAPS * channels).collect(),
            frames_in: 0,
            frame_index: 0,
            minimum: VecDeque::with_capacity(lookahead + 1),
            release_state: 1.0,
            window: std::iter::repeat_n(1.0, lookahead).collect(),
            window_sum: lookahead as f64,
            delay: VecDeque::with_capacity((lookahead + 1) * channels),
            min_gain: 1.0,
        }
    }

    /// Process interleaved frames. `output` is replaced with however many
    /// frames leave the delay line (the first calls return fewer frames).
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        output.clear();
        output.reserve(input.len());
        for frame in input.chunks_exact(self.channels) {
            self.push_frame(frame, output);
        }
    }

    /// Drain the delay line so the output length matches the input length.
    pub fn flush(&mut self, output: &mut Vec<f32>) {
        output.clear();
        let silence = vec![0.0; self.channels];
        for _ in 0..TP_DELAY + self.lookahead - 1 {
            self.push_frame(&silence, output);
        }
    }

    fn push_frame(&mut self, frame: &[f32], output: &mut Vec<f32>) {
        for _ in 0..self.channels {
            self.history.pop_front();
        }
        self.history.extend(frame.iter().copied());
        self.frames_in += 1;
        // The oldest TP_DELAY frames of history are the zero pre-fill.
        if self.frames_in <= TP_DELAY as u64 {
            return;
        }
        let peak = self.true_peak_of_detected_frame();
        let base = (TP_TAPS - 1 - TP_DELAY) * self.channels;
        for ch in 0..self.channels {
            self.delay.push_back(self.history[base + ch]);
        }
        self.apply_gain(peak, output);
    }

    /// Peak of frame `TP_DELAY` frames ago, including the interpolated
    /// values between it and the following frame.
    fn true_peak_of_detected_frame(&self) -> f32 {
        let ch_count = self.channels;
        let center = TP_TAPS - 1 - TP_DELAY;
        let mut peak = 0.0f32;
        for ch in 0..ch_count {
            peak = peak.max(self.history[center * ch_count + ch].abs());
            for phase in &self.tp_coeffs {
                let mut acc = 0.0f32;
                for (tap, c) in phase.iter().enumerate() {
                    acc += self.history[tap * ch_count + ch] * c;
                }
                peak = peak.max(acc.abs());
            }
        }
        peak
    }

    fn apply_gain(&mut self, peak: f32, output: &mut Vec<f32>) {
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
        self.window_sum += self.release_state;
        self.window.push_back(self.release_state);
        if let Some(old) = self.window.pop_front() {
            self.window_sum -= old;
        }
        let gain = (self.window_sum / self.lookahead as f64).min(1.0);

        // The gain lines up with the frame that entered lookahead-1 frames ago.
        if self.delay.len() >= self.lookahead * self.channels {
            self.min_gain = self.min_gain.min(gain);
            let gain = gain as f32;
            let ceiling = self.ceiling;
            for _ in 0..self.channels {
                let sample = self.delay.pop_front().unwrap_or(0.0) * gain;
                // Guard against floating-point rounding in the running sum.
                output.push(sample.clamp(-ceiling, ceiling));
            }
        }
    }
}

/// Hann-windowed sinc taps for the fractional positions 1/4, 2/4 and 3/4
/// between history frame `TP_TAPS - 1 - TP_DELAY` and the next one.
fn interpolation_coeffs() -> [[f32; TP_TAPS]; OVERSAMPLE - 1] {
    let center = (TP_TAPS - 1 - TP_DELAY) as f64;
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
