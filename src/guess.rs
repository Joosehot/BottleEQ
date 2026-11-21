//! The guesser: a whole curve from a part of one.
//!
//! A short measurement (one sweep, in a noisy room) is believed at some frequencies and not at others.
//! The guesser fills in the others from what curves of the same kind of device look like:
//!
//! - **What it learns from** (`training`): curves kept in the folder `training/<kind>/` (brought in
//!   with `import` from a database of measured curves, such as AutoEq's results), and the measurements
//!   of the calibrations kept for that kind, at the frequencies that were measured and believed there.
//! - **What it learns** (`learn`): the average curve of the kind, and the few ways in which the curves
//!   of the kind most differ from it (their principal components: "more bass", "a dip at 3 kHz" ...),
//!   each with how much it usually varies.
//! - **How it guesses** (`guess`): the average, moved up or down as a whole and along those few ways as
//!   far as the frequencies that were measured ask for, held back toward the average (a ridge
//!   regression). How hard it is held back is learned too: a curve has much of its own that the few
//!   ways do not hold, and followed too closely the ways are bent to fit that and go wrong where
//!   nothing was measured (with 736 headphones and no holding back the guess was worse than the
//!   average alone). So when a kind is learned, some of its curves are hidden in part and guessed with
//!   each of a row of holds, and the hold that guesses them best is kept. Where nothing was measured the curve is that; and so that it meets what was
//!   measured without a step, what the measured part still differs by at its edge is carried on into
//!   the guessed part and fades out over an octave.
//!
//! What is guessed is marked as guessed, to be drawn apart from what was measured. With no curve of the
//! kind to learn from, the guess is only the measured part's level carried on.

use crate::calibrate;
use crate::measure::Measured;

/// the kinds of device that are told apart
pub const KINDS: [&str; 2] = ["speakers", "headphones"];
const FOLDER: &str = "training";
/// the most ways a kind's curves are let differ in
const WAYS: usize = 8;
/// how hard the guess is held to the kind's average rather than to the frequencies measured: the
/// values tried when a kind is learned (the one that guesses the kind's own curves best is kept), and
/// the one used when there are too few curves to try on
const HOLDS: [f32; 9] = [1.0, 10.0, 30.0, 100.0, 300.0, 1000.0, 3000.0, 10000.0, 100000.0];
const HOLD: f32 = 300.0;
/// what a measured part still differs by at its edge fades to half over this many frequencies (half an octave)
const FADE: f32 = 12.0;

/// The frequencies every curve is kept at: every 24th of an octave from 20 Hz to 20 kHz, as a
/// measurement has them.
pub fn grid() -> Vec<f32> {
    (0..).map(|i| 20.0 * 2f32.powf(i as f32 / 24.0)).take_while(|hz| *hz <= 20000.0).collect()
}

/// A curve to learn from, at the grid's frequencies: its levels in dB against its own average between
/// 200 Hz and 5 kHz, and which of them are known.
#[derive(Clone)]
pub struct Known {
    pub name: String,
    pub db: Vec<f32>,
    pub known: Vec<bool>,
}

/// Set a curve against its own average between 200 Hz and 5 kHz, over what is known of it there (or
/// anywhere, if nothing is).
fn levelled(hz: &[f32], db: &mut [f32], known: &[bool]) {
    let middle: Vec<f32> = (0..db.len()).filter(|i| known[*i] && (200.0..5000.0).contains(&hz[*i])).map(|i| db[i]).collect();
    let all: Vec<f32> = (0..db.len()).filter(|i| known[*i]).map(|i| db[i]).collect();
    let of = if middle.is_empty() { &all } else { &middle };
    let average = of.iter().sum::<f32>() / of.len().max(1) as f32;
    db.iter_mut().for_each(|d| *d -= average);
}

