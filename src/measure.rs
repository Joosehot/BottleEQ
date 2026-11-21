//! Measuring what reaches the listening place: a sweep is played out of the speakers and recorded by a
//! microphone at the same time, and the two are compared.
//!
//! - **The sweep** is a sine that rises from 20 Hz to 20 kHz in ten seconds, each octave taking as long
//!   as the last (a logarithmic sweep).
//! - **The comparison**: the recording's spectrum divided by the sweep's is what the way from the
//!   program to the microphone did to each frequency (speakers, room, microphone). Turned back into
//!   time it is that way's answer to one click (its impulse response): a spike where the sound arrives,
//!   the room's echoes after it. Nothing has to be lined up by hand: where the spike stands is the delay.
//! - **The noise**: ten seconds of sweep are gathered into that one short spike, while the room's noise
//!   stays spread over the whole time. Only the half second around the spike is kept, so most of
//!   the noise is left out; what remains is read with the same window at places after the spike where
//!   only noise is. A frequency whose level is not at least 10 dB above that noise is marked uncertain.
//! - **The curve** is the level in each sixth of an octave, every 24th of an octave, in dB against its
//!   own average between 200 Hz and 5 kHz (how loud the microphone is set says nothing).
//!
//! - **Twice, at two levels**: the sweep is played once more, 10 dB quieter. Speakers and a room answer
//!   the same at any level; a frequency that does not come out 10 dB quieter (within 3 dB) is not being
//!   answered by them alone (small speakers below their range, a microphone that gates or evens out
//!   what it hears), and is marked uncertain too: no equalizer can set right what changes with level.
//!
//! The microphone's own response is in the curve: what is measured is speakers, room and microphone
//! together, and a cheap microphone's weak bass or treble cannot be told from the room's.

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use crate::audio::{self, Input, Output};
use crate::eq::Curve;

pub const LOW: f64 = 20.0;
pub const HIGH: f64 = 20000.0;
/// the sweep's length in seconds
pub const SECONDS: f64 = 10.0;
/// silence recorded before the sweep and after it, in seconds (the noise is read after it)
pub const BEFORE: f64 = 1.0;
pub const AFTER: f64 = 3.6;
/// the part of the answer kept: this long before the spike and after it, in seconds. (Before it: the
/// sweep ends at 20 Hz and 20 kHz, and an answer cut off there rings a few cycles to both sides of the
/// spike; with 2 ms kept before it the lowest octave read 1 dB wrong.)
const PRE: f64 = 0.15;
const POST: f64 = 0.3;
/// where the noise is read: this long after the spike, in seconds
const NOISE_AT: [f64; 3] = [1.0, 1.7, 2.4];
/// how far above the noise a level must be to be believed, in dB
pub const CLEAR: f32 = 10.0;
/// the second sweep: this much quieter than the first, in dB, and how far from exactly that much
/// quieter a frequency may come out and still be believed
pub const QUIETER: f32 = 10.0;
pub const STEADY: f32 = 3.0;

/// The sweep at a moment, at full height: it fades in over a tenth of a second and out over a hundredth.
fn sweep_at(t: f64) -> f64 {
    if !(0.0..SECONDS).contains(&t) {
        return 0.0;
    }
    let octaves = (HIGH / LOW).ln();
    let phase = std::f64::consts::TAU * LOW * SECONDS / octaves * ((t / SECONDS * octaves).exp() - 1.0);
    let fade = |x: f64| 0.5 - 0.5 * (std::f64::consts::PI * x.clamp(0.0, 1.0)).cos();
    phase.sin() * fade(t / 0.1) * fade((SECONDS - t) / 0.01)
}

/// The sweep as samples (one channel).
pub fn sweep(rate: u32, level: f32) -> Vec<f32> {
    (0..(SECONDS * rate as f64) as usize).map(|i| level * sweep_at(i as f64 / rate as f64) as f32).collect()
}

