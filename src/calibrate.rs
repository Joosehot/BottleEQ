//! Calibrating: a measured curve turned over into the equalizer's curve, so that what reaches the
//! listening place is flat; and calibrations kept by name.
//!
//! - **What is to be set right** is the speakers and the room alone: the measurement less the
//!   equalizer's curve the sweep went through (`raw`).
//! - **What is wanted** (`wanted`) is that turned over, read a third of an octave wide (a narrow dip is a
//!   place in the room, and a hand's width away it is elsewhere). It is never raised more than 6 dB (a
//!   deep hole is sound that cancels itself: more of it cancels as much, and costs the headroom) nor
//!   lowered more than 12. Where the measurement is not to be believed nothing is wanted: the curve
//!   stays at 0 dB there. Nor where the level lies more than 10 dB under the rest: that is past the end
//!   of what the speakers (or the microphone) can do, and no 6 dB would bring it back. Toward such an
//!   end what is wanted fades out over a third of an octave: the falling edge is not propped up.
//! - **The curve** (`fit`) is made of what the equalizer has: the ten bands' gains, then points (peaks
//!   put where the most is still missing, each with its frequency, gain and width), each number tried
//!   in turn until the curve lies on what is wanted.
//! - **Two ways of doing it** (`How`): `EXACT`, the mirror image of the measured curve as it is (a sixth
//!   of an octave wide, as it was measured; raised up to 12 dB; nothing faded; as many as 24 points),
//!   and `GENTLE`, the careful one described above (8 points). The exact one is flatter at the place
//!   that was measured and asks more of the speakers; the gentle one holds better a step to the side.
//! - **A calibration** is that curve with a name, the devices and the day, and the measurement it was
//!   made from: a file in the folder `calibrations` where Bottle EQ is run from.

use crate::eq::{Curve, Kind, Point, Settings, BANDS, MOST, POINTS};
use crate::measure::Measured;

/// How a measured curve is turned over.
pub struct How {
    /// how wide what is wanted is read: this many frequencies (24ths of an octave) to each side
    pub wide: usize,
    /// the most a frequency is raised, in dB
    pub up: f32,
    /// a level this far under the rest is past the end of what the speakers can do, in dB
    pub reach: f32,
    /// whether what is wanted fades out toward the ends of the reach
    pub fade: bool,
    /// the most points the curve is made of
    pub points: usize,
    /// a point is added while this much is still missing somewhere, in dB
    pub worth: f32,
    /// how near another a point may be put, in octaves, and the narrowest it may be (as 2 to this)
    pub apart: f32,
    pub narrowest: f32,
    /// every which of the frequencies the curve is fitted at
    pub every: usize,
}

/// The mirror image of what was measured, as it was measured.
pub const EXACT: How = How { wide: 0, up: MOST, reach: MOST, fade: false, points: POINTS, worth: 0.4, apart: 0.17, narrowest: 3.3, every: 1 };
/// The careful one: a third of an octave wide, raised 6 dB at the most, faded toward the ends, eight points.
pub const GENTLE: How = How { wide: 4, up: 6.0, reach: 10.0, fade: true, points: 8, worth: 0.75, apart: 0.33, narrowest: 2.3, every: 2 };
/// how much the curve is held at 0 dB where the measurement is not to be believed, against 1 where it is
const HOLD: f32 = 0.3;

/// The mean of the values that count.
fn mean(values: impl Iterator<Item = f32>) -> f32 {
    let (sum, n) = values.fold((0.0, 0), |(sum, n), v| (sum + v, n + 1));
    sum / n.max(1) as f32
}

/// The speakers and the room alone: the measurement less the equalizer's curve it went through, in dB
/// against its own average between 200 Hz and 5 kHz where it is certain.
pub fn raw(m: &Measured) -> Vec<f32> {
    let alone: Vec<f32> = m.db.iter().zip(&m.eq).map(|(db, eq)| db - eq).collect();
    let middle = (0..alone.len()).filter(|i| m.certain[*i] && (200.0..5000.0).contains(&m.hz[*i]));
    let all = (0..alone.len()).filter(|i| m.certain[*i]);
    let average = if middle.clone().count() > 0 { mean(middle.map(|i| alone[i])) } else { mean(all.map(|i| alone[i])) };
    alone.iter().map(|a| a - average).collect()
}