/// A table of numbers as written in a file: (frequency, level) from each line that has them. In a
/// file with a line of names first (AutoEq's results: frequency, raw, error ...), the level is the
/// column "error" (how far from its target the device is) if there is one, else "raw"; in a file
/// without one, the second number. Lines that do not begin with a number are left out.
pub fn read_curve(text: &str) -> Vec<(f32, f32)> {
    let cells = |line: &str| -> Vec<String> { line.split([',', ';', '\t', ' ']).filter(|c| !c.is_empty()).map(|c| c.trim().to_lowercase()).collect() };
    let names = text.lines().map(cells).find(|c| c.iter().any(|n| n == "frequency"));
    let column = names.as_ref().and_then(|n| n.iter().position(|c| c == "error").or(n.iter().position(|c| c == "raw"))).unwrap_or(1);
    let at = names.as_ref().and_then(|n| n.iter().position(|c| c == "frequency")).unwrap_or(0);
    let rows = text.lines().map(cells).filter_map(|c| Some((c.get(at)?.parse::<f32>().ok()?, c.get(column)?.parse::<f32>().ok()?)));
    let mut points: Vec<(f32, f32)> = rows.filter(|(hz, db)| *hz > 0.0 && db.is_finite()).collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points
}

/// A curve given at its own frequencies, read at the grid's (on a straight line between its points,
/// along the octaves); outside what it covers it is not known.
pub fn onto_grid(name: &str, points: &[(f32, f32)]) -> Option<Known> {
    if points.len() < 2 {
        return None;
    }
    let hz = grid();
    let (first, last) = (points.first()?.0, points.last()?.0);
    let known: Vec<bool> = hz.iter().map(|hz| (first..=last).contains(hz)).collect();
    let mut db: Vec<f32> = hz
        .iter()
        .map(|hz| {
            let after = points.partition_point(|p| p.0 < *hz).clamp(1, points.len() - 1);
            let (a, b) = (points[after - 1], points[after]);
            let part = if b.0 > a.0 { ((hz / a.0).ln() / (b.0 / a.0).ln()).clamp(0.0, 1.0) } else { 0.0 };
            a.1 + (b.1 - a.1) * part
        })
        .collect();
    if known.iter().filter(|k| **k).count() < 24 {
        return None; // less than an octave of it: not a curve
    }
    levelled(&hz, &mut db, &known);
    Some(Known { name: name.to_string(), db, known })
}

fn plain(name: &str) -> String {
    name.trim().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect()
}

/// As a curve is written in its file in the training folder.
fn written(curve: &Known) -> String {
    let hz = grid();
    let rows: Vec<String> = (0..hz.len()).filter(|i| curve.known[*i]).map(|i| format!("{:.2},{:.2}", hz[i], curve.db[i])).collect();
    format!("# {}\nfrequency,raw\n{}\n", curve.name, rows.join("\n"))
}

/// Bring curves in to learn from: a file, or every .csv and .txt file under a folder. Each is kept
/// under its file's name in `training/<kind>/`. Returns how many were brought in and how many files
/// held no curve.
pub fn import(path: &str, kind: &str) -> Result<(usize, usize), String> {
    fn files(path: &std::path::Path, into: &mut Vec<std::path::PathBuf>) {
        match std::fs::read_dir(path) {
            Ok(folder) => folder.flatten().for_each(|f| files(&f.path(), into)),
            Err(_) => {
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("txt")) {
                    into.push(path.to_path_buf());
                }
            }
        }
    }
    let mut found = Vec::new();
    files(std::path::Path::new(path), &mut found);
    if found.is_empty() {
        return Err(format!("{path}: no .csv or .txt file there"));
    }
    let folder = format!("{FOLDER}/{}", plain(kind));
    std::fs::create_dir_all(&folder).map_err(|e| format!("{folder}: {e}"))?;
    let (mut taken, mut left) = (0, 0);
    for file in found {
        let name = file.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::read_to_string(&file).ok().and_then(|text| onto_grid(&name, &read_curve(&text))) {
            Some(curve) => {
                let to = format!("{folder}/{}.csv", plain(&name));
                std::fs::write(&to, written(&curve)).map_err(|e| format!("{to}: {e}"))?;
                taken += 1;
            }
            None => left += 1,
        }
    }
    Ok((taken, left))
}