/// What was measured: the level at each frequency, in dB against the curve's own average.
#[derive(Clone)]
pub struct Measured {
    pub hz: Vec<f32>,
    pub db: Vec<f32>,
    /// to be believed: clearly above the noise, and (when measured at two levels) the same at both
    pub certain: Vec<bool>,
    /// how much quieter than the 10 dB asked each frequency came out of the quieter sweep, in dB
    /// (0: just as it should); empty when only one level was measured
    pub off_level: Vec<f32>,
    /// how far from the 10 dB quieter it was sent the quieter sweep arrived as a whole, in dB (0: as
    /// sent; +14: it arrived 4 dB louder than the first). Something on the way then sets the volume by
    /// itself; each frequency is judged against this, not against what was sent
    pub second_off: f32,
    /// the equalizer's curve the sweep went through, in dB at each frequency (0 where it did not)
    pub eq: Vec<f32>,
    /// true where the level was not measured but guessed (`guess.rs`); empty when nothing was
    pub guessed: Vec<bool>,
    /// how far above the noise each level is, in dB
    pub over_noise: Vec<f32>,
    /// the loudest sample of the recording (1 is the most a recording holds: it clipped)
    pub peak: f32,
    /// when the sweep arrived at the microphone, in seconds from the recording's start
    pub arrived: f32,
    /// the average the curve is set against: how loud the microphone heard the sweep, in dB of the
    /// sweep as it was sent
    pub level: f32,
}

impl Measured {
    /// The share of the frequencies that are to be believed.
    pub fn share(&self) -> f32 {
        self.certain.iter().filter(|c| **c).count() as f32 / self.certain.len().max(1) as f32
    }

    /// What is to be said when the quieter sweep did not arrive as much quieter as it was sent.
    pub fn levels_said(&self) -> Option<String> {
        (self.second_off.abs() > STEADY).then(|| {
            format!(
                "the second sweep was sent {QUIETER:.0} dB quieter than the first and arrived {:+.1} dB against it: something between Bottle EQ and the speakers sets the volume by itself (a leveller, a limiter, sound effects, the speakers' own); the curve holds only as far as that kept still within each sweep",
                self.second_off - QUIETER
            )
        })
    }

    /// The level nearest a frequency, and whether it is certain.
    #[cfg(test)]
    pub fn at(&self, hz: f32) -> (f32, bool) {
        let i = (0..self.hz.len()).min_by(|a, b| (self.hz[*a] / hz).ln().abs().total_cmp(&(self.hz[*b] / hz).ln().abs())).unwrap_or(0);
        (self.db[i], self.certain[i])
    }
}

