//! The live spectrum: the last few thousand samples of the sound, as levels by frequency.
//!
//! The audio thread only pushes its samples into a `Ring` (it never waits); the window takes them out,
//! keeps the last `SIZE` of them, and on each look windows them (Hann), transforms them (rustfft, as
//! Vuuvee's `dsp/fft.rs` does) and reads each bin's level in dB, where a full-scale sine is 0 dB.
//! A level rises at once and falls slowly, so the picture can be read.
//!
//! What is drawn is not the bins themselves (thousands of needles) but `band`: the power of all the
//! bins between two frequencies, added up. Read over a band a sixth of an octave wide that slides along
//! the axis, it is one smooth line; a full-scale sine is still 0 dB, and sound with as much power in
//! every octave (pink noise, and roughly most music) is a level line, so what the equalizer raises or
//! lowers shows as the line bending.

use std::sync::Arc;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use crate::ring::Ring;

/// samples in one transform: at 48 kHz 171 ms of sound, bins 5.9 Hz apart
pub const SIZE: usize = 8192;
/// a sine's power lies over this many bins of a Hann window (its noise bandwidth)
const SPREAD: f32 = 1.5;
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

    /// The level of the sound between two frequencies, in dB: the bins' power there added up (between
    /// two bins: the power on the line from one to the other).
    pub fn band(&self, from: f32, to: f32, rate: f32) -> f32 {
        let top = (SIZE / 2) as f32;
        let (a, b) = ((from * SIZE as f32 / rate).clamp(0.0, top), (to * SIZE as f32 / rate).clamp(0.0, top));
        let power = |place: f32| {
            let bin = (place as usize).min(SIZE / 2 - 1);
            let (low, high) = (10f32.powf(self.levels[bin] / 10.0), 10f32.powf(self.levels[bin + 1] / 10.0));
            low + (high - low) * (place - bin as f32)
        };
        // two looks a bin, evenly across the band
        let n = 2 * ((b - a).ceil() as usize).max(1);
        let mean = (0..=n).map(|i| power(a + (b - a) * i as f32 / n as f32)).sum::<f32>() / (n + 1) as f32;
        (10.0 * (mean * (b - a) / SPREAD).max(1e-12).log10()).max(FLOOR)
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
        // the sixth of an octave around the sine holds all of it: -6 dB; the sixth beside it nothing
        let sixth = 2f32.powf(1.0 / 12.0);
        let around = s.band(hz / sixth, hz * sixth, rate);
        assert!((around - -6.02).abs() < 0.5, "the band around the sine is {around} dB");
        assert!(s.band(hz * sixth * sixth, hz * sixth.powi(4), rate) < -60.0);
        // silence after it: the level falls 30 dB in a second, not at once
        ring.push(&vec![0.0; SIZE]);
        s.take(&ring);
        s.look(1.0);
        assert!((s.levels[top] - (-6.0 - 30.0)).abs() < 1.6, "a second later the top is {} dB", s.levels[top]);
    }
}
