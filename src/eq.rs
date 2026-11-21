//! The equalizer: a bank of biquad filters, one after another.
//!
//! Two layers make the curve:
//!
//! - **Ten bands** at the octave centres from 31.5 Hz to 16 kHz: the lowest a low shelf, the highest a
//!   high shelf, the eight between peaks one octave wide, each with a gain. A *mode* (modes.txt) is a
//!   named set of these ten gains.
//! - **Points**, as in FL Studio's parametric EQ: up to eight filters that can be put anywhere. A point
//!   has a frequency, a gain, a width and a kind:
//!   - a *cut* is a slope: below a low cut (above a high cut) the sound falls away 12 dB an octave, so
//!     the curve starts (or ends) with a slope; its width is how sharp the corner is, and a sharp one
//!     stands up as a ridge before the slope;
//!   - a *shelf* raises or lowers everything below (or above) the point;
//!   - a *peak* is a bump or a dip around the point: a spike when it is narrow.
//!   A point that is given no kind is a shelf if it is the lowest or the highest of several, else a
//!   peak (FL Studio's own layout).
//!
//! A band or point at 0 dB does nothing at all and is skipped, so with everything at 0 (and no cut) the
//! sound leaves exactly as it came.
//!
//! The filters are RBJ's ("Audio EQ Cookbook") biquads, in the shape of Vuuvee's `dsp/biquad.rs`:
//! direct form I, in 64-bit floats (at 31.5 Hz a 32-bit state would hiss).
//!
//! The curve is set from another thread (`Settings`, a few atomics) and never jumps in the sound: the
//! audio thread moves each gain, frequency and width a step per block toward what is set and makes
//! that filter anew.
//!
//! So that a raised curve cannot clip, the whole sound is turned down by the curve's highest point
//! (`headroom`).
//!
//! **Volume** is a gain on the whole sound after that, from -40 to +18 dB. Turned up it can give back
//! what the headroom took, and more; then a sound at full height would no longer fit, so a limiter
//! stands after it: the moment a frame would go over full scale the whole sound is turned down just
//! enough (at once, all channels alike), and let back up over a sixth of a second. Nothing ever clips;
//! what it costs is that the loudest moments are held back, which is said (`Settings::limited`).

use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

pub const BANDS: usize = 10;
pub const CENTRES: [f32; BANDS] = [31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// the most points a curve can have
pub const POINTS: usize = 8;
/// a band's peak is one octave wide
const BAND_Q: f32 = std::f32::consts::SQRT_2;
/// a point's width when none is given: a broad bump; and a cut's: the flattest corner there is
pub const POINT_Q: f32 = 0.9;
pub const CUT_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// the furthest a gain can be set, in dB
pub const MOST: f32 = 12.0;
/// the volume's ends, in dB
pub const QUIETEST: f32 = -40.0;
pub const LOUDEST: f32 = 18.0;
/// how long the limiter takes to let the sound back up, in seconds
const LET_UP: f64 = 0.16;
/// how far a gain moves in one block, in dB (a block is about 3 ms), and a frequency, as a ratio
const STEP: f32 = 0.25;
const SLIDE: f32 = 1.06;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    LowCut,
    LowShelf,
    Peak,
    HighShelf,
    HighCut,
}

impl Kind {
    fn is_cut(self) -> bool {
        matches!(self, Kind::LowCut | Kind::HighCut)
    }
}

/// A biquad's five numbers, with a0 divided out.
#[derive(Clone, Copy)]
struct Coef {
    b: [f64; 3],
    a: [f64; 2],
}