/// Compare a recording (one channel, at `rate`) with the sweep that was played while it was made.
pub fn analyse(recording: &[f32], rate: u32) -> Result<Measured, String> {
    let rate_f = rate as f64;
    let sent: Vec<f64> = (0..(SECONDS * rate_f) as usize).map(|i| sweep_at(i as f64 / rate_f)).collect();
    if recording.len() < sent.len() {
        return Err("the recording is shorter than the sweep".to_string());
    }
    let peak = recording.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    if peak <= 0.0 {
        return Err("the microphone gave only silence (is it the right one, and is it switched on?)".to_string());
    }
    // the recording's spectrum over the sweep's, at the frequencies the sweep has
    let n = (recording.len() + sent.len()).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let spectrum = |sound: &mut dyn Iterator<Item = f64>, planner: &mut FftPlanner<f64>| {
        let mut all: Vec<Complex<f64>> = sound.map(|s| Complex::new(s, 0.0)).collect();
        all.resize(n, Complex::new(0.0, 0.0));
        planner.plan_fft_forward(n).process(&mut all);
        all
    };
    let x = spectrum(&mut sent.iter().copied(), &mut planner);
    let mut h = spectrum(&mut recording.iter().map(|s| *s as f64), &mut planner);
    let most = x.iter().map(|c| c.norm_sqr()).fold(0.0, f64::max);
    for k in 0..=n / 2 {
        let hz = k as f64 * rate_f / n as f64;
        let over = if (0.9 * LOW..1.02 * HIGH).contains(&hz) { h[k] * x[k].conj() / (x[k].norm_sqr() + 1e-9 * most) } else { Complex::new(0.0, 0.0) };
        h[k] = over;
        if k > 0 && k < n / 2 {
            h[n - k] = over.conj();
        }
    }
    // back into time: the way's answer to one click
    planner.plan_fft_inverse(n).process(&mut h);
    let answer: Vec<f64> = h.iter().map(|c| c.re / n as f64).collect();
    let spike = (0..recording.len()).max_by(|a, b| answer[*a].abs().total_cmp(&answer[*b].abs())).unwrap_or(0);
    // the noise is read after the spike, where the recording still holds the room's noise at every
    // frequency: before the recording ends, the sweep's length and the last noise window after the spike
    let last = NOISE_AT[NOISE_AT.len() - 1] + POST;
    if spike + sent.len() + (last * rate_f) as usize > recording.len() {
        return Err("the sweep was not found whole in the recording (it came too late or not at all): is the sound on, and is this the microphone that hears it?".to_string());
    }

    // the half second around a place in the answer, as a spectrum of powers
    let (pre, post) = ((PRE * rate_f) as usize, (POST * rate_f) as usize);
    let size = (2 * (pre + post)).next_power_of_two();
    let cut = planner.plan_fft_forward(size);
    let around = |place: usize| -> Vec<f64> {
        let mut part = vec![Complex::new(0.0, 0.0); size];
        for (i, p) in part.iter_mut().enumerate().take(pre + post) {
            // rising over the first half of the time before the place, level until half the time after
            // it, then falling
            let weight = match i {
                i if i < pre / 2 => 0.5 - 0.5 * (std::f64::consts::PI * i as f64 / (pre / 2) as f64).cos(),
                i if i < pre + post / 2 => 1.0,
                i => 0.5 + 0.5 * (std::f64::consts::PI * (i - pre - post / 2) as f64 / (post - post / 2) as f64).cos(),
            };
            *p = Complex::new(answer[(place + n - pre + i) % n] * weight, 0.0);
        }
        cut.process(&mut part);
        part[..=size / 2].iter().map(|c| c.norm_sqr()).collect()
    };
    let heard = around(spike);
    let mut noise = vec![0.0; heard.len()];
    for at in NOISE_AT {
        let there = around(spike + (at * rate_f) as usize);
        noise.iter_mut().zip(there).for_each(|(n, t)| *n += t / NOISE_AT.len() as f64);
    }

    // the level in each sixth of an octave, every 24th of an octave
    let band = |powers: &[f64], hz: f64| -> f64 {
        let place = |hz: f64| hz * size as f64 / rate_f;
        let (from, to) = (place(hz / 2f64.powf(1.0 / 12.0)).ceil() as usize, (place(hz * 2f64.powf(1.0 / 12.0)) as usize).min(size / 2));
        match to >= from {
            true => powers[from..=to].iter().sum::<f64>() / (to - from + 1) as f64,
            false => powers[(place(hz).round() as usize).min(size / 2)],
        }
    };
    let top = HIGH.min(0.45 * rate_f);
    let hz: Vec<f64> = (0..).map(|i| LOW * 2f64.powf(i as f64 / 24.0)).take_while(|hz| *hz <= top).collect();
    let level: Vec<f32> = hz.iter().map(|hz| (10.0 * band(&heard, *hz).max(1e-30).log10()) as f32).collect();
    let over_noise: Vec<f32> = hz.iter().zip(&level).map(|(hz, level)| level - (10.0 * band(&noise, *hz).max(1e-30).log10()) as f32).collect();
    let certain: Vec<bool> = over_noise.iter().map(|over| *over >= CLEAR).collect();
    if !certain.iter().any(|c| *c) {
        return Err("the sweep was not heard above the noise at any frequency: turn the sound up or bring the microphone nearer".to_string());
    }
    // against its own average between 200 Hz and 5 kHz (over what is certain there, else over all that is)
    let middle: Vec<f32> = (0..hz.len()).filter(|i| certain[*i] && (200.0..5000.0).contains(&hz[*i])).map(|i| level[i]).collect();
    let all: Vec<f32> = (0..hz.len()).filter(|i| certain[*i]).map(|i| level[i]).collect();
    let of = if middle.is_empty() { &all } else { &middle };
    let average = of.iter().sum::<f32>() / of.len() as f32;
    let eq = vec![0.0; hz.len()];
    Ok(Measured { hz: hz.iter().map(|hz| *hz as f32).collect(), db: level.iter().map(|l| l - average).collect(), certain, off_level: Vec::new(), second_off: 0.0, eq, guessed: Vec::new(), over_noise, peak, arrived: spike as f32 / rate as f32, level: average })
}