/// How far from flat a measurement is: the usual distance of its level from its mean, in dB, over the
/// frequencies that count.
pub fn spread(db: &[f32], counts: &[bool]) -> f32 {
    let average = mean((0..db.len()).filter(|i| counts[*i]).map(|i| db[i]));
    mean((0..db.len()).filter(|i| counts[*i]).map(|i| (db[i] - average).powi(2))).sqrt()
}

/// What is wanted of the equalizer, at each of the measurement's frequencies.
pub struct Wanted {
    /// the gain wanted, in dB
    pub db: Vec<f32>,
    /// how much each frequency counts: 1 where something is wanted, a little where the curve is only
    /// to stay at 0 dB
    pub counts: Vec<f32>,
    /// within reach: the measurement is certain there and the speakers have something to give
    pub reach: Vec<bool>,
}

/// What the equalizer should do to make what was measured flat.
pub fn wanted(m: &Measured, how: &How) -> Wanted {
    let alone = raw(m);
    // as wide as asked: so many places to each side (the frequencies are a 24th of an octave apart)
    let wide: Vec<f32> = (0..alone.len()).map(|i| mean((i.saturating_sub(how.wide)..(i + how.wide + 1).min(alone.len())).filter(|j| m.certain[*j]).map(|j| alone[j]))).collect();
    // (`raw` is against the average of the middle, so "the rest" is 0 dB)
    // the reach is from the first to the last third of an octave (eight places running) that is certain
    // and not too far under the rest; a hole between them is within it
    let up: Vec<bool> = (0..wide.len()).map(|i| m.certain[i] && wide[i] > -how.reach).collect();
    let runs: Vec<usize> = (0..wide.len().saturating_sub(7)).filter(|i| up[*i..*i + 8].iter().all(|u| *u)).collect();
    let (first, last) = (runs.first().copied().unwrap_or(usize::MAX), runs.last().map_or(0, |i| i + 7));
    let reach: Vec<bool> = (0..wide.len()).map(|i| m.certain[i] && i >= first && i <= last).collect();
    // what is within reach is turned over about its own mean, so that as much is raised as lowered
    let level = mean((0..wide.len()).filter(|i| reach[*i]).map(|i| wide[i]));
    // fading out over a third of an octave (eight places) toward where the reach ends
    let near_end = |i: usize| if how.fade { (1..=8).find(|d| !reach.get(i + d).copied().unwrap_or(true) || (i >= *d && !reach[i - d])).map_or(1.0, |d| d as f32 / 9.0) } else { 1.0 };
    let db = (0..wide.len()).map(|i| if reach[i] { (level - wide[i]).clamp(-MOST, how.up) * near_end(i) } else { 0.0 }).collect();
    Wanted { db, counts: reach.iter().map(|r| if *r { 1.0 } else { HOLD }).collect(), reach }
}

/// A curve of the equalizer's.
fn curve(gains: &[f32; BANDS], points: &[Point]) -> Curve {
    let settings = Settings::default();
    settings.set_all(gains);
    settings.set_points(points);
    settings.get()
}

/// The lowest place of a valley between two ends, found by narrowing in on it.
fn lowest(mut from: f32, mut to: f32, mut height: impl FnMut(f32) -> f32) -> f32 {
    const GOLD: f32 = 0.618034;
    let (mut a, mut b) = (to - GOLD * (to - from), from + GOLD * (to - from));
    let (mut at_a, mut at_b) = (height(a), height(b));
    for _ in 0..14 {
        if at_a < at_b {
            (to, b, at_b) = (b, a, at_a);
            a = to - GOLD * (to - from);
            at_a = height(a);
        } else {
            (from, a, at_a) = (a, b, at_b);
            b = from + GOLD * (to - from);
            at_b = height(b);
        }
    }
    (from + to) / 2.0
}

