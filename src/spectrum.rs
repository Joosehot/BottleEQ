//! The live spectrum: the last few thousand samples of the sound, as levels by frequency.
//!
//! The audio thread only pushes its samples into a `Ring` (it never waits); the window takes them out,
//! keeps the last `SIZE` of them, and on each look windows them (Hann), transforms them (rustfft, as
//! Vuuvee's `dsp/fft.rs` does) and reads each bin's level in dB, where a full-scale sine is 0 dB.
//! A level rises at once and falls slowly, so the picture can be read.

use std::sync::Arc;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use crate::ring::Ring;

/// samples in one transform: at 48 kHz 85 ms of sound, bins 11.7 Hz apart
pub const SIZE: usize = 4096;
/// the lowest level kept, in dB
pub const FLOOR: f32 = -100.0;
/// how fast a level falls, in dB a second
const FALL: f32 = 30.0;

pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// the last SIZE samples, the oldest at `at`
    last: Vec<f32>,
    at: usize,
    work: Vec<Complex<f32>>,
    /// each bin's level in dB, 0 to SIZE / 2
    pub levels: Vec<f32>,
}

impl Spectrum {
    pub fn new() -> Spectrum {
        let window: Vec<f32> = (0..SIZE).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / SIZE as f32).cos()).collect();
        Spectrum {
            fft: FftPlanner::new().plan_fft_forward(SIZE),
            window,
            last: vec![0.0; SIZE],
            at: 0,
            work: vec![Complex::new(0.0, 0.0); SIZE],
            levels: vec![FLOOR; SIZE / 2 + 1],
        }
    }

    /// The frequency of a bin.
    #[cfg(test)]
    pub fn hz(bin: usize, rate: f32) -> f32 {
        bin as f32 * rate / SIZE as f32
    }

    /// Take what the audio thread has pushed since the last look.
    pub fn take(&mut self, ring: &Ring) {
        let n = ring.len();
        for i in 0..n {
            self.last[self.at] = ring.peek(i);
            self.at = (self.at + 1) % SIZE;
        }
        ring.skip(n);
    }

    /// Look: the levels now, `seconds` after the last look.
    pub fn look(&mut self, seconds: f32) {
        for (i, c) in self.work.iter_mut().enumerate() {
            *c = Complex::new(self.last[(self.at + i) % SIZE] * self.window[i], 0.0);
        }
        self.fft.process(&mut self.work);
        // a sine of height 1 is 0 dB: its bin holds half the window's sum
        let full = self.window.iter().sum::<f32>() / 2.0;
        for (level, c) in self.levels.iter_mut().zip(&self.work) {
            let now = (20.0 * (c.norm() / full).max(1e-9).log10()).max(FLOOR);
            *level = now.max(*level - FALL * seconds);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_stands_at_its_frequency_and_level() {
        let (rate, hz) = (48000.0, 3000.0);
        let ring = Ring::new(2 * SIZE);
        let sine: Vec<f32> = (0..SIZE).map(|i| 0.5 * (std::f32::consts::TAU * hz * i as f32 / rate).sin()).collect();
        ring.push(&sine);
        let mut s = Spectrum::new();
        s.take(&ring);
        s.look(0.0);
        let top = (0..s.levels.len()).max_by(|a, b| s.levels[*a].total_cmp(&s.levels[*b])).unwrap();
        assert!((Spectrum::hz(top, rate) - hz).abs() < rate / SIZE as f32, "the top is at {} Hz", Spectrum::hz(top, rate));
        // 0.5 is -6.02 dB; between two bins a Hann window reads up to 1.4 dB low
        assert!(s.levels[top] < -6.0 && s.levels[top] > -7.5, "the top is {} dB", s.levels[top]);
        assert!(s.levels[top + 40] < -80.0, "far from the sine there is {} dB", s.levels[top + 40]);
        // silence after it: the level falls 30 dB in a second, not at once
        ring.push(&vec![0.0; SIZE]);
        s.take(&ring);
        s.look(1.0);
        assert!((s.levels[top] - (-6.0 - 30.0)).abs() < 1.6, "a second later the top is {} dB", s.levels[top]);
    }
}