/// One sweep: record the microphone, play the sweep to an output like any program would, and compare.
fn once(microphone: &Input, play_to: &Output, level: f32) -> Result<Measured, String> {
    let recording = audio::record(microphone)?;
    std::thread::sleep(std::time::Duration::from_secs_f64(BEFORE));
    let sound: Vec<f32> = sweep(play_to.rate, level).iter().flat_map(|s| [*s, *s]).collect();
    let _ = audio::play(play_to, sound).join();
    std::thread::sleep(std::time::Duration::from_secs_f64(AFTER));
    let (rate, sound) = recording.finish()?;
    analyse(&sound, rate)
}

/// The louder of two measurements, with every frequency that the quieter one does not bear out marked
/// uncertain: where the quieter is not clearly above the noise itself, or did not come out `QUIETER`
/// dB quieter within `STEADY`.
fn borne_out(mut loud: Measured, quiet: &Measured) -> Measured {
    loud.off_level = (0..loud.hz.len()).map(|i| (quiet.db[i] + quiet.level) - (loud.db[i] + loud.level) + QUIETER).collect();
    // the sweep as a whole: the middle one of the frequencies heard clearly in both. On some machines
    // the second sweep does not arrive as much quieter as it was sent (it was heard louder than the
    // first on a Linux machine); a frequency is then held against the whole, not against what was sent
    let mut both: Vec<f32> = (0..loud.hz.len()).filter(|i| loud.certain[*i] && quiet.certain[*i]).map(|i| loud.off_level[i]).collect();
    both.sort_by(|a, b| a.total_cmp(b));
    loud.second_off = both.get(both.len() / 2).copied().unwrap_or(0.0);
    for i in 0..loud.hz.len() {
        loud.certain[i] &= quiet.certain[i] && (loud.off_level[i] - loud.second_off).abs() <= STEADY;
    }
    loud
}

/// How long a measurement takes, in seconds, and a short one (one sweep).
pub const TAKES: f64 = 2.0 * (BEFORE + SECONDS + AFTER + 0.7);
pub const SHORT_TAKES: f64 = BEFORE + SECONDS + AFTER + 0.7;

/// The short measurement the guesser works from: one sweep at one level, a quarter of a minute. What
/// changes with the level is not found by it.
pub fn short(microphone: &Input, play_to: &Output, level: f32, through: &[Curve]) -> Result<Measured, String> {
    let mut m = once(microphone, play_to, level)?;
    m.eq = m.hz.iter().map(|hz| through.iter().map(|curve| curve.response_db(play_to.rate as f64, *hz as f64)).sum()).collect();
    Ok(m)
}