/// RBJ's filter of a kind at a frequency, gain and width.
fn coef(kind: Kind, hz: f32, gain_db: f32, q: f32, rate: f64) -> Coef {
    let a = 10f64.powf(gain_db as f64 / 40.0);
    // a filter above 0.45 of the rate is held there (16 kHz at 32,000 Hz would be at Nyquist)
    let w = 2.0 * PI * (hz as f64).clamp(10.0, 0.45 * rate) / rate;
    let (cos, sin) = (w.cos(), w.sin());
    let alpha = sin / (2.0 * q.max(0.1) as f64);
    let (b, d) = match kind {
        Kind::Peak => ([1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a]),
        Kind::LowCut => ([(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0], [1.0 + alpha, -2.0 * cos, 1.0 - alpha]),
        Kind::HighCut => ([(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0], [1.0 + alpha, -2.0 * cos, 1.0 - alpha]),
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

/// How many dB a filter is at a frequency.
fn filter_db(c: &Coef, rate: f64, hz: f64) -> f64 {
    let w = 2.0 * PI * hz / rate;
    let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
    let top = (c.b[0] + c.b[1] * c1 + c.b[2] * c2).powi(2) + (c.b[1] * s1 + c.b[2] * s2).powi(2);
    let bottom = (1.0 + c.a[0] * c1 + c.a[1] * c2).powi(2) + (c.a[0] * s1 + c.a[1] * s2).powi(2);
    10.0 * (top / bottom).max(1e-30).log10()
}

fn band_kind(band: usize) -> Kind {
    match band {
        0 => Kind::LowShelf,
        b if b == BANDS - 1 => Kind::HighShelf,
        _ => Kind::Peak,
    }
}

/// A point of the curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub hz: f32,
    /// its gain in dB (a cut has none)
    pub db: f32,
    /// its width: higher is narrower (a cut's: a sharper corner)
    pub q: f32,
    /// None: by its place among the points
    pub kind: Option<Kind>,
}

impl Point {
    pub fn new(hz: f32, db: f32) -> Point {
        Point { hz, db, q: POINT_Q, kind: None }
    }
}

/// A whole curve: the ten bands' gains and the points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Curve {
    pub gains: [f32; BANDS],
    points: [Point; POINTS],
    count: usize,
}

impl Default for Curve {
    fn default() -> Curve {
        Curve { gains: [0.0; BANDS], points: [Point::new(1000.0, 0.0); POINTS], count: 0 }
    }
}

impl Curve {
    pub fn points(&self) -> &[Point] {
        &self.points[..self.count]
    }

    /// The kind of point `i`: its own, or by its place (the lowest of several a low shelf, the highest a
    /// high shelf, the others peaks).
    pub fn kind(&self, i: usize) -> Kind {
        let points = self.points();
        let hz = points[i].hz;
        points[i].kind.unwrap_or(match points.len() {
            n if n > 1 && points.iter().all(|p| p.hz >= hz) => Kind::LowShelf,
            n if n > 1 && points.iter().all(|p| p.hz <= hz) => Kind::HighShelf,
            _ => Kind::Peak,
        })
    }

    /// How many dB the curve is at a frequency.
    pub fn response_db(&self, rate: f64, hz: f64) -> f32 {
        let bands = self.gains.iter().enumerate().filter(|g| *g.1 != 0.0).map(|(band, gain)| filter_db(&coef(band_kind(band), CENTRES[band], *gain, BAND_Q, rate), rate, hz));
        let points = (0..self.count).filter(|i| self.kind(*i).is_cut() || self.points[*i].db != 0.0).map(|i| filter_db(&coef(self.kind(i), self.points[i].hz, self.points[i].db, self.points[i].q, rate), rate, hz));
        (bands.sum::<f64>() + points.sum::<f64>()) as f32
    }

    /// How far the whole sound is turned down so that the curve's highest point is at 0 dB: 0 or less.
    pub fn headroom(&self, rate: f64) -> f32 {
        // the curve, looked at every twelfth of an octave from 20 Hz up, and at every band and point
        let between = (0..121).map(|i| 20.0 * 2f64.powf(i as f64 / 12.0));
        let at = between.chain(CENTRES.iter().map(|c| *c as f64)).chain(self.points().iter().map(|p| p.hz as f64));
        -at.map(|hz| self.response_db(rate, hz.min(0.49 * rate))).fold(0.0f32, f32::max)
    }
}

/// A mode: a named set of the ten bands' gains.
#[derive(Clone)]
pub struct Mode {
    pub name: String,
    pub gains: [f32; BANDS],
}

fn read_modes(text: &str) -> Result<Vec<Mode>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (name, numbers) = line.split_once('=').ok_or(format!("modes.txt line {}: a mode is `Name = ten numbers`", n + 1))?;
        let given: Vec<f32> = numbers.split_whitespace().map(|g| g.parse::<f32>().map_err(|_| format!("modes.txt line {}: \"{g}\" is not a number", n + 1))).collect::<Result<_, _>>()?;
        if given.len() != BANDS {
            return Err(format!("modes.txt line {}: {} has {} gains, not {BANDS}", n + 1, name.trim(), given.len()));
        }
        out.push(Mode { name: name.trim().to_string(), gains: std::array::from_fn(|b| given[b].clamp(-MOST, MOST)) });
    }
    Ok(out)
}