/// The curve fitted to what is wanted.
pub struct Fitted {
    pub gains: [f32; BANDS],
    pub points: Vec<Point>,
    /// how far the curve is from what is wanted where it counts fully, in dB: the usual and the most
    pub off: f32,
    pub most_off: f32,
}

/// The equalizer's curve that lies nearest to what is wanted: the ten bands, then up to eight points.
pub fn fit(hz: &[f32], want: &[f32], counts: &[f32], rate: f64, how: &How) -> Fitted {
    let looked: Vec<usize> = (0..hz.len()).step_by(how.every).collect();
    let missing = |gains: &[f32; BANDS], points: &[Point]| -> f32 {
        let c = curve(gains, points);
        looked.iter().map(|i| counts[*i] * (c.response_db(rate, hz[*i] as f64) - want[*i]).powi(2)).sum()
    };
    let mut gains = [0.0f32; BANDS];
    let mut points: Vec<Point> = Vec::new();
    let bands = |gains: &mut [f32; BANDS], points: &[Point], rounds: usize| {
        for _ in 0..rounds {
            for band in 0..BANDS {
                gains[band] = lowest(-MOST, MOST, |db| {
                    let mut tried = *gains;
                    tried[band] = db;
                    missing(&tried, points)
                });
            }
        }
    };
    bands(&mut gains, &points, 3);
    let with = |point: Point, points: &[Point]| [points, &[point]].concat();
    // a point's gain, width and frequency, each tried in turn with the others held (the width and the
    // frequency by ratio: from a third to three times, and a third of an octave each way)
    let tune = |point: &mut Point, gains: &[f32; BANDS], others: &[Point]| {
        for _ in 0..2 {
            point.db = lowest(-MOST, MOST, |db| missing(gains, &with(Point { db, ..*point }, others)));
            let q = point.q.log2();
            point.q = 2f32.powf(lowest((q - 1.6).max(-1.5), (q + 1.6).min(how.narrowest), |q| missing(gains, &with(Point { q: 2f32.powf(q), ..*point }, others))));
            point.hz *= 2f32.powf(lowest(-0.33, 0.33, |up| missing(gains, &with(Point { hz: point.hz * 2f32.powf(up), ..*point }, others))));
        }
    };
    // places where a point was tried and did not help: not tried again
    let mut no_use: Vec<f32> = Vec::new();
    while points.len() < how.points && no_use.len() < 6 + how.points / 2 {
        // where the most is still missing
        let c = curve(&gains, &points);
        let still: Vec<f32> = (0..hz.len()).map(|i| counts[i] * (want[i] - c.response_db(rate, hz[i] as f64))).collect();
        // (nor where a point already is: two at one place only fight over it)
        let open = |i: &usize| no_use.iter().chain(points.iter().map(|p| &p.hz)).all(|tried| (hz[*i] / tried).log2().abs() > how.apart);
        let Some(worst) = (0..hz.len()).filter(open).max_by(|a, b| still[*a].abs().total_cmp(&still[*b].abs())) else { break };
        if still[worst].abs() < how.worth {
            break;
        }
        // as wide as the stretch around it where at least half as much is missing, on the same side
        let part = |i: usize| still[i] * still[worst].signum() > 0.5 * still[worst].abs();
        let from = (0..worst).rev().find(|i| !part(*i)).map_or(0, |i| i + 1);
        let to = (worst..hz.len()).find(|i| !part(*i)).unwrap_or(hz.len());
        let octaves = (hz[to - 1] / hz[from]).log2().max(1.0 / 12.0);
        let q = (2f32.powf(octaves).sqrt() / (2f32.powf(octaves) - 1.0)).clamp(0.4, 2f32.powf(how.narrowest));
        let before = missing(&gains, &points);
        let mut point = Point { hz: (hz[from] * hz[to - 1]).sqrt(), db: still[worst].clamp(-MOST, MOST), q, kind: Some(Kind::Peak) };
        tune(&mut point, &gains, &points);
        // a point that hardly helps is not worth having
        if missing(&gains, &with(point, &points)) > 0.985 * before {
            no_use.push(hz[worst]);
            continue;
        }
        points.push(point);
        bands(&mut gains, &points, 1);
    }
    // and once more over everything, now that all of it is there
    for i in 0..points.len() {
        let mut point = points.remove(i);
        tune(&mut point, &gains, &points);
        points.insert(i, point);
    }
    bands(&mut gains, &points, 1);
    let c = curve(&gains, &points);
    let full: Vec<f32> = (0..hz.len()).filter(|i| counts[*i] >= 1.0).map(|i| (c.response_db(rate, hz[i] as f64) - want[i]).abs()).collect();
    Fitted { gains, points, off: mean(full.iter().copied()), most_off: full.iter().copied().fold(0.0, f32::max) }
}