/// The curves there are to learn a kind from: those brought in, and the measurements of the
/// calibrations kept for it (at the frequencies measured and believed).
pub fn training(kind: &str) -> Vec<Known> {
    let mut all = Vec::new();
    if let Ok(folder) = std::fs::read_dir(format!("{FOLDER}/{}", plain(kind))) {
        let mut files: Vec<_> = folder.flatten().map(|f| f.path()).collect();
        files.sort();
        for file in files {
            let name = file.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            all.extend(std::fs::read_to_string(&file).ok().and_then(|text| onto_grid(&name, &read_curve(&text))));
        }
    }
    let hz = grid();
    for c in calibrate::calibrations().into_iter().filter(|c| c.kind == kind && c.hz.len() <= hz.len() && c.certain.iter().any(|c| *c)) {
        // (a calibration measured at 44,100 Hz ends a little under 20 kHz: the rest is not known)
        let known: Vec<bool> = (0..hz.len()).map(|i| c.certain.get(i).copied().unwrap_or(false)).collect();
        let mut db: Vec<f32> = (0..hz.len()).map(|i| c.raw.get(i).copied().unwrap_or(0.0)).collect();
        levelled(&hz, &mut db, &known);
        all.push(Known { name: format!("calibration {}", c.name), db, known });
    }
    all
}

/// What was learned of a kind of device.
pub struct Model {
    /// how many curves it was learned from
    pub n: usize,
    /// the average curve
    pub mean: Vec<f32>,
    /// the share of the curves that know each frequency
    pub seen: Vec<f32>,
    /// the ways the curves most differ from the average, each as long as it usually goes
    pub ways: Vec<Vec<f32>>,
    /// how hard a guess is held to the average (see `HOLDS`)
    pub hold: f32,
}

/// Learn a kind from its curves.
pub fn learn(curves: &[Known]) -> Model {
    let size = grid().len();
    let n = curves.len();
    let count = |i: usize| curves.iter().filter(|c| c.known[i]).count();
    let mean: Vec<f32> = (0..size).map(|i| curves.iter().filter(|c| c.known[i]).map(|c| c.db[i]).sum::<f32>() / count(i).max(1) as f32).collect();
    let seen: Vec<f32> = (0..size).map(|i| count(i) as f32 / n.max(1) as f32).collect();
    // how each curve differs from the average (where it is not known: not at all)
    let differs: Vec<Vec<f64>> = curves.iter().map(|c| (0..size).map(|i| if c.known[i] { (c.db[i] - mean[i]) as f64 } else { 0.0 }).collect()).collect();
    // how the frequencies vary together, and its strongest directions, one after another
    let mut together = vec![0.0f64; size * size];
    for d in &differs {
        for i in 0..size {
            for j in 0..size {
                together[i * size + j] += d[i] * d[j] / n.max(1) as f64;
            }
        }
    }
    let mut ways = Vec::new();
    for way in 0..WAYS.min(n.saturating_sub(1)) {
        // begun from a different wave each time, so that none begins at right angles to what it seeks
        let mut v: Vec<f64> = (0..size).map(|i| ((i as f64 + 1.0) * (way as f64 + 1.0) * 0.37).sin() + 0.1).collect();
        let mut strength = 0.0;
        for _ in 0..80 {
            let next: Vec<f64> = (0..size).map(|i| (0..size).map(|j| together[i * size + j] * v[j]).sum()).collect();
            strength = next.iter().map(|x| x * x).sum::<f64>().sqrt();
            if strength < 1e-9 {
                break;
            }
            v = next.iter().map(|x| x / strength).collect();
        }
        // a way the curves go less than a tenth of a dB along is no way
        if strength < 0.01 {
            break;
        }
        for i in 0..size {
            for j in 0..size {
                together[i * size + j] -= strength * v[i] * v[j];
            }
        }
        ways.push(v.iter().map(|x| (x * strength.sqrt()) as f32).collect());
    }
    let mut model = Model { n, mean, seen, ways, hold: HOLD };
    // the hold that guesses the kind's own curves best: up to sixty of them, each shown only in part
    // (three different parts), guessed with each hold in turn
    if n >= 10 && !model.ways.is_empty() {
        let hz = grid();
        let some: Vec<&Known> = curves.iter().step_by((n / 60).max(1)).collect();
        let missed = |model: &Model| -> f64 {
            let mut squares = 0.0;
            for curve in &some {
                for (from, to) in [(300.0, 3000.0), (100.0, 8000.0), (20.0, 2000.0)] {
                    let shown: Vec<bool> = (0..size).map(|i| curve.known[i] && (from..=to).contains(&hz[i])).collect();
                    let g = guess(model, &curve.db, &shown);
                    squares += (0..size).filter(|i| curve.known[*i] && !shown[*i]).map(|i| ((g.db[i] - curve.db[i]) as f64).powi(2)).sum::<f64>();
                }
            }
            squares
        };
        let mut best = f64::MAX;
        for hold in HOLDS {
            model.hold = hold;
            let m = missed(&model);
            if m < best {
                best = m;
            } else {
                break;
            }
        }
        // (the row is tried upward and left at the first hold that is worse than the one before)
        model.hold = HOLDS[HOLDS.iter().position(|h| *h == model.hold).unwrap_or(1).saturating_sub(if missed(&model) > best { 1 } else { 0 })];
    }
    model
}

