//! The equalizer: a bank of biquad filters, one after another.
//!
//! Ten bands at the octave centres from 31.5 Hz to 16 kHz. The lowest is a low shelf (it raises or
//! lowers everything below it), the highest a high shelf, the eight between are peaks one octave wide.
//! Each has a gain in dB; at 0 dB a band does nothing at all and is skipped, so with every band at 0
//! the sound leaves exactly as it came.
//!
//! The filters are RBJ's ("Audio EQ Cookbook") biquads, in the shape of Vuuvee's `dsp/biquad.rs`:
//! direct form I, in 64-bit floats (at 31.5 Hz a 32-bit state would hiss).
//!
//! Gains are set from another thread (`Gains`, a few atomics) and never jump in the sound: the audio
//! thread moves each band toward its set gain a step per block and makes that band's filter anew.
//!
//! So that a raised band cannot clip, the whole sound is turned down by the highest point of the curve
//! (`headroom`).

use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const BANDS: usize = 10;
pub const CENTRES: [f32; BANDS] = [31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// a peak one octave wide
const Q: f64 = std::f64::consts::SQRT_2;
/// the furthest a band can be set, in dB
pub const MOST: f32 = 12.0;
/// how far a band's gain moves in one block, in dB (a block is about 3 ms)
const STEP: f32 = 0.25;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    LowShelf,
    Peak,
    HighShelf,
}

fn kind(band: usize) -> Kind {
    match band {
        0 => Kind::LowShelf,
        b if b == BANDS - 1 => Kind::HighShelf,
        _ => Kind::Peak,
    }
}

/// A biquad's five numbers, with a0 divided out.
#[derive(Clone, Copy)]
struct Coef {
    b: [f64; 3],
    a: [f64; 2],
}

/// RBJ's filter for one band at a gain.
fn coef(band: usize, gain_db: f32, rate: f64) -> Coef {
    let a = 10f64.powf(gain_db as f64 / 40.0);
    // a band above 0.45 of the rate is held there (16 kHz at 32,000 Hz would be at Nyquist)
    let w = 2.0 * PI * (CENTRES[band] as f64).min(0.45 * rate) / rate;
    let (cos, sin) = (w.cos(), w.sin());
    let (b, d) = match kind(band) {
        Kind::Peak => {
            let alpha = sin / (2.0 * Q);
            ([1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a])
        }
        shelf => {
            // slope 1: the steepest shelf that does not overshoot
            let k = 2.0 * a.sqrt() * (sin / 2.0 * 2f64.sqrt());
            let s = if shelf == Kind::LowShelf { 1.0 } else { -1.0 };
            (
                [a * ((a + 1.0) - s * (a - 1.0) * cos + k), s * 2.0 * a * ((a - 1.0) - s * (a + 1.0) * cos), a * ((a + 1.0) - s * (a - 1.0) * cos - k)],
                [(a + 1.0) + s * (a - 1.0) * cos + k, -s * 2.0 * ((a - 1.0) + s * (a + 1.0) * cos), (a + 1.0) + s * (a - 1.0) * cos - k],
            )
        }
    };
    Coef { b: [b[0] / d[0], b[1] / d[0], b[2] / d[0]], a: [d[1] / d[0], d[2] / d[0]] }
}

/// How many dB the curve of these gains is at a frequency.
pub fn response_db(gains: &[f32; BANDS], rate: f64, hz: f64) -> f32 {
    let w = 2.0 * PI * hz / rate;
    let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
    let mut db = 0.0;
    for (band, &gain) in gains.iter().enumerate().filter(|g| *g.1 != 0.0) {
        let c = coef(band, gain, rate);
        let top = (c.b[0] + c.b[1] * c1 + c.b[2] * c2).powi(2) + (c.b[1] * s1 + c.b[2] * s2).powi(2);
        let bottom = (1.0 + c.a[0] * c1 + c.a[1] * c2).powi(2) + (c.a[0] * s1 + c.a[1] * s2).powi(2);
        db += 10.0 * (top / bottom).log10();
    }
    db as f32
}

/// How far the whole sound is turned down so that the curve's highest point is at 0 dB: 0 or less.
pub fn headroom(gains: &[f32; BANDS], rate: f64) -> f32 {
    // the curve, looked at every sixth of an octave from 20 Hz up
    let between = (0..61).map(|i| 20.0 * 2f64.powf(i as f64 / 6.0));
    let top = between.chain(CENTRES.iter().map(|c| *c as f64)).map(|hz| response_db(gains, rate, hz.min(0.49 * rate))).fold(0.0f32, f32::max);
    -top
}

/// The gains as they are set, by whoever sets them (a window, the keyboard); read by the audio thread.
pub struct Gains {
    db: [AtomicU32; BANDS],
    /// false: the sound passes untouched whatever the gains are
    pub on: AtomicBool,
}