/// A calibration: a correction curve by name, with what it was made for and from.
#[derive(Clone)]
pub struct Calibration {
    pub name: String,
    /// the kind of device: "speakers" or "headphones" (what the guesser learns it as)
    pub kind: String,
    /// the speakers (or headphones) and the microphone it was measured with, and the day
    pub device: String,
    pub microphone: String,
    pub day: String,
    pub gains: [f32; BANDS],
    pub points: Vec<Point>,
    /// the measurement it was made from: the speakers and the room alone, and where it is certain
    pub hz: Vec<f32>,
    pub raw: Vec<f32>,
    pub certain: Vec<bool>,
}

const FOLDER: &str = "calibrations";
const KINDS: [(&str, Kind); 5] = [("lowcut", Kind::LowCut), ("lowshelf", Kind::LowShelf), ("peak", Kind::Peak), ("highshelf", Kind::HighShelf), ("highcut", Kind::HighCut)];

/// Today as year-month-day (by the clock of the world, not the wall's).
pub fn today() -> String {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86400) as i64;
    // days since 1970 into a date (Howard Hinnant's civil_from_days)
    let z = days + 719468;
    let (era, of_era) = (z.div_euclid(146097), z.rem_euclid(146097));
    let year_of_era = (of_era - of_era / 1460 + of_era / 36524 - of_era / 146096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let m = (5 * day_of_year + 2) / 153;
    let (day, month) = (day_of_year - (153 * m + 2) / 5 + 1, if m < 10 { m + 3 } else { m - 9 });
    format!("{:04}-{:02}-{:02}", year_of_era + era * 400 + (month <= 2) as i64, month, day)
}

/// The file a calibration of a name is kept in.
fn file(name: &str) -> String {
    let plain: String = name.trim().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{FOLDER}/{plain}.txt")
}

impl Calibration {
    /// As it is written in its file: a line for each thing, `what = it`.
    pub fn written(&self) -> String {
        let mut out = format!("name = {}\ntype = {}\ndevice = {}\nmicrophone = {}\nday = {}\n", self.name, self.kind, self.device, self.microphone, self.day);
        out += &format!("bands = {}\n", self.gains.iter().map(|g| format!("{g:.2}")).collect::<Vec<_>>().join(" "));
        for (i, p) in self.points.iter().enumerate() {
            let kind = p.kind.or(Some(curve(&self.gains, &self.points).kind(i))).and_then(|k| KINDS.iter().find(|(_, kind)| *kind == k)).map_or("peak", |k| k.0);
            out += &format!("point = {:.1} {:.2} {:.3} {kind}\n", p.hz, p.db, p.q);
        }
        out += "# the measurement it was made from: frequency (Hz), the device and the room alone (dB), measured and believed (1) or not (0)\n";
        for i in 0..self.hz.len() {
            out += &format!("measured = {:.2} {:.2} {}\n", self.hz[i], self.raw[i], self.certain[i] as u8);
        }
        out
    }