/// A curve with its gaps filled in.
pub struct Guessed {
    pub db: Vec<f32>,
    /// true where the level was not measured but guessed
    pub guessed: Vec<bool>,
}

/// Solve a small set of equations (Gauss, with the largest number in each column brought up first).
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for col in 0..n {
        let top = (col..n).max_by(|x, y| a[*x][col].abs().total_cmp(&a[*y][col].abs())).unwrap_or(col);
        a.swap(col, top);
        b.swap(col, top);
        if a[col][col].abs() < 1e-12 {
            continue;
        }
        for row in col + 1..n {
            let by = a[row][col] / a[col][col];
            for k in col..n {
                a[row][k] -= by * a[col][k];
            }
            b[row] -= by * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let rest: f64 = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = if a[row][row].abs() < 1e-12 { 0.0 } else { (b[row] - rest) / a[row][row] };
    }
    x
}

/// Fill in a curve: `db` at the first of the grid's frequencies (a measurement may end before 20 kHz),
/// believed where `known`; everywhere else, and beyond its end, it is guessed. The curve comes back at
/// as many frequencies as it was given.
pub fn guess(model: &Model, db: &[f32], known: &[bool]) -> Guessed {
    let size = db.len().min(model.mean.len());
    let at: Vec<usize> = (0..size).filter(|i| known[*i]).collect();
    // the average, as a whole up or down by `level`, and along each way by its weight: as the known
    // frequencies ask, each weight held toward 0 as a weight is that usually stays within 1
    let unknowns = 1 + model.ways.len();
    let row = |i: usize| -> Vec<f64> { std::iter::once(1.0).chain(model.ways.iter().map(|w| w[i] as f64)).collect() };
    let mut a = vec![vec![0.0f64; unknowns]; unknowns];
    let mut b = vec![0.0f64; unknowns];
    for i in &at {
        let r = row(*i);
        let y = (db[*i] - model.mean[*i]) as f64;
        for j in 0..unknowns {
            b[j] += r[j] * y;
            for k in 0..unknowns {
                a[j][k] += r[j] * r[k];
            }
        }
    }
    for j in 1..unknowns {
        a[j][j] += model.hold as f64;
    }
    let weights = if at.is_empty() { vec![0.0; unknowns] } else { solve(a, b) };
    let said: Vec<f32> = (0..size).map(|i| model.mean[i] + row(i).iter().zip(&weights).map(|(r, w)| r * w).sum::<f64>() as f32).collect();
    // what the measured part still differs by is carried on from its edges into the guessed part,
    // fading with the distance
    let out = (0..size)
        .map(|i| {
            if known[i] {
                return db[i];
            }
            let edge = |j: Option<&usize>| j.map(|j| ((db[*j] - said[*j]), 0.5f32.powf((*j as f32 - i as f32).abs() / FADE)));
            let after = at.partition_point(|j| *j < i);
            let sides: Vec<(f32, f32)> = [edge(after.checked_sub(1).and_then(|b| at.get(b))), edge(at.get(after))].into_iter().flatten().collect();
            let pull: f32 = sides.iter().map(|(_, near)| near).sum();
            // between two edges each counts by how near it is; together never more than the nearer one alone
            let carried = if pull > 0.0 { sides.iter().map(|(off, near)| off * near * near).sum::<f32>() / pull } else { 0.0 };
            said[i] + carried
        })
        .collect();
    Guessed { db: out, guessed: (0..size).map(|i| !known[i]).collect() }
}