impl Default for Gains {
    fn default() -> Gains {
        Gains { db: std::array::from_fn(|_| AtomicU32::new(0f32.to_bits())), on: AtomicBool::new(true) }
    }
}

impl Gains {
    pub fn set(&self, band: usize, db: f32) {
        self.db[band].store(db.clamp(-MOST, MOST).to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> [f32; BANDS] {
        std::array::from_fn(|b| f32::from_bits(self.db[b].load(Ordering::Relaxed)))
    }

    /// The flat curve: every band at 0 dB.
    pub fn flat(&self) {
        (0..BANDS).for_each(|b| self.set(b, 0.0));
    }
}

/// One biquad's memory: the last two samples in and out.
#[derive(Clone, Copy, Default)]
struct Memory {
    x: [f64; 2],
    y: [f64; 2],
}

/// The equalizer as the audio thread runs it.
pub struct Eq {
    rate: f64,
    /// each band's gain as it is in the sound now (on its way to the set gain), and its filter
    now: [f32; BANDS],
    coefs: [Coef; BANDS],
    /// the whole sound's gain now, in dB (the headroom, on its way), and the headroom of the gains last seen
    level: f32,
    seen: [f32; BANDS],
    room: f32,
    /// memory[channel][band]
    memory: Vec<[Memory; BANDS]>,
}

impl Eq {
    pub fn new(rate: u32) -> Eq {
        let rate = rate as f64;
        Eq { rate, now: [0.0; BANDS], coefs: std::array::from_fn(|b| coef(b, 0.0, rate)), level: 0.0, seen: [0.0; BANDS], room: 0.0, memory: Vec::new() }
    }

    /// One block of interleaved frames, equalized in place toward the gains set.
    pub fn process(&mut self, sound: &mut [f32], channels: usize, gains: &Gains) {
        if self.memory.len() != channels {
            self.memory = vec![[Memory::default(); BANDS]; channels];
        }
        let set = if gains.on.load(Ordering::Relaxed) { gains.get() } else { [0.0; BANDS] };
        for band in 0..BANDS {
            if self.now[band] != set[band] {
                self.now[band] += (set[band] - self.now[band]).clamp(-STEP, STEP);
                self.coefs[band] = coef(band, self.now[band], self.rate);
            }
        }
        if set != self.seen {
            (self.seen, self.room) = (set, headroom(&set, self.rate));
        }
        let level_before = self.level;
        self.level += (self.room - self.level).clamp(-STEP, STEP);
        if self.now.iter().all(|g| *g == 0.0) && self.level == 0.0 && level_before == 0.0 {
            return; // flat: the sound is left exactly as it came
        }

        let frames = sound.len() / channels;
        let (from, to) = (10f64.powf(level_before as f64 / 20.0), 10f64.powf(self.level as f64 / 20.0));
        for (c, memory) in self.memory.iter_mut().enumerate() {
            for (i, frame) in sound.chunks_mut(channels).enumerate() {
                let mut s = frame[c] as f64;
                for band in (0..BANDS).filter(|b| self.now[*b] != 0.0) {
                    let (k, m) = (&self.coefs[band], &mut memory[band]);
                    let y = k.b[0] * s + k.b[1] * m.x[0] + k.b[2] * m.x[1] - k.a[0] * m.y[0] - k.a[1] * m.y[1];
                    m.x = [s, m.x[0]];
                    m.y = [y, m.y[0]];
                    s = y;
                }
                // the level slides across the block
                frame[c] = (s * (from + (to - from) * (i + 1) as f64 / frames as f64)) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48000;

    fn sine(hz: f64, seconds: f64) -> Vec<f32> {
        (0..(seconds * RATE as f64) as usize).map(|i| (0.25 * (2.0 * PI * hz * i as f64 / RATE as f64).sin()) as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// A sine through the equalizer once it has settled: how many dB louder it comes out, without the
    /// headroom.
    fn measured(gains: &[f32; BANDS], hz: f64) -> f64 {
        let set = Gains::default();
        gains.iter().enumerate().for_each(|(b, g)| set.set(b, *g));
        let mut eq = Eq::new(RATE);
        let mut sound = sine(hz, 3.0);
        let before = rms(&sound[RATE as usize * 2..]);
        sound.chunks_mut(144).for_each(|block| eq.process(block, 1, &set));
        20.0 * (rms(&sound[RATE as usize * 2..]) / before).log10() - headroom(gains, RATE as f64) as f64
    }

    #[test]
    fn flat_leaves_the_sound_as_it_came() {
        let set = Gains::default();
        let mut eq = Eq::new(RATE);
        let before = sine(440.0, 0.5);
        let mut sound = before.clone();
        sound.chunks_mut(144).for_each(|block| eq.process(block, 1, &set));
        assert!(sound == before);
    }

    #[test]
    fn a_peak_is_its_gain_at_its_centre_and_nothing_far_away() {
        let mut gains = [0.0; BANDS];
        gains[5] = 6.0; // 1 kHz
        assert!((response_db(&gains, RATE as f64, 1000.0) - 6.0).abs() < 0.01);
        assert!(response_db(&gains, RATE as f64, 31.5).abs() < 0.1);
        assert!(response_db(&gains, RATE as f64, 16000.0).abs() < 0.1);
        // one octave wide: half the gain (in dB) half an octave to each side
        assert!((response_db(&gains, RATE as f64, 707.0) - 3.0).abs() < 0.3);
        assert!((response_db(&gains, RATE as f64, 1414.0) - 3.0).abs() < 0.3);
    }

    #[test]
    fn the_shelves_raise_the_ends() {
        let mut low = [0.0; BANDS];
        low[0] = 6.0;
        assert!((response_db(&low, RATE as f64, 5.0) - 6.0).abs() < 0.1);
        assert!((response_db(&low, RATE as f64, 31.5) - 3.0).abs() < 0.1);
        assert!(response_db(&low, RATE as f64, 1000.0).abs() < 0.1);
        let mut high = [0.0; BANDS];
        high[BANDS - 1] = -6.0;
        assert!((response_db(&high, RATE as f64, 23000.0) + 6.0).abs() < 0.3);
        assert!((response_db(&high, RATE as f64, 16000.0) + 3.0).abs() < 0.1);
        assert!(response_db(&high, RATE as f64, 1000.0).abs() < 0.1);
    }

    #[test]
    fn what_comes_out_is_what_the_curve_says() {
        let gains = [4.0, -3.0, 6.0, 0.0, -6.0, 2.0, 0.0, 5.0, -4.0, 3.0];
        for hz in [40.0, 100.0, 125.0, 300.0, 500.0, 1000.0, 3000.0, 4000.0, 9000.0, 15000.0] {
            let (said, heard) = (response_db(&gains, RATE as f64, hz) as f64, measured(&gains, hz));
            assert!((said - heard).abs() < 0.05, "{hz} Hz: the curve says {said:.2} dB, the sound is {heard:.2} dB");
        }
    }

    #[test]
    fn every_band_at_its_most_stays_sound() {
        for most in [MOST, -MOST] {
            let set = Gains::default();
            (0..BANDS).for_each(|b| set.set(b, most));
            let mut eq = Eq::new(RATE);
            let mut sound: Vec<f32> = (0..RATE).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
            sound.chunks_mut(144).for_each(|block| eq.process(block, 1, &set));
            assert!(sound.iter().all(|s| s.is_finite()));
            assert!(sound[RATE as usize - 100..].iter().all(|s| s.abs() < 1e-6), "the filters ring on");
        }
    }

    #[test]
    fn the_headroom_keeps_a_raised_curve_from_clipping() {
        let mut gains = [0.0; BANDS];
        gains[5] = 9.0;
        assert!((headroom(&gains, RATE as f64) + 9.0).abs() < 0.05);
        assert_eq!(headroom(&[-6.0; BANDS], RATE as f64), 0.0);
        // a full-scale sine at the raised band comes out at full scale, not over
        let set = Gains::default();
        set.set(5, 9.0);
        let mut eq = Eq::new(RATE);
        let mut sound: Vec<f32> = (0..RATE * 2).map(|i| (2.0 * PI * 1000.0 * i as f64 / RATE as f64).sin() as f32).collect();
        sound.chunks_mut(144).for_each(|block| eq.process(block, 1, &set));
        let peak = sound[RATE as usize..].iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak <= 1.002 && peak > 0.99, "peak {peak}");
    }

    #[test]
    fn a_gain_set_while_playing_does_not_click() {
        let set = Gains::default();
        let mut eq = Eq::new(RATE);
        let mut sound = sine(1000.0, 1.0);
        let slope = 0.25 * 2.0 * PI * 1000.0 / RATE as f64; // the most a 1 kHz sine at this level moves in a frame
        let mut last = 0.0f32;
        let mut jump = 0.0f64;
        for (n, block) in sound.chunks_mut(144).enumerate() {
            if n == 100 {
                set.set(5, 12.0);
                set.set(2, -12.0);
            }
            eq.process(block, 1, &set);
            for s in block.iter() {
                jump = jump.max((*s - last).abs() as f64);
                last = *s;
            }
        }
        // the sine itself may be up to 12 dB louder for a moment before the headroom has come down
        assert!(jump < slope * 4.5, "a jump of {jump:.4} (the sine alone: {slope:.4})");
    }
}