/// Measure: the sweep at `level` and once more `QUIETER` dB under it, about half a minute in all.
/// `play_to` is the device the sound is on its way to the speakers from: the path's source when the
/// path runs (the sweep then passes the equalizer as music does; `through` are its curves then), else
/// the speakers themselves.
pub fn measure(microphone: &Input, play_to: &Output, level: f32, through: &[Curve]) -> Result<Measured, String> {
    let loud = once(microphone, play_to, level)?;
    let quiet = once(microphone, play_to, level * 10f32.powf(-QUIETER / 20.0))?;
    let mut m = borne_out(loud, &quiet);
    m.eq = m.hz.iter().map(|hz| through.iter().map(|curve| curve.response_db(play_to.rate as f64, *hz as f64)).sum()).collect();
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eq::{Eq, Kind, Point, Settings, CUT_Q};

    /// What a microphone would record: silence, the sweep through `way` (a block at a time), silence;
    /// and noise of a height over all of it.
    fn recorded(rate: u32, noise: f32, mut way: impl FnMut(&mut [f32])) -> Vec<f32> {
        let mut sound = vec![0.0; (1.23 * rate as f64) as usize];
        sound.extend(sweep(rate, 0.5));
        sound.extend(vec![0.0; (AFTER * rate as f64) as usize]);
        sound.chunks_mut(128).for_each(&mut way);
        let mut seed = 12345u32;
        for s in sound.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *s += noise * ((seed >> 8) as f32 / 8388608.0 - 1.0);
        }
        sound
    }

    /// The equalizer set to a curve and settled there, as the way the sweep goes.
    fn through(settings: &Settings, rate: u32) -> impl FnMut(&mut [f32]) + '_ {
        let mut eq = Eq::new(rate);
        for _ in 0..2000 {
            eq.process(&mut [0.0; 128], 1, settings);
        }
        move |block| eq.process(block, 1, settings)
    }

    /// The curve as the measurement shows one: against its average over the same frequencies.
    fn expected(settings: &Settings, m: &Measured, rate: u32) -> Vec<f32> {
        let curve = settings.get();
        let db: Vec<f32> = m.hz.iter().map(|hz| curve.response_db(rate as f64, *hz as f64)).collect();
        let middle: Vec<f32> = (0..db.len()).filter(|i| m.certain[*i] && (200.0..5000.0).contains(&m.hz[*i])).map(|i| db[i]).collect();
        let average = middle.iter().sum::<f32>() / middle.len() as f32;
        db.iter().map(|d| d - average).collect()
    }

    #[test]
    fn a_way_that_changes_nothing_measures_flat() {
        for rate in [48000, 44100] {
            let m = analyse(&recorded(rate, 0.0, |_| {}), rate).unwrap();
            assert!((m.arrived - 1.23).abs() < 0.001, "the sweep arrived at {} s", m.arrived);
            // the sweep was sent at half height
            assert!((m.level - -6.02).abs() < 0.1, "it was heard at {} dB of what was sent", m.level);
            for i in 0..m.hz.len() {
                if (25.0..18000.0).contains(&m.hz[i]) {
                    assert!(m.certain[i] && m.db[i].abs() < 0.2, "at {} Hz and {rate}: {} dB, certain {}", m.hz[i], m.db[i], m.certain[i]);
                }
            }
        }
    }

    #[test]
    fn a_known_curve_is_measured_through_noise() {
        let settings = Settings::default();
        settings.set_all(&[6.0, 0.0, -4.0, 0.0, 3.0, -6.0, 0.0, 4.0, 0.0, -3.0]);
        // noise 54 dB under the sweep
        let m = analyse(&recorded(48000, 0.001, through(&settings, 48000)), 48000).unwrap();
        let want = expected(&settings, &m, 48000);
        assert!(m.share() > 0.95, "{} of the frequencies are certain", m.share());
        for i in 0..m.hz.len() {
            if m.certain[i] && (30.0..16000.0).contains(&m.hz[i]) {
                assert!((m.db[i] - want[i]).abs() < 0.5, "at {} Hz: measured {} dB, the curve is {} dB", m.hz[i], m.db[i], want[i]);
            }
        }
    }

    #[test]
    fn what_is_under_the_noise_is_marked_uncertain() {
        // a small speaker: nothing below 300 Hz (two slopes, 24 dB an octave), in a noisy room
        let settings = Settings::default();
        let cut = Point { kind: Some(Kind::LowCut), q: CUT_Q, ..Point::new(300.0, 0.0) };
        settings.set_points(&[cut, cut]);
        let m = analyse(&recorded(48000, 0.02, through(&settings, 48000)), 48000).unwrap();
        assert!(!m.at(30.0).1 && !m.at(50.0).1, "30 Hz is {:?}, 50 Hz is {:?}", m.at(30.0), m.at(50.0));
        assert!(m.at(1000.0).1, "1 kHz is {:?}", m.at(1000.0));
        assert!(m.share() < 0.9 && m.share() > 0.3, "{} of the frequencies are certain", m.share());
        // what is certain is still right
        let want = expected(&settings, &m, 48000);
        for i in 0..m.hz.len() {
            if m.certain[i] && m.over_noise[i] > 20.0 {
                assert!((m.db[i] - want[i]).abs() < 1.0, "at {} Hz: measured {} dB, the curve is {} dB", m.hz[i], m.db[i], want[i]);
            }
        }
    }

    #[test]
    fn what_changes_with_the_level_is_marked_uncertain() {
        // speakers that give no bass at all once the sound is quieter: the quieter sweep has a slope
        // at 150 Hz that the louder has not
        let gated = Settings::default();
        let cut = Point { kind: Some(Kind::LowCut), q: CUT_Q, ..Point::new(150.0, 0.0) };
        gated.set_points(&[cut, cut]);
        let quieter = 10f32.powf(-QUIETER / 20.0);
        let loud = analyse(&recorded(48000, 0.0005, |_| {}), 48000).unwrap();
        let mut way = through(&gated, 48000);
        let quiet = analyse(&recorded(48000, 0.0005, |block| { block.iter_mut().for_each(|s| *s *= quieter); way(block) }), 48000).unwrap();
        assert!(loud.at(60.0).1, "alone, the louder sweep is believed at 60 Hz");
        let m = borne_out(loud, &quiet);
        assert!(!m.at(60.0).1 && !m.at(100.0).1, "60 Hz is {:?}, 100 Hz is {:?}", m.at(60.0), m.at(100.0));
        assert!(m.at(1000.0).1 && m.at(5000.0).1);
        let i = (0..m.hz.len()).find(|i| m.hz[*i] >= 1000.0).unwrap();
        assert!(m.off_level[i].abs() < 0.3 && m.off_level[(0..m.hz.len()).find(|i| m.hz[*i] >= 60.0).unwrap()] < -10.0);
        // and the same sound twice is borne out everywhere it was believed
        let (a, b) = (analyse(&recorded(48000, 0.0005, |_| {}), 48000).unwrap(), analyse(&recorded(48000, 0.0005, |block| block.iter_mut().for_each(|s| *s *= quieter)), 48000).unwrap());
        let share = a.share();
        let m = borne_out(a, &b);
        assert!(m.share() > share - 0.05);
        assert!(m.second_off.abs() < 0.3 && m.levels_said().is_none());
    }

    #[test]
    fn a_second_sweep_that_arrives_louder_is_said_and_its_shape_still_counts() {
        // something on the way sets the volume by itself: the sweep sent 10 dB quieter arrives 4 dB
        // louder than the first, with the same shape
        let louder = 10f32.powf(4.0 / 20.0);
        let (a, b) = (analyse(&recorded(48000, 0.0005, |_| {}), 48000).unwrap(), analyse(&recorded(48000, 0.0005, |block| block.iter_mut().for_each(|s| *s *= louder)), 48000).unwrap());
        let share = a.share();
        let m = borne_out(a, &b);
        assert!((m.second_off - 14.0).abs() < 0.3, "the second sweep is {} dB off", m.second_off);
        assert!(m.share() > share - 0.05, "{} of {} is still believed", m.share(), share);
        assert!(m.levels_said().unwrap().contains("+4.0 dB"), "{:?}", m.levels_said());
    }

    #[test]
    fn a_recording_without_the_sweep_says_so() {
        assert!(analyse(&vec![0.0; 48000 * 15], 48000).is_err());
        let only_noise = recorded(48000, 0.01, |block| block.fill(0.0));
        assert!(analyse(&only_noise, 48000).is_err());
    }
}