/// The modes: from a `modes.txt` in the folder Bottle EQ is run from, else the ones built in. A line
/// is `Name = ten numbers`; `#` begins a remark.
pub fn modes() -> Result<Vec<Mode>, String> {
    read_modes(&std::fs::read_to_string("modes.txt").unwrap_or_else(|_| include_str!("../modes.txt").to_string()))
}

/// The mode asked for: its name, the beginning of it, or a part of it; capitals do not matter.
pub fn mode(wanted: &str) -> Result<Mode, String> {
    let all = modes()?;
    let key = |s: &str| s.to_lowercase().replace([' ', '-', '_'], "");
    let w = key(wanted);
    let found = all.iter().find(|m| key(&m.name) == w).or_else(|| all.iter().find(|m| key(&m.name).starts_with(&w))).or_else(|| all.iter().find(|m| key(&m.name).contains(&w)));
    found.cloned().ok_or(format!("no mode is called \"{wanted}\" (there are: {})", all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")))
}

/// The curve as it is set, by whoever sets it (a window, the keyboard); read by the audio thread.
pub struct Settings {
    gains: [AtomicU32; BANDS],
    /// each point's frequency, gain, width and kind (0: by its place)
    points: [[AtomicU32; 4]; POINTS],
    count: AtomicUsize,
    /// false: the sound passes untouched whatever is set
    pub on: AtomicBool,
    /// the volume in dB, and the most the limiter held the sound back since it was last asked, in dB
    volume: AtomicU32,
    limited: AtomicU32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            gains: std::array::from_fn(|_| AtomicU32::new(0f32.to_bits())),
            points: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))),
            count: AtomicUsize::new(0),
            on: AtomicBool::new(true),
            volume: AtomicU32::new(0f32.to_bits()),
            limited: AtomicU32::new(0f32.to_bits()),
        }
    }
}

const KINDS: [Kind; 5] = [Kind::LowCut, Kind::LowShelf, Kind::Peak, Kind::HighShelf, Kind::HighCut];

impl Settings {
    pub fn set(&self, band: usize, db: f32) {
        self.gains[band].store(db.clamp(-MOST, MOST).to_bits(), Ordering::Relaxed);
    }