    /// Read back from what `written` wrote.
    pub fn read(text: &str) -> Result<Calibration, String> {
        let mut c = Calibration { name: String::new(), kind: "speakers".to_string(), device: String::new(), microphone: String::new(), day: String::new(), gains: [0.0; BANDS], points: Vec::new(), hz: Vec::new(), raw: Vec::new(), certain: Vec::new() };
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((what, it)) = line.split_once('=') else { continue };
            let it = it.trim();
            let wrong = || format!("line {}: \"{line}\" is not understood", n + 1);
            let numbers = |words: &[&str]| -> Result<Vec<f32>, String> { words.iter().map(|w| w.parse::<f32>().map_err(|_| wrong())).collect() };
            let words: Vec<&str> = it.split_whitespace().collect();
            match what.trim() {
                "name" => c.name = it.to_string(),
                "type" => c.kind = it.to_string(),
                "device" => c.device = it.to_string(),
                "microphone" => c.microphone = it.to_string(),
                "day" => c.day = it.to_string(),
                "bands" => {
                    let given = numbers(&words)?;
                    if given.len() != BANDS {
                        return Err(wrong());
                    }
                    c.gains = std::array::from_fn(|b| given[b]);
                }
                "point" if words.len() == 4 => {
                    let n = numbers(&words[..3])?;
                    let kind = KINDS.iter().find(|k| k.0 == words[3]).ok_or_else(wrong)?.1;
                    c.points.push(Point { hz: n[0], db: n[1], q: n[2], kind: Some(kind) });
                }
                "measured" if words.len() == 3 => {
                    let n = numbers(&words)?;
                    c.hz.push(n[0]);
                    c.raw.push(n[1]);
                    c.certain.push(n[2] != 0.0);
                }
                _ => return Err(wrong()),
            }
        }
        if c.name.is_empty() {
            return Err("it has no name".to_string());
        }
        Ok(c)
    }

    /// Keep it in its file; a calibration of the same name is written over.
    pub fn save(&self) -> Result<String, String> {
        let path = file(&self.name);
        std::fs::create_dir_all(FOLDER).and_then(|_| std::fs::write(&path, self.written())).map_err(|e| format!("{path}: {e}"))?;
        Ok(path)
    }

    /// Set the equalizer to it.
    pub fn set(&self, settings: &Settings) {
        settings.set_all(&self.gains);
        settings.set_points(&self.points);
    }
}

/// The calibrations kept, by name.
pub fn calibrations() -> Vec<Calibration> {
    let Ok(folder) = std::fs::read_dir(FOLDER) else { return Vec::new() };
    let mut all: Vec<Calibration> = folder.flatten().filter_map(|f| std::fs::read_to_string(f.path()).ok()).filter_map(|text| Calibration::read(&text).ok()).collect();
    all.sort_by_key(|c| c.name.to_lowercase());
    all
}