/// Fill in a measurement from what was learned of its kind: the speakers and the room alone (the
/// equalizer the sweep went through taken off) are guessed wherever the measurement is not to be
/// believed, and also where no curve of the kind was ever believed (small speakers' lowest bass is
/// heard above the noise in one sweep, yet is not theirs: see `measure.rs`). The measurement then holds
/// the whole curve, with what was guessed marked. Returns how many frequencies were measured and how
/// many guessed.
pub fn fill(m: &mut Measured, model: &Model) -> (usize, usize) {
    let alone = calibrate::raw(m);
    let believed: Vec<bool> = (0..alone.len()).map(|i| m.certain[i] && (model.n == 0 || model.seen.get(i).copied().unwrap_or(0.0) > 0.0)).collect();
    let g = guess(model, &alone, &believed);
    m.db = g.db.iter().zip(&m.eq).map(|(db, eq)| db + eq).collect();
    m.certain = vec![true; m.db.len()];
    m.guessed = g.guessed;
    (believed.iter().filter(|b| **b).count(), m.guessed.iter().filter(|g| **g).count())
}

/// A curve of the training set as a measurement would hold it: all of it guessed (nothing was
/// measured here), its own gaps filled from its kind.
pub fn like(curve: &Known, model: &Model) -> Measured {
    let hz = grid();
    let g = guess(model, &curve.db, &curve.known);
    let n = hz.len();
    Measured { hz, db: g.db, certain: vec![true; n], off_level: Vec::new(), eq: vec![0.0; n], guessed: vec![true; n], over_noise: vec![0.0; n], peak: 0.0, arrived: 0.0, level: 0.0 }
}

/// The curve of the training set asked for by name: the name, its beginning, or a part of it.
pub fn named(curves: &[Known], wanted: &str) -> Result<Known, String> {
    let key = |s: &str| s.to_lowercase().replace([' ', '-', '_'], "");
    let w = key(wanted);
    let found = curves.iter().find(|c| key(&c.name) == w).or_else(|| curves.iter().find(|c| key(&c.name).starts_with(&w))).or_else(|| curves.iter().find(|c| key(&c.name).contains(&w)));
    found.cloned().ok_or(format!("no curve among the {} to learn from is called \"{wanted}\"", curves.len()))
}