    /// The volume in dB: 0 leaves the sound's level as it is.
    pub fn set_volume(&self, db: f32) {
        self.volume.store(db.clamp(QUIETEST, LOUDEST).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// The most the limiter held the sound back since this was last asked, in dB (0: it did not).
    pub fn limited(&self) -> f32 {
        f32::from_bits(self.limited.swap(0f32.to_bits(), Ordering::Relaxed))
    }

    pub fn set_all(&self, gains: &[f32; BANDS]) {
        gains.iter().enumerate().for_each(|(b, g)| self.set(b, *g));
    }

    /// The points of the curve (at most `POINTS`; more are left out).
    pub fn set_points(&self, points: &[Point]) {
        for (slot, p) in self.points.iter().zip(points) {
            slot[0].store(p.hz.clamp(10.0, 22000.0).to_bits(), Ordering::Relaxed);
            slot[1].store(p.db.clamp(-MOST, MOST).to_bits(), Ordering::Relaxed);
            slot[2].store(p.q.clamp(0.1, 20.0).to_bits(), Ordering::Relaxed);
            slot[3].store(p.kind.map_or(0, |k| 1 + KINDS.iter().position(|x| *x == k).unwrap() as u32), Ordering::Relaxed);
        }
        self.count.store(points.len().min(POINTS), Ordering::Relaxed);
    }

    pub fn get(&self) -> Curve {
        let f = |a: &AtomicU32| f32::from_bits(a.load(Ordering::Relaxed));
        Curve {
            gains: std::array::from_fn(|b| f(&self.gains[b])),
            points: std::array::from_fn(|i| {
                let slot = &self.points[i];
                Point { hz: f(&slot[0]), db: f(&slot[1]), q: f(&slot[2]), kind: slot[3].load(Ordering::Relaxed).checked_sub(1).map(|k| KINDS[k as usize % 5]) }
            }),
            count: self.count.load(Ordering::Relaxed).min(POINTS),
        }
    }

    /// The flat curve: every band at 0 dB, every point at 0 dB where it is, and no cut.
    pub fn flat(&self) {
        self.set_all(&[0.0; BANDS]);
        let flat: Vec<Point> = self.get().points().iter().map(|p| Point { db: 0.0, kind: p.kind.filter(|k| !k.is_cut()), ..*p }).collect();
        self.set_points(&flat);
    }
}

/// One biquad's memory: the last two samples in and out.
#[derive(Clone, Copy, Default)]
struct Memory {
    x: [f64; 2],
    y: [f64; 2],
}

/// A filter as the audio thread runs it: what it is now (on its way to what is set), and its numbers.
#[derive(Clone, Copy)]
struct Filter {
    kind: Kind,
    hz: f32,
    db: f32,
    q: f32,
    coef: Coef,
    /// in the sound: a filter at 0 dB that is not a cut is left out
    live: bool,
}

/// The equalizer as the audio thread runs it.
pub struct Eq {
    rate: f64,
    /// the ten bands, then the points
    filters: [Filter; BANDS + POINTS],
    /// the whole sound's gain now, in dB (the headroom and the volume, on their way), and the headroom
    /// of the curve last seen
    level: f32,
    seen: Curve,
    room: f32,
    /// the limiter: what the sound is held back to now (1: not at all)
    held: f64,
    /// memory[channel][filter]
    memory: Vec<[Memory; BANDS + POINTS]>,
}

impl Eq {
    pub fn new(rate: u32) -> Eq {
        let rate = rate as f64;
        let filters = std::array::from_fn(|i| {
            let (kind, hz, q) = if i < BANDS { (band_kind(i), CENTRES[i], BAND_Q) } else { (Kind::Peak, 1000.0, POINT_Q) };
            Filter { kind, hz, db: 0.0, q, coef: coef(kind, hz, 0.0, q, rate), live: false }
        });
        Eq { rate, filters, level: 0.0, seen: Curve::default(), room: 0.0, held: 1.0, memory: Vec::new() }
    }

    /// One block of interleaved frames, equalized in place toward the curve set.
    pub fn process(&mut self, sound: &mut [f32], channels: usize, settings: &Settings) {
        if self.memory.len() != channels {
            self.memory = vec![[Memory::default(); BANDS + POINTS]; channels];
        }
        let set = if settings.on.load(Ordering::Relaxed) { settings.get() } else { Curve::default() };
        for i in 0..BANDS + POINTS {
            // what this filter is set to be: (kind, frequency, gain, width), or nothing (a point not there)
            let want = match i.checked_sub(BANDS) {
                None => Some((band_kind(i), CENTRES[i], set.gains[i], BAND_Q)),
                Some(p) if p < set.count => Some((set.kind(p), set.points[p].hz, set.points[p].db, set.points[p].q)),
                Some(_) => None,
            };
            let f = &mut self.filters[i];
            let before = (f.kind, f.hz, f.db, f.q, f.live);
            match want {
                Some((kind, hz, db, q)) => {
                    if !f.live || f.kind != kind {
                        // a filter that comes in (or changes kind) begins at the set place, from nothing
                        (f.kind, f.hz, f.q, f.db) = (kind, hz, q, 0.0);
                        self.memory.iter_mut().for_each(|m| m[i] = Memory::default());
                    }
                    f.db += (db - f.db).clamp(-STEP, STEP);
                    f.hz *= (hz / f.hz).clamp(1.0 / SLIDE, SLIDE);
                    f.q *= (q / f.q).clamp(1.0 / SLIDE, SLIDE);
                    f.live = kind.is_cut() || f.db != 0.0;
                }
                // a point taken away goes down to nothing first
                None => {
                    f.db -= f.db.clamp(-STEP, STEP);
                    f.live = !f.kind.is_cut() && f.db != 0.0;
                }
            }
            if f.live && before != (f.kind, f.hz, f.db, f.q, f.live) {
                f.coef = coef(f.kind, f.hz, f.db, f.q, self.rate);
            }
        }
        if set != self.seen {
            (self.seen, self.room) = (set, set.headroom(self.rate));
        }
        let level_before = self.level;
        // the volume moves four times as fast as a curve's gain: it is a knob that is turned
        let aim = self.room + settings.volume();
        self.level += (aim - self.level).clamp(-4.0 * STEP, 4.0 * STEP);
        if self.filters.iter().all(|f| !f.live) && self.level == 0.0 && level_before == 0.0 && self.held == 1.0 {
            return; // flat: the sound is left exactly as it came
        }

        let frames = sound.len() / channels;
        let (from, to) = (10f64.powf(level_before as f64 / 20.0), 10f64.powf(self.level as f64 / 20.0));
        for (c, memory) in self.memory.iter_mut().enumerate() {
            for (i, frame) in sound.chunks_mut(channels).enumerate() {
                let mut s = frame[c] as f64;
                for (f, m) in self.filters.iter().zip(memory.iter_mut()).filter(|x| x.0.live) {
                    let k = &f.coef;
                    let y = k.b[0] * s + k.b[1] * m.x[0] + k.b[2] * m.x[1] - k.a[0] * m.y[0] - k.a[1] * m.y[1];
                    m.x = [s, m.x[0]];
                    m.y = [y, m.y[0]];
                    s = y;
                }
                // the level slides across the block
                frame[c] = (s * (from + (to - from) * (i + 1) as f64 / frames as f64)) as f32;
            }
        }
        // the limiter, when the volume is up over what the headroom allows (or it is still letting up)
        if self.level.max(level_before) > self.room || self.held < 1.0 {
            let up = 1.0 - (-1.0 / (LET_UP * self.rate)).exp();
            let mut least = 1.0f64;
            for frame in sound.chunks_mut(channels) {
                let peak = frame.iter().fold(0.0f32, |p, s| p.max(s.abs())) as f64;
                self.held += (1.0 - self.held) * up;
                if peak * self.held > 1.0 {
                    self.held = 1.0 / peak;
                }
                least = least.min(self.held);
                if self.held < 1.0 {
                    frame.iter_mut().for_each(|s| *s = (*s as f64 * self.held) as f32);
                }
            }
            if self.held > 0.999 {
                self.held = 1.0;
            }
            let back = (-20.0 * least.log10()) as f32;
            if back > 0.01 && back > f32::from_bits(settings.limited.load(Ordering::Relaxed)) {
                settings.limited.store(back.to_bits(), Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48000;
    const R: f64 = RATE as f64;

    fn sine(hz: f64, seconds: f64) -> Vec<f32> {
        (0..(seconds * R) as usize).map(|i| (0.25 * (2.0 * PI * hz * i as f64 / R).sin()) as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn settings(gains: &[f32; BANDS], points: &[Point]) -> Settings {
        let s = Settings::default();
        s.set_all(gains);
        s.set_points(points);
        s
    }

    fn bands(gains: [f32; BANDS]) -> Curve {
        Curve { gains, ..Curve::default() }
    }

    /// A sine through the equalizer once it has settled: how many dB louder it comes out, without the
    /// headroom.
    fn measured(set: &Settings, hz: f64) -> f64 {
        let mut eq = Eq::new(RATE);
        let mut sound = sine(hz, 3.0);
        let before = rms(&sound[RATE as usize * 2..]);
        sound.chunks_mut(144).for_each(|block| eq.process(block, 1, set));
        20.0 * (rms(&sound[RATE as usize * 2..]) / before).log10() - set.get().headroom(R) as f64
    }

    #[test]
    fn flat_leaves_the_sound_as_it_came() {
        // also with points standing at 0 dB
        let set = settings(&[0.0; BANDS], &[Point::new(80.0, 0.0), Point::new(1000.0, 0.0), Point::new(10000.0, 0.0)]);
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
        let curve = bands(gains);
        assert!((curve.response_db(R, 1000.0) - 6.0).abs() < 0.01);
        assert!(curve.response_db(R, 31.5).abs() < 0.1);
        assert!(curve.response_db(R, 16000.0).abs() < 0.1);
        // one octave wide: half the gain (in dB) half an octave to each side
        assert!((curve.response_db(R, 707.0) - 3.0).abs() < 0.3);
        assert!((curve.response_db(R, 1414.0) - 3.0).abs() < 0.3);
    }

    #[test]
    fn the_shelves_raise_the_ends() {
        let mut low = [0.0; BANDS];
        low[0] = 6.0;
        assert!((bands(low).response_db(R, 5.0) - 6.0).abs() < 0.1);
        assert!((bands(low).response_db(R, 31.5) - 3.0).abs() < 0.1);
        assert!(bands(low).response_db(R, 1000.0).abs() < 0.1);
        let mut high = [0.0; BANDS];
        high[BANDS - 1] = -6.0;
        assert!((bands(high).response_db(R, 23000.0) + 6.0).abs() < 0.3);
        assert!((bands(high).response_db(R, 16000.0) + 3.0).abs() < 0.1);
        assert!(bands(high).response_db(R, 1000.0).abs() < 0.1);
    }

    #[test]
    fn points_are_shelves_at_the_ends_and_peaks_between() {
        let curve = settings(&[0.0; BANDS], &[Point::new(5000.0, -4.0), Point::new(100.0, 6.0), Point::new(1000.0, 3.0)]).get();
        // (they may come in any order)
        assert_eq!((curve.kind(0), curve.kind(1), curve.kind(2)), (Kind::HighShelf, Kind::LowShelf, Kind::Peak));
        assert!((curve.response_db(R, 20.0) - 6.0).abs() < 0.5, "the low end is raised");
        assert!((curve.response_db(R, 20000.0) + 4.0).abs() < 0.5, "the high end is lowered");
        // a point alone is a peak, and exactly its gain where it stands
        let alone = settings(&[0.0; BANDS], &[Point::new(3000.0, 5.0)]).get();
        assert_eq!(alone.kind(0), Kind::Peak);
        assert!((alone.response_db(R, 3000.0) - 5.0).abs() < 0.01);
        assert!(alone.response_db(R, 100.0).abs() < 0.2);
    }

    #[test]
    fn a_cut_is_a_slope_and_a_narrow_peak_a_spike() {
        // the curve starts with a slope: 12 dB an octave below the low cut
        let cut = settings(&[0.0; BANDS], &[Point { hz: 200.0, db: 0.0, q: CUT_Q, kind: Some(Kind::LowCut) }]).get();
        assert!((cut.response_db(R, 200.0) + 3.0).abs() < 0.1);
        assert!((cut.response_db(R, 50.0) - cut.response_db(R, 25.0) - 12.0).abs() < 0.5);
        assert!(cut.response_db(R, 2000.0).abs() < 0.1);
        // a sharp corner stands up as a ridge before the slope
        let ridge = settings(&[0.0; BANDS], &[Point { hz: 200.0, db: 0.0, q: 3.0, kind: Some(Kind::LowCut) }]).get();
        assert!(ridge.response_db(R, 200.0) > 8.0);
        // a spike at the end: narrow, so an octave away it is nearly gone
        let spike = settings(&[0.0; BANDS], &[Point { hz: 12000.0, db: 9.0, q: 6.0, kind: Some(Kind::Peak) }]).get();
        assert!((spike.response_db(R, 12000.0) - 9.0).abs() < 0.01);
        assert!(spike.response_db(R, 6000.0) < 0.5);
        // a high cut mirrors the low cut
        let high = settings(&[0.0; BANDS], &[Point { hz: 4000.0, db: 0.0, q: CUT_Q, kind: Some(Kind::HighCut) }]).get();
        assert!((high.response_db(R, 4000.0) + 3.0).abs() < 0.1);
        assert!(high.response_db(R, 16000.0) < -20.0);
    }

    #[test]
    fn what_comes_out_is_what_the_curve_says() {
        let set = settings(
            &[4.0, -3.0, 6.0, 0.0, -6.0, 2.0, 0.0, 5.0, -4.0, 3.0],
            &[Point { hz: 70.0, db: 0.0, q: 1.2, kind: Some(Kind::LowCut) }, Point::new(700.0, -3.0), Point { hz: 11000.0, db: 6.0, q: 4.0, kind: Some(Kind::Peak) }],
        );
        for hz in [40.0, 100.0, 125.0, 300.0, 500.0, 1000.0, 3000.0, 4000.0, 9000.0, 11000.0, 15000.0] {
            let (said, heard) = (set.get().response_db(R, hz) as f64, measured(&set, hz));
            assert!((said - heard).abs() < 0.05, "{hz} Hz: the curve says {said:.2} dB, the sound is {heard:.2} dB");
        }
    }

    #[test]
    fn everything_at_its_most_stays_sound() {
        for most in [MOST, -MOST] {
            let points: Vec<Point> = (0..POINTS).map(|i| Point { hz: 40.0 * 2.3f32.powi(i as i32), db: most, q: 8.0, kind: None }).collect();
            let set = settings(&[most; BANDS], &points);
            let mut eq = Eq::new(RATE);
            let mut sound: Vec<f32> = (0..RATE * 2).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
            sound.chunks_mut(144).for_each(|block| eq.process(block, 1, &set));
            assert!(sound.iter().all(|s| s.is_finite()));
            assert!(sound[sound.len() - 100..].iter().all(|s| s.abs() < 1e-6), "the filters ring on");
        }
    }

    #[test]
    fn the_headroom_keeps_a_raised_curve_from_clipping() {
        let mut gains = [0.0; BANDS];
        gains[5] = 9.0;
        assert!((bands(gains).headroom(R) + 9.0).abs() < 0.05);
        assert_eq!(bands([-6.0; BANDS]).headroom(R), 0.0);
        // a full-scale sine at the raised band comes out at full scale, not over; and at a point's spike
        for (set, hz) in [(settings(&gains, &[]), 1000.0), (settings(&[0.0; BANDS], &[Point { hz: 5000.0, db: 12.0, q: 9.0, kind: Some(Kind::Peak) }]), 5000.0)] {
            let mut eq = Eq::new(RATE);
            let mut sound: Vec<f32> = (0..RATE * 2).map(|i| (2.0 * PI * hz * i as f64 / R).sin() as f32).collect();
            sound.chunks_mut(144).for_each(|block| eq.process(block, 1, &set));
            let peak = sound[RATE as usize..].iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!(peak <= 1.002 && peak > 0.99, "peak {peak} at {hz} Hz");
        }
    }

    #[test]
    fn the_volume_raises_the_sound_and_the_limiter_keeps_it_from_clipping() {
        // a quiet sine, 6 dB up: 6 dB louder, and the limiter has nothing to do
        let settings = Settings::default();
        settings.set_volume(6.0);
        let mut eq = Eq::new(RATE);
        let mut quiet = sine(1000.0, 1.0);
        let before = rms(&quiet[24000..]);
        quiet.chunks_mut(128).for_each(|block| eq.process(block, 1, &settings));
        assert!((20.0 * (rms(&quiet[24000..]) / before).log10() - 6.0).abs() < 0.01);
        assert_eq!(settings.limited(), 0.0);
        // a sine at full height, 12 dB up: no sample goes over, and it is said how far it was held back
        settings.set_volume(12.0);
        let mut loud: Vec<f32> = sine(1000.0, 1.0).iter().map(|s| 4.0 * s).collect();
        loud.chunks_mut(128).for_each(|block| eq.process(block, 1, &settings));
        assert!(loud.iter().all(|s| s.abs() <= 1.0), "the loudest sample is {}", loud.iter().fold(0.0f32, |p, s| p.max(s.abs())));
        let held = settings.limited();
        assert!((held - 12.0).abs() < 0.5 && settings.limited() == 0.0, "held back {held} dB");
        // and still a sine: held back evenly, not cut off at the top
        let top = loud[24000..].iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(top > 0.9 && (rms(&loud[24000..]) / top as f64 - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.02, "top {top}, rms {}", rms(&loud[24000..]));
        // back at 0 the sound passes exactly as it came again, once the limiter has let up
        settings.set_volume(0.0);
        let mut again = sine(440.0, 3.0);
        again.chunks_mut(128).for_each(|block| eq.process(block, 1, &settings));
        assert!(again[96000..] == sine(440.0, 3.0)[96000..], "two seconds after the volume is back at 0 the sound is not yet as it came");
    }

    #[test]
    fn a_curve_set_while_playing_does_not_click() {
        let set = Settings::default();
        let mut eq = Eq::new(RATE);
        let mut sound = sine(1000.0, 2.0);
        let slope = 0.25 * 2.0 * PI * 1000.0 / R; // the most a 1 kHz sine at this level moves in a frame
        let (mut last, mut jump) = (0.0f32, 0.0f64);
        for (n, block) in sound.chunks_mut(144).enumerate() {
            match n {
                100 => {
                    set.set(5, 12.0);
                    set.set(2, -12.0);
                }
                // points come, are dragged across the sine's own frequency, and go
                200 => set.set_points(&[Point::new(300.0, 8.0), Point::new(900.0, -9.0), Point::new(6000.0, 5.0)]),
                300 => set.set_points(&[Point::new(600.0, -6.0), Point::new(2500.0, 9.0), Point::new(6000.0, 5.0)]),
                400 => set.set_points(&[]),
                500 => set.flat(),
                _ => {}
            }
            eq.process(block, 1, &set);
            for s in block.iter() {
                jump = jump.max((*s - last).abs() as f64);
                last = *s;
            }
        }
        assert!(jump < slope * 4.5, "a jump of {jump:.4} (the sine alone: {slope:.4})");
    }

    #[test]
    fn the_modes_built_in_read_and_are_found_by_name() {
        let all = read_modes(include_str!("../modes.txt")).unwrap();
        assert_eq!(all.len(), 20);
        assert_eq!((all[0].name.as_str(), all[0].gains), ("Flat", [0.0; BANDS]));
        assert!(all.iter().all(|m| m.gains.iter().all(|g| g.abs() <= MOST)));
        // (mode() reads a modes.txt from the folder it is run in, which here is the project's own)
        assert_eq!(mode("bass boost").unwrap().name, "Bass Boost");
        assert_eq!(mode("hiphop").unwrap().name, "Hip-Hop");
        assert_eq!(mode("class").unwrap().name, "Classical");
        assert!(mode("polka").is_err());
    }

    #[test]
    fn flat_takes_the_cuts_away_and_keeps_the_points_where_they_are() {
        let set = settings(&[3.0; BANDS], &[Point { hz: 90.0, db: 0.0, q: CUT_Q, kind: Some(Kind::LowCut) }, Point::new(2000.0, 5.0)]);
        set.flat();
        let curve = set.get();
        assert_eq!(curve.gains, [0.0; BANDS]);
        assert_eq!(curve.points().iter().map(|p| (p.hz, p.db, p.kind)).collect::<Vec<_>>(), vec![(90.0, 0.0, None), (2000.0, 0.0, None)]);
        assert!((20..20000).step_by(500).all(|hz| curve.response_db(R, hz as f64) == 0.0));
    }
}