/// The calibration asked for: its name, the beginning of it, or a part of it; capitals do not matter.
pub fn calibration(wanted: &str) -> Result<Calibration, String> {
    let all = calibrations();
    let w = wanted.to_lowercase();
    let found = all.iter().find(|c| c.name.to_lowercase() == w).or_else(|| all.iter().find(|c| c.name.to_lowercase().starts_with(&w))).or_else(|| all.iter().find(|c| c.name.to_lowercase().contains(&w)));
    found.cloned().ok_or(match all.is_empty() {
        true => format!("no calibration is called \"{wanted}\": none is kept yet (measure, correct and save one)"),
        false => format!("no calibration is called \"{wanted}\" (there are: {})", all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A measurement of speakers whose response is a curve of the equalizer's, certain from `from` Hz up.
    fn measured(speakers: &Curve, from: f32) -> Measured {
        let hz: Vec<f32> = (0..240).map(|i| 20.0 * 2f32.powf(i as f32 / 24.0)).collect();
        let db: Vec<f32> = hz.iter().map(|hz| speakers.response_db(48000.0, *hz as f64)).collect();
        let certain: Vec<bool> = hz.iter().map(|hz| *hz >= from).collect();
        let n = hz.len();
        Measured { hz, db, certain, off_level: Vec::new(), eq: vec![0.0; n], guessed: Vec::new(), over_noise: vec![40.0; n], peak: 0.5, arrived: 1.2, level: -20.0 }
    }

    /// Speakers with a hump in the bass, a dip in the middle and a falling treble.
    fn speakers() -> Curve {
        curve(&[0.0, 0.0, 5.0, 2.0, 0.0, -1.0, 0.0, 0.0, -3.0, -6.0], &[Point { hz: 1800.0, db: -5.0, q: 2.5, kind: Some(Kind::Peak) }, Point { hz: 300.0, db: 4.0, q: 3.0, kind: Some(Kind::Peak) }])
    }

    #[test]
    fn the_correction_makes_the_speakers_flat() {
        let m = measured(&speakers(), 20.0);
        let want = wanted(&m, &GENTLE);
        let fitted = fit(&m.hz, &want.db, &want.counts, 48000.0, &GENTLE);
        assert!(fitted.off < 0.5 && fitted.most_off < 2.0, "the curve is {} dB off what is wanted, {} at the most", fitted.off, fitted.most_off);
        // the speakers through the correction: flat where they were not
        let correction = curve(&fitted.gains, &fitted.points);
        let after: Vec<f32> = (0..m.hz.len()).map(|i| m.db[i] + correction.response_db(48000.0, m.hz[i] as f64)).collect();
        let heard: Vec<bool> = m.hz.iter().map(|hz| (40.0..16000.0).contains(hz)).collect();
        let (before, after) = (spread(&m.db, &heard), spread(&after, &heard));
        assert!(before > 2.5 && after < 0.6, "from {before} dB to {after} dB from flat");
    }

    #[test]
    fn nothing_is_done_where_the_measurement_is_not_believed_and_a_hole_is_not_filled() {
        // speakers with nothing below 100 Hz (not certain there) and a hole 20 dB deep at 3 kHz
        let small = curve(&[0.0; BANDS], &[Point { hz: 3000.0, db: -12.0, q: 4.0, kind: Some(Kind::Peak) }, Point { hz: 3000.0, db: -8.0, q: 4.0, kind: Some(Kind::Peak) }]);
        let m = measured(&small, 100.0);
        let want = wanted(&m, &GENTLE);
        assert!(want.db.iter().all(|w| *w <= GENTLE.up + 0.01), "nothing is raised more than {} dB", GENTLE.up);
        assert!((0..m.hz.len()).all(|i| m.certain[i] || want.db[i] == 0.0));
        let fitted = fit(&m.hz, &want.db, &want.counts, 48000.0, &GENTLE);
        let correction = curve(&fitted.gains, &fitted.points);
        assert!(correction.response_db(48000.0, 40.0).abs() < 1.0, "at 40 Hz the correction is {} dB", correction.response_db(48000.0, 40.0));
        let at_hole = correction.response_db(48000.0, 3000.0);
        assert!(at_hole > 2.0 && at_hole < GENTLE.up + 1.0, "at the hole the correction is {at_hole} dB");
    }

    #[test]
    fn what_the_speakers_cannot_do_is_left_alone() {
        // speakers (or a microphone) with nothing above 9 kHz: measured, certain, and 30 dB down
        let cut = Point { kind: Some(Kind::HighCut), q: 0.7, ..Point::new(9000.0, 0.0) };
        let m = measured(&curve(&[0.0, 0.0, 4.0, 0.0, 0.0, -3.0, 0.0, 0.0, 0.0, 0.0], &[cut, cut, cut]), 20.0);
        let want = wanted(&m, &GENTLE);
        let at = |hz: f32| (0..m.hz.len()).find(|i| m.hz[*i] >= hz).unwrap();
        assert!(!want.reach[at(16000.0)] && want.db[at(16000.0)] == 0.0 && want.reach[at(1000.0)]);
        let fitted = fit(&m.hz, &want.db, &want.counts, 48000.0, &GENTLE);
        let correction = curve(&fitted.gains, &fitted.points);
        assert!(correction.response_db(48000.0, 16000.0).abs() < 1.5, "at 16 kHz the correction is {} dB", correction.response_db(48000.0, 16000.0));
        // and where the speakers are whole (up to 5 kHz) it is flat
        let after: Vec<f32> = (0..m.hz.len()).map(|i| m.db[i] + correction.response_db(48000.0, m.hz[i] as f64)).collect();
        let whole: Vec<bool> = m.hz.iter().map(|hz| (30.0..5000.0).contains(hz)).collect();
        assert!(spread(&m.db, &whole) > 1.5 && spread(&after, &whole) < 0.6, "from {} to {} dB from flat up to 5 kHz", spread(&m.db, &whole), spread(&after, &whole));
    }

    #[test]
    fn the_exact_mirror_follows_what_the_gentle_one_leaves() {
        // speakers and a room: the speakers' broad shape, and on it narrow ups and downs every third
        // of an octave or so, and a hole 9 dB deep
        let broad = speakers();
        let mut m = measured(&broad, 20.0);
        for i in 0..m.hz.len() {
            let octaves = (m.hz[i] / 20.0).log2();
            m.db[i] += 2.5 * (octaves * 9.0).sin() + 1.5 * (octaves * 5.3 + 1.0).sin() - 9.0 * (-((m.hz[i] / 700.0).log2().powi(2)) / 0.004).exp();
        }
        let heard: Vec<bool> = m.hz.iter().map(|hz| (40.0..16000.0).contains(hz)).collect();
        let after = |how: &How| {
            let want = wanted(&m, how);
            let fitted = fit(&m.hz, &want.db, &want.counts, 48000.0, how);
            let correction = curve(&fitted.gains, &fitted.points);
            let with: Vec<f32> = (0..m.hz.len()).map(|i| m.db[i] + correction.response_db(48000.0, m.hz[i] as f64)).collect();
            (spread(&with, &heard), fitted.points.len(), correction.response_db(48000.0, 700.0))
        };
        let (gentle, exact) = (after(&GENTLE), after(&EXACT));
        assert!(spread(&m.db, &heard) > 3.0);
        assert!(exact.0 < 0.8 && exact.0 < 0.6 * gentle.0, "from flat: {} dB with the exact mirror, {} dB with the gentle one", exact.0, gentle.0);
        assert!(exact.1 > 8 && gentle.1 <= 8, "{} points in the exact mirror, {} in the gentle", exact.1, gentle.1);
        // the narrow hole: the gentle one reads over it, the exact one goes into it
        assert!(exact.2 > gentle.2 + 2.0, "at the hole: {} dB exact, {} dB gentle", exact.2, gentle.2);
    }

    #[test]
    fn the_equalizer_the_sweep_went_through_is_taken_off() {
        // measured through a curve that is already set: what is to be set right is still the speakers alone
        let mut m = measured(&speakers(), 20.0);
        let alone = raw(&m);
        let set = curve(&[3.0, 0.0, -2.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 1.0], &[]);
        m.eq = m.hz.iter().map(|hz| set.response_db(48000.0, *hz as f64)).collect();
        m.db = m.db.iter().zip(&m.eq).map(|(db, eq)| db + eq).collect();
        assert!(raw(&m).iter().zip(&alone).all(|(a, b)| (a - b).abs() < 0.01));
    }

    #[test]
    fn a_calibration_is_read_back_as_it_was_written() {
        let m = measured(&speakers(), 60.0);
        let want = wanted(&m, &GENTLE);
        let fitted = fit(&m.hz, &want.db, &want.counts, 48000.0, &GENTLE);
        let c = Calibration { name: "Speakers at the desk".to_string(), kind: "headphones".to_string(), device: "Speakers (USB2.0 Device)".to_string(), microphone: "Microphone".to_string(), day: today(), gains: fitted.gains, points: fitted.points, hz: m.hz.clone(), raw: raw(&m), certain: m.certain.clone() };
        let back = Calibration::read(&c.written()).unwrap();
        assert_eq!((back.name.as_str(), back.kind.as_str(), back.device.as_str(), back.day.as_str()), (c.name.as_str(), "headphones", c.device.as_str(), c.day.as_str()));
        assert!(back.gains.iter().zip(&c.gains).all(|(a, b)| (a - b).abs() < 0.01));
        assert_eq!(back.points.len(), c.points.len());
        assert!(back.points.iter().zip(&c.points).all(|(a, b)| (a.hz / b.hz - 1.0).abs() < 0.01 && (a.db - b.db).abs() < 0.01 && a.kind == b.kind));
        assert_eq!(back.certain, c.certain);
        assert_eq!(file("Speakers at the desk"), "calibrations/Speakers_at_the_desk.txt");
        // the day is a date
        assert!(today().len() == 10 && today().starts_with("20"));
    }
}
