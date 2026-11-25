//! Calibrations kept by name: a correction curve with what it was made for and from, a file in the
//! folder `calibrations` where Bottle EQ is run from. Apart from the measuring (`calibrate`), so that
//! the equalizer and its calibrations can be used as a library (`bottle_eq`) without the app.

use crate::eq::{Curve, Kind, Point, Settings, BANDS};

/// A curve of the equalizer's.
pub fn curve(gains: &[f32; BANDS], points: &[Point]) -> Curve {
    let settings = Settings::default();
    settings.set_all(gains);
    settings.set_points(points);
    settings.get()
}

/// The kinds of device a calibration is kept as.
pub const DEVICE_KINDS: [&str; 2] = ["speakers", "headphones"];

/// A calibration: a correction curve by name, with what it was made for and from.
#[derive(Clone)]
pub struct Calibration {
    pub name: String,
    /// the kind of device: "speakers" or "headphones"
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
pub fn file(name: &str) -> String {
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

    /// Set it as the correction, under whatever curve is set by hand.
    pub fn set(&self, settings: &Settings) {
        settings.set_correction(&self.gains, &self.points);
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