/// How well a kind is guessed, tried on curves it did not learn from: every fifth curve (by name) is
/// held out, the rest are learned from, and of each held-out curve only the frequencies from `from` to
/// `to` Hz are shown. Returns (curves learned from, curves tried, the usual error of the guess in dB
/// over the frequencies not shown, the same for the kind's average alone set to the shown part's
/// level, the same for a level line).
pub fn tried(curves: &[Known], from: f32, to: f32) -> (usize, usize, f32, f32, f32) {
    let hz = grid();
    let (held, learned): (Vec<_>, Vec<_>) = curves.iter().cloned().enumerate().partition(|(i, _)| i % 5 == 4);
    let (held, learned): (Vec<Known>, Vec<Known>) = (held.into_iter().map(|c| c.1).collect(), learned.into_iter().map(|c| c.1).collect());
    let model = learn(&learned);
    let average = Model { n: model.n, mean: model.mean.clone(), seen: model.seen.clone(), ways: Vec::new(), hold: HOLD };
    let level = Model { n: 0, mean: vec![0.0; hz.len()], seen: vec![0.0; hz.len()], ways: Vec::new(), hold: HOLD };
    let mut squares = [0.0f64; 3];
    let mut count = 0usize;
    for curve in &held {
        let shown: Vec<bool> = (0..hz.len()).map(|i| curve.known[i] && (from..=to).contains(&hz[i])).collect();
        for (which, m) in [&model, &average, &level].into_iter().enumerate() {
            let g = guess(m, &curve.db, &shown);
            // (without the fading carry for the two simpler ones it would not be a fair race: all three have it)
            squares[which] += (0..hz.len()).filter(|i| curve.known[*i] && !shown[*i]).map(|i| ((g.db[i] - curve.db[i]) as f64).powi(2)).sum::<f64>();
        }
        count += (0..hz.len()).filter(|i| curve.known[*i] && !shown[*i]).count();
    }
    let usual = |s: f64| (s / count.max(1) as f64).sqrt() as f32;
    (learned.len(), held.len(), usual(squares[0]), usual(squares[1]), usual(squares[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up kind of device: every curve is a bass shelf, a dip near 3 kHz and a treble slope, each
    /// of its own size (from a seed), with a little of its own unevenness on top.
    fn made_up(seed: u32) -> Known {
        let hz = grid();
        let mut state = seed.wrapping_mul(2654435761).wrapping_add(12345);
        let mut next = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 8) as f32 / 8388608.0 - 1.0
        };
        let (bass, dip, treble) = (6.0 * next(), 4.0 * next(), 5.0 * next());
        let mut db: Vec<f32> = hz
            .iter()
            .map(|hz| {
                let octaves = (hz / 1000.0).log2();
                bass / (1.0 + (hz / 150.0).powi(2)) + dip * (-((hz / 3000.0).log2().powi(2)) / 0.3).exp() + treble * (octaves - 2.0).max(0.0) / 2.0 + 0.3 * next()
            })
            .collect();
        let known = vec![true; hz.len()];
        levelled(&hz, &mut db, &known);
        Known { name: format!("made up {seed}"), db, known }
    }

    #[test]
    fn a_table_is_read_and_set_onto_the_grid() {
        // AutoEq's kind of file: the error is taken, not the raw level
        let autoeq = "frequency,raw,smoothed,error,target\n20.00,9.2,9.2,3.0,5.5\n1000,1.0,1.0,0.0,1.0\n20000,-4.0,-4.0,-6.0,2.0\n";
        assert_eq!(read_curve(autoeq), [(20.0, 3.0), (1000.0, 0.0), (20000.0, -6.0)]);
        // a plain table, with remarks: the second number
        assert_eq!(read_curve("* measured by hand\n100 2.5 0\n1000 0.5 0\n"), [(100.0, 2.5), (1000.0, 0.5)]);
        let curve = onto_grid("a", &[(50.0, 4.0), (500.0, 0.0), (5000.0, 0.0), (10000.0, -8.0)]).unwrap();
        let hz = grid();
        let at = |f: f32| (0..hz.len()).find(|i| hz[*i] >= f).unwrap();
        assert!(!curve.known[at(30.0)] && curve.known[at(60.0)] && !curve.known[at(15000.0)]);
        // against its average between 200 Hz and 5 kHz, which is a little above its 0
        assert!(curve.db[at(1000.0)] < 0.0 && curve.db[at(1000.0)] > -1.0 && (curve.db[at(50.0)] - curve.db[at(1000.0)] - 4.0).abs() < 0.2);
        assert!(onto_grid("too short", &[(1000.0, 0.0), (1200.0, 1.0)]).is_none());
    }

    #[test]
    fn what_was_not_measured_is_guessed_from_the_kind() {
        let curves: Vec<Known> = (0..60).map(made_up).collect();
        // held out, with only 300 Hz to 2 kHz shown (the dip at 3 kHz and both ends are to be guessed):
        // the guess is much nearer than the kind's average or a level line (which here are much the
        // same: the made-up curves go as often up as down)
        let (learned, held, guessed, average, level) = tried(&curves, 300.0, 2000.0);
        assert_eq!((learned, held), (48, 12));
        assert!(guessed < 0.7 * average && guessed < 0.7 * level, "guessed {guessed} dB off, the average {average} dB, a level line {level} dB");
        // and with the dip and the treble's beginning shown too, nearer still
        let wide = tried(&curves, 300.0, 6000.0).2;
        assert!(wide < 0.8 * guessed && wide < 1.0, "with more shown: {wide} dB off (with less: {guessed})");
    }

    #[test]
    fn what_was_measured_stays_and_the_guess_meets_it_without_a_step() {
        let curves: Vec<Known> = (0..40).map(made_up).collect();
        let model = learn(&curves);
        assert!(model.n == 40 && !model.ways.is_empty() && model.ways.len() <= WAYS);
        let hz = grid();
        let truth = made_up(999);
        // a device of the kind with something of its own: 5 dB more everywhere above 1 kHz than its kind would say
        let own: Vec<f32> = (0..hz.len()).map(|i| truth.db[i] + if hz[i] > 1000.0 { 5.0 } else { 0.0 }).collect();
        let known: Vec<bool> = hz.iter().map(|hz| (100.0..4000.0).contains(hz)).collect();
        let g = guess(&model, &own, &known);
        assert!((0..hz.len()).all(|i| !known[i] || g.db[i] == own[i]));
        assert!((0..hz.len()).all(|i| g.guessed[i] != known[i]));
        let edge = (0..hz.len()).rev().find(|i| known[*i]).unwrap();
        assert!((g.db[edge + 1] - g.db[edge]).abs() < 1.0, "a step of {} dB where the guess begins", g.db[edge + 1] - g.db[edge]);
        // a shorter curve (measured at 44,100 Hz) comes back as short
        assert_eq!(guess(&model, &own[..230], &known[..230]).db.len(), 230);
    }

    #[test]
    fn with_nothing_to_learn_from_the_measured_level_is_carried_on() {
        let model = learn(&[]);
        assert!(model.n == 0 && model.ways.is_empty());
        let hz = grid();
        let db: Vec<f32> = hz.iter().map(|hz| if *hz < 1000.0 { 2.0 } else { -2.0 }).collect();
        let known: Vec<bool> = hz.iter().map(|hz| (200.0..5000.0).contains(hz)).collect();
        let g = guess(&model, &db, &known);
        let (lowest, highest) = (g.db[0], g.db[hz.len() - 1]);
        // just outside the measured part it is what the edge was; far away it has come back toward the measured part's level (0)
        let first = (0..hz.len()).find(|i| known[*i]).unwrap();
        assert!((g.db[first - 1] - 2.0).abs() < 0.3 && lowest.abs() < 1.0 && highest.abs() < 1.5, "next to the edge {}, at 20 Hz {lowest}, at 20 kHz {highest}", g.db[first - 1]);
        // and with nothing measured at all, nothing is said but the average (here: nothing)
        assert!(guess(&model, &db, &vec![false; hz.len()]).db.iter().all(|d| *d == 0.0));
    }
}
