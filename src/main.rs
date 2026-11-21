//! Bottle EQ: a system-wide audio equalizer for Windows and Linux.
//!
//!   bottle-eq [window] [--from <device>] [--to <device>]   the window: the live spectrum, the curve and
//!                                                      its points to drag, the modes, Flat, Start and Stop
//!   bottle-eq devices                                  the output devices, their formats and blocks, and
//!                                                      the microphones
//!   bottle-eq measure --mic <microphone> --to <device> [--from <device>] [--level <0..1>] [--keep <csv file>]
//!                                                      a sweep out of the speakers, recorded by the
//!                                                      microphone: what reaches the listening place. With
//!                                                      --from it goes through the path and the equalizer
//!       ... --correct [--save <name>] [--gentle]       and then: the measured curve turned over into the
//!                                                      equalizer's (its exact mirror image; --gentle: the
//!                                                      careful way), measured again through it, and kept
//!                                                      as a calibration of that name
//!   bottle-eq calibrations                             the calibrations kept
//!   bottle-eq guess --mic <microphone> --to <device> [--from <device>] [--type speakers|headphones] [--save <name>]
//!                                                      one short sweep, the rest of the curve guessed from
//!                                                      what curves of that kind look like, the correction
//!                                                      for it, and (with --from) a whole measurement
//!                                                      through it to see how near flat it came
//!   bottle-eq guess --type headphones --like <name> [--save <name>]
//!                                                      the correction for a curve of the training set by
//!                                                      name: nothing is played or measured
//!   bottle-eq import <file or folder> --type <kind>    bring curves in to learn from (AutoEq's results,
//!                                                      or any table of frequency and dB)
//!   bottle-eq training [--test]                        what there is to learn from; --test: how well each
//!                                                      kind is guessed on curves it did not learn from
//!   bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other
//!   bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
//!   bottle-eq playtest --from <device> --to <device> --file <wav> [--record <folder>] [--level <0..1>]
//!                                                      music through the whole path; what was captured and
//!                                                      what the target was given are kept as WAV files
//!
//! A device is its number in `devices` or a part of its name. On Linux --from is left out: the sound
//! is captured from Bottle EQ's own device, which programs play to (see `audio/pipewire.rs`). Options:
//!   --exclusive | --shared   take the target for Bottle EQ alone (short delay) or share it (long delay);
//!                            without either, `run` asks
//!   --cushion-ms <n>         sound kept queued on top of what the devices' blocks need (default 2)
//!   --seconds <n>            run: stop by itself; selftest: the tone's length
//!
//!   --gains <ten numbers>    the ten bands' gains in dB, low to high, e.g. --gains 4,2,0,0,0,0,0,0,2,4
//!   --mode <name>            a mode from modes.txt, e.g. --mode "bass boost"
//!   --calibration <name>     a calibration kept (its bands and points), e.g. --calibration speakers
//!   --volume <dB>            the whole sound louder (up to +18) or quieter; a limiter keeps it from clipping
//!   --points <points>        points on top of the bands, as in FL Studio's EQ: each a filter of its own,
//!                            e.g. --points "100:cut 1k:-2 12k:+8:spike" (a slope at the start, a dip, a
//!                            spike at the end); see `points_of` for how a point is written
//!
//!   bottle-eq modes                                    the modes
//!   bottle-eq curve --gains | --mode | --points ...    the curve they make, every third of an octave
//!
//! While `run` plays, a line sets the curve: "mode rock", "modes", "points 100:cut 12k:+8:spike", "1k +3",
//! "31.5 -2", "8 4" (the 8th band), "flat", "off", "on"; an empty line stops.
//!
//! The window begins at once when --exclusive or --shared is given with both devices; else it waits for
//! Start. For trying it out: --file <wav> [--level x] is played to the source, --drag <point>:<hz>:<dB>
//! drags a point there by itself, --mic <microphone> --measure measures by itself a second after opening
//! (with --correct it then corrects and measures again),
//! and --shot <bmp> [--seconds n] keeps a picture of the window and closes.
//!
//! Steps 1 to 5 of 5.

mod audio;
mod calibrate;
mod eq;
mod guess;
mod measure;
mod ring;
mod spectrum;
mod window;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use eq::{Eq, Kind, Point, Settings, BANDS, CENTRES, CUT_Q, POINT_Q};
use audio::{find, outputs, run, value, Output, Running};

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

fn db(x: f32) -> String {
    if x <= 0.0 { "  -inf".to_string() } else { format!("{:6.1}", 20.0 * x.log10()) }
}

/// What was heard in the sound handed to the target: when it began, and how much of it was the tone.
#[derive(Default)]
struct Heard {
    began: bool,
    n: f64,
    squares: f64,
    /// the level over the first half second after the onset (the tone is surely on then)
    first: f64,
    /// the tone's power, summed over tenths of a second (so a pitch a hair off still counts as the tone)
    tone: f64,
    sin: f64,
    cos: f64,
    /// the loudest jump from one frame to the next: a click would show here
    jump: f64,
    last: f64,
}

impl Heard {
    fn hear(&mut self, sound: &[f32], channels: usize, hz: f64, rate: f64) {
        for frame in sound.chunks(channels) {
            let s = frame[0] as f64;
            self.began |= s.abs() > 0.002;
            if self.began {
                let angle = std::f64::consts::TAU * hz * self.n / rate;
                self.squares += s * s;
                if self.n < rate / 2.0 {
                    self.first += s * s;
                }
                self.sin += s * angle.sin();
                self.cos += s * angle.cos();
                self.n += 1.0;
                self.jump = self.jump.max((s - self.last).abs());
                if self.n % (rate / 10.0).floor() == 0.0 {
                    self.tone += 2.0 * (self.sin * self.sin + self.cos * self.cos) / (rate / 10.0).floor();
                    (self.sin, self.cos) = (0.0, 0.0);
                }
            }
            self.last = s;
        }
    }
}

/// A WAV file as (rate, interleaved two-channel float samples). Float 32 and 16-bit PCM are read; a
/// mono file goes to both channels, further channels are left out.
fn read_wav(path: &str) -> Result<(u32, Vec<f32>), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".to_string());
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let (mut at, mut format) = (12, None);
    while at + 8 <= bytes.len() {
        let (name, size) = (&bytes[at..at + 4], u32_at(at + 4) as usize);
        let body = at + 8;
        if name == b"fmt " {
            let tag = if u16_at(body) == 0xFFFE { u16_at(body + 24) } else { u16_at(body) };
            format = Some((tag, u16_at(body + 2) as usize, u32_at(body + 4), u16_at(body + 14)));
        } else if name == b"data" {
            let (tag, channels, rate, bits) = format.ok_or("no format before the data")?;
            let data = &bytes[body..(body + size).min(bytes.len())];
            let samples: Vec<f32> = match (tag, bits) {
                (3, 32) => data.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
                (1, 16) => data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect(),
                _ => return Err(format!("format {tag} with {bits} bits is not read (32-bit float and 16-bit PCM are)")),
            };
            let two: Vec<f32> = samples.chunks_exact(channels).flat_map(|f| [f[0], f[if channels > 1 { 1 } else { 0 }]]).collect();
            return Ok((rate, two));
        }
        at = body + size + (size & 1);
    }
    Err("no sound in the file".to_string())
}

fn write_wav(path: &str, rate: u32, channels: usize, samples: &[f32]) -> Result<(), String> {
    let mut out = Vec::with_capacity(44 + samples.len() * 4);
    let bytes = (samples.len() * 4) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&(channels as u16).to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
    out.extend_from_slice(&(channels as u16 * 4).to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out).map_err(|e| format!("{path}: {e}"))
}

/// The gains given with --gains, as ten numbers in dB.
fn parse_gains(text: &str) -> [f32; BANDS] {
    let given: Vec<f32> = text.split(',').map(|g| g.trim().parse().unwrap_or_else(|_| fail("--gains takes ten numbers, like 4,2,0,0,0,0,0,0,2,4"))).collect();
    if given.len() != BANDS {
        fail(&format!("--gains takes {BANDS} numbers, one for each band ({} were given)", given.len()));
    }
    std::array::from_fn(|b| given[b])
}

/// A frequency as written: "31.5", "1k", "16000".
fn hz_of(text: &str) -> Option<f32> {
    match text.to_lowercase().strip_suffix('k') {
        Some(thousands) => Some(thousands.parse::<f32>().ok()? * 1000.0),
        None => text.parse().ok(),
    }
    .filter(|hz| *hz > 0.0)
}

/// The points of the curve as written, apart by spaces or commas. A point is its frequency, then after
/// colons its gain in dB, its width, and its kind, of which only the frequency must be there:
/// "80:+5", "1k:-3:2" (narrower), "100:cut" (the curve starts with a slope), "12k:+8:spike",
/// "8k:+4:shelf". Kinds: cut, shelf (low or high by which side of 1 kHz the point is), lowcut, highcut,
/// lowshelf, highshelf, peak, spike (a narrow peak).
fn points_of(text: &str) -> Result<Vec<Point>, String> {
    let how = "a point is frequency:dB, like 80:+5 1k:-3 12k:+8:spike, or 100:cut for a slope";
    let mut points = Vec::new();
    for word in text.split([' ', ',']).filter(|p| !p.is_empty()) {
        let mut parts = word.split(':');
        let hz = parts.next().and_then(hz_of).ok_or(format!("\"{word}\": {how}"))?;
        let (mut numbers, mut kind, mut narrow) = (Vec::new(), None, false);
        for part in parts {
            match (part.parse::<f32>(), part.to_lowercase().as_str()) {
                (Ok(n), _) => numbers.push(n),
                (_, "cut") => kind = Some(if hz < 1000.0 { Kind::LowCut } else { Kind::HighCut }),
                (_, "shelf") => kind = Some(if hz < 1000.0 { Kind::LowShelf } else { Kind::HighShelf }),
                (_, "lowcut") => kind = Some(Kind::LowCut),
                (_, "highcut") => kind = Some(Kind::HighCut),
                (_, "lowshelf") => kind = Some(Kind::LowShelf),
                (_, "highshelf") => kind = Some(Kind::HighShelf),
                (_, "peak") => kind = Some(Kind::Peak),
                (_, "spike") => (kind, narrow) = (Some(Kind::Peak), true),
                _ => return Err(format!("\"{word}\": {how}")),
            }
        }
        let cut = matches!(kind, Some(Kind::LowCut | Kind::HighCut));
        // a cut has no gain: a number after it is its width
        let (db, q) = match (cut, numbers.as_slice()) {
            (true, [q, ..]) => (0.0, *q),
            (true, []) => (0.0, CUT_Q),
            (false, [db, q, ..]) => (*db, *q),
            (false, [db]) => (*db, if narrow { 6.0 } else { POINT_Q }),
            (false, []) => (0.0, if narrow { 6.0 } else { POINT_Q }),
        };
        points.push(Point { hz, db, q, kind });
    }
    if points.is_empty() || points.len() > eq::POINTS {
        return Err(format!("a curve has from one to {} points; {how}", eq::POINTS));
    }
    Ok(points)
}

/// A band named as its place (1 to 10) or its centre ("31.5", "1k", "16000").
fn band_of(name: &str) -> Option<usize> {
    let hz = hz_of(name)?;
    if hz.fract() == 0.0 && (1.0..=BANDS as f32).contains(&hz) {
        return Some(hz as usize - 1);
    }
    CENTRES.iter().position(|c| (c / hz).log2().abs() < 0.25)
}

fn show_gains(gains: &Settings, rate: u32) {
    let set = gains.get();
    let hz = |hz: f32| if hz >= 1000.0 { format!("{}k", hz / 1000.0) } else { format!("{hz}") };
    let names: Vec<String> = CENTRES.iter().zip(set.gains).map(|(c, g)| format!("{}:{g:+.1}", hz(*c))).collect();
    println!("bands (dB): {}", names.join(" "));
    let points: Vec<String> = (0..set.points().len())
        .map(|i| {
            let p = set.points()[i];
            match set.kind(i) {
                Kind::LowCut => format!("{} low cut (the curve starts with a slope; width {:.2})", hz(p.hz), p.q),
                Kind::HighCut => format!("{} high cut (width {:.2})", hz(p.hz), p.q),
                Kind::LowShelf => format!("{}:{:+.1} low shelf", hz(p.hz), p.db),
                Kind::HighShelf => format!("{}:{:+.1} high shelf", hz(p.hz), p.db),
                Kind::Peak => format!("{}:{:+.1} peak (width {:.2})", hz(p.hz), p.db, p.q),
            }
        })
        .collect();
    if !points.is_empty() {
        println!("points:     {}", points.join(" | "));
    }
    match gains.on.load(Ordering::Relaxed) {
        true => println!("            turned down {:.1} dB for headroom{}", -set.headroom(rate as f64), if gains.volume() != 0.0 { format!("; volume {:+.1} dB", gains.volume()) } else { String::new() }),
        false => println!("            off: the sound passes untouched"),
    }
}

fn describe(running: &Running, source: &Output, target: &Output) {
    let o = &running.opened;
    if cfg!(target_os = "linux") {
        println!("programs play to:  \"Bottle EQ\", a device of its own in the PipeWire graph (choose it as the output device)");
        println!("it is played on to: {}  (32-bit float, {} Hz; a block of {} frames, {:.2} ms, is asked for)", target.name, running.out_rate, o.out_block, o.out_block as f32 * 1000.0 / running.out_rate as f32);
        return;
    }
    println!("capturing what plays to: {}  (in blocks of {} frames, {:.2} ms)", source.name, o.in_block, o.in_block as f32 * 1000.0 / running.in_rate as f32);
    println!(
        "playing it to:           {}  ({}, blocks of {} frames, {:.2} ms, {})",
        target.name,
        if o.direct { "exclusive mode, each block played as it arrives" } else if o.exclusive { "exclusive mode" } else { "shared mode" },
        o.out_block,
        o.out_block as f32 * 1000.0 / running.out_rate as f32,
        if o.exclusive { format!("{}-bit", o.out_bits) } else { "32-bit float".to_string() }
    );
    if !o.exclusive && !o.note.is_empty() {
        println!("exclusive mode was not to be had: {}", o.note);
    }
    let with = running.meters.priority.load(Ordering::Relaxed);
    println!("audio threads with Windows' Pro Audio priority: {}", if with == 2 { "yes" } else { "NO" });
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let (from, to) = (take("--from"), take("--to"));
    // Linux: the sound comes from Bottle EQ's own device, the first of the devices
    let from = from.or(cfg!(target_os = "linux").then(|| "1".to_string()));
    let cushion_ms: f32 = take("--cushion-ms").map(|s| s.parse().unwrap_or_else(|_| fail("--cushion-ms takes a number"))).unwrap_or(2.0);
    let (file, record) = (take("--file"), take("--record"));
    let level_given: Option<f32> = take("--level").map(|s| s.parse().unwrap_or_else(|_| fail("--level takes a number")));
    let level = level_given.unwrap_or(1.0);
    let (mic_text, keep_text) = (take("--mic"), take("--keep"));
    let (calibration_text, save_text) = (take("--calibration"), take("--save"));
    let (kind, like_text) = (take("--type").unwrap_or("speakers".to_string()), take("--like"));
    let volume: f32 = take("--volume").map(|s| s.parse().unwrap_or_else(|_| fail("--volume takes a number of dB, like +6"))).unwrap_or(0.0);
    let (gains_text, mode_text, points_text) = (take("--gains"), take("--mode"), take("--points"));
    let (shot, drag) = (take("--shot"), take("--drag"));
    let seconds: Option<f32> = take("--seconds").map(|s| s.parse().unwrap_or_else(|_| fail("--seconds takes a number")));
    let mut flag = |f: &str| match args.iter().position(|a| a == f) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    };
    let (exclusive, shared, measure_flag, correct) = (flag("--exclusive"), flag("--shared"), flag("--measure"), flag("--correct"));
    let (test_flag, guess_flag) = (flag("--test"), flag("--guess"));
    // a measured curve is turned over exactly, unless the careful way is asked for
    let gentle = flag("--gentle");
    let how = if gentle { &calibrate::GENTLE } else { &calibrate::EXACT };
    let gains = Arc::new(Settings::default());
    gains.set_volume(volume);
    let given = gains_text.map(|g| parse_gains(&g));
    given.iter().flatten().enumerate().for_each(|(b, g)| gains.set(b, *g));
    // a mode is a named set of the ten bands' gains; points are filters of their own on top
    if let Some(name) = &mode_text {
        let mode = eq::mode(name).unwrap_or_else(|e| fail(&e));
        println!("mode: {}", mode.name);
        gains.set_all(&mode.gains);
    }
    if let Some(name) = &calibration_text {
        let calibration = calibrate::calibration(name).unwrap_or_else(|e| fail(&e));
        println!("calibration: {} ({}, measured {})", calibration.name, calibration.device, calibration.day);
        calibration.set(&gains);
    }
    if let Some(text) = &points_text {
        gains.set_points(&points_of(text).unwrap_or_else(|e| fail(&e)));
    }
    let pair = || {
        let (Some(from), Some(to)) = (&from, &to) else { fail("give --from <device> and --to <device> (see `bottle-eq devices`)") };
        (find(from).unwrap_or_else(|e| fail(&e)), find(to).unwrap_or_else(|e| fail(&e)))
    };

    match args.first().map(String::as_str) {
        None | Some("window") => {
            let drag = drag.map(|d| {
                let parts: Vec<&str> = d.split(':').collect();
                match parts.as_slice() {
                    [i, hz, db] => (i.parse::<usize>().ok().filter(|i| *i >= 1).zip(hz_of(hz)).zip(db.parse::<f32>().ok())).map(|((i, hz), db)| (i - 1, hz, db)),
                    _ => None,
                }
                .unwrap_or_else(|| fail("--drag takes point:frequency:dB, like 3:10k:+8"))
            });
            let start = window::Start {
                from,
                to,
                exclusive: if exclusive { Some(true) } else if shared { Some(false) } else { None },
                cushion_ms,
                settings: gains,
                mode: mode_text,
                play: file.map(|f| (f, level)),
                drag,
                shot: shot.map(|path| (path, seconds.unwrap_or(4.0))),
                mic: mic_text,
                measure: measure_flag,
                correct,
                guess: guess_flag,
                gentle,
                kind: kind.clone(),
            };
            window::open(start).unwrap_or_else(|e| fail(&e));
        }
        Some("devices") => {
            for (i, o) in outputs().unwrap_or_else(|e| fail(&e)).iter().enumerate() {
                let ms = |frames: u32| frames as f32 * 1000.0 / o.rate as f32;
                let plays = if o.default { format!("  [{} plays here]", audio::SYSTEM) } else { String::new() };
                match cfg!(windows) {
                    true => println!("{}. {}{plays}  ({} channels, {} Hz; shared blocks {:.2} ms, least {:.2} ms)", i + 1, o.name, o.channels, o.rate, ms(o.periods.0), ms(o.periods.1)),
                    false => println!("{}. {}{plays}", i + 1, o.name),
                }
            }
            println!("microphones:");
            for (i, m) in audio::inputs().unwrap_or_else(|e| fail(&e)).iter().enumerate() {
                println!("{}. {}{}", i + 1, m.name, if m.default { format!("  [{} records here]", audio::SYSTEM) } else { String::new() });
            }
        }
        Some("import") => {
            let path = args.get(1).cloned().unwrap_or_else(|| fail("import needs a file or a folder of curves, and --type <kind>"));
            let (taken, left) = guess::import(&path, &kind).unwrap_or_else(|e| fail(&e));
            println!("{taken} curves brought in to learn {kind} from (training/{kind}/); {left} files held no curve");
        }
        Some("training") => {
            for kind in guess::KINDS {
                let curves = guess::training(kind);
                let own = curves.iter().filter(|c| c.name.starts_with("calibration ")).count();
                let model = guess::learn(&curves);
                println!("{kind}: {} curves to learn from ({} brought in, {own} from calibrations kept); they differ from their average in {} ways that count (hold {})", curves.len(), curves.len() - own, model.ways.len(), model.hold);
                if test_flag && curves.len() >= 10 {
                    println!("  every fifth curve held out and guessed from the other four fifths; the usual error over the frequencies not shown:");
                    for (from, to) in [(300.0, 3000.0), (100.0, 8000.0), (500.0, 20000.0), (20.0, 2000.0)] {
                        let (learned, held, guessed, average, level) = guess::tried(&curves, from, to);
                        println!("  shown {from:>5.0} Hz to {to:>5.0} Hz ({learned} learned from, {held} tried): guessed {guessed:.2} dB, the kind's average alone {average:.2} dB, a level line {level:.2} dB");
                    }
                } else if test_flag {
                    println!("  too few to hold any out");
                }
            }
        }
        Some("guess") if like_text.is_some() => {
            // the correction for a curve of the training set: nothing is played, nothing measured
            let curves = guess::training(&kind);
            let curve = guess::named(&curves, like_text.as_deref().unwrap_or_default()).unwrap_or_else(|e| fail(&e));
            let m = guess::like(&curve, &guess::learn(&curves));
            let want = calibrate::wanted(&m, how);
            let fitted = calibrate::fit(&m.hz, &want.db, &want.counts, 48000.0, how);
            gains.set_all(&fitted.gains);
            gains.set_points(&fitted.points);
            println!("\"{}\", one of the {} curves of {kind}: not measured here; what its measurer found it to be off by (from its target, where the file says one)", curve.name, curves.len());
            println!("the correction (the curve lies {:.2} dB from what is wanted, {:.2} dB at the most):", fitted.off, fitted.most_off);
            show_gains(&gains, 48000);
            let set = gains.get();
            let after: Vec<f32> = (0..m.hz.len()).map(|i| m.db[i] + set.response_db(48000.0, m.hz[i] as f64)).collect();
            println!("      Hz   the curve   wanted   the correction   with it");
            for i in (0..m.hz.len()).step_by(8) {
                println!("  {:>6.0}   {:>+6.1}   {:>+6.1}   {:>+6.1}   {:>+6.1}  {}", m.hz[i], m.db[i], want.db[i], set.response_db(48000.0, m.hz[i] as f64), after[i], if want.reach[i] { "" } else { "-" });
            }
            println!("\"-\" marks what is out of reach. Off within reach: {:.2} dB as its measurer found it, {:.2} dB with the correction, if this device is as that one was (not measured)", calibrate::spread(&m.db, &want.reach), calibrate::spread(&after, &want.reach));
            if let Some(name) = save_text {
                let calibration = calibrate::Calibration { name, kind: kind.clone(), device: curve.name.clone(), microphone: "none: guessed from the training set".to_string(), day: calibrate::today(), gains: set.gains, points: set.points().to_vec(), hz: m.hz.clone(), raw: m.db.clone(), certain: vec![false; m.hz.len()] };
                println!("kept: {} (use it with --calibration \"{}\")", calibration.save().unwrap_or_else(|e| fail(&e)), calibration.name);
            }
        }
        Some(command @ ("measure" | "guess")) => {
            let guessing = command == "guess";
            let mic = audio::find_input(&mic_text.unwrap_or_else(|| fail("give --mic <microphone> (see `bottle-eq devices`)"))).unwrap_or_else(|e| fail(&e));
            let target = find(&to.clone().unwrap_or_else(|| fail("give --to <device>: the speakers the sweep is played out of"))).unwrap_or_else(|e| fail(&e));
            // with a source the sweep is played to it and goes through the path and the equalizer as
            // music does; without, straight to the speakers
            let source = from.as_ref().map(|f| find(f).unwrap_or_else(|e| fail(&e)));
            let running = source.as_ref().map(|source| {
                let (mut equalizer, g) = (Eq::new(target.rate), gains.clone());
                let running = run(source, &target, cushion_ms, !shared, |_, _| {}, move |sound, channels| equalizer.process(sound, channels, &g)).unwrap_or_else(|e| fail(&e));
                describe(&running, source, &target);
                show_gains(&gains, target.rate);
                std::thread::sleep(Duration::from_millis(500));
                running
            });
            let through = || running.is_some().then(|| gains.get());
            let play_to = source.as_ref().unwrap_or(&target);
            let level = level_given.unwrap_or(0.5);
            let said = |m: &measure::Measured| {
                println!("the sweep arrived {:.2} s into the recording, {:.1} dB of what was sent; the recording's loudest sample: {} dB{}", m.arrived, m.level, db(m.peak).trim(), if m.peak >= 0.99 { "  (IT CLIPPED: turn the microphone down)" } else { "" });
                println!("{:.0}% of the frequencies from 20 Hz to 20 kHz are to be believed: clearly ({} dB) above the noise, and {} dB quieter (within {}) in the quieter sweep", 100.0 * m.share(), measure::CLEAR, measure::QUIETER, measure::STEADY);
            };
            let m = if guessing {
                // one sweep, and the rest of the curve from what curves of the kind look like
                println!("a short measurement with \"{}\": one sweep from 20 Hz to 20 kHz, about {:.0} seconds ...", mic.name, measure::SHORT_TAKES);
                let mut m = measure::short(&mic, play_to, level, through().as_ref()).unwrap_or_else(|e| fail(&e));
                println!("the sweep arrived {:.2} s into the recording, {:.1} dB of what was sent; {:.0}% of the frequencies clearly above the noise", m.arrived, m.level, 100.0 * m.share());
                let curves = guess::training(&kind);
                let (measured, guessed) = guess::fill(&mut m, &guess::learn(&curves));
                println!("{measured} frequencies measured, {guessed} guessed from {} curves of {kind}", curves.len());
                println!("      Hz      dB");
                for i in (0..m.hz.len()).step_by(8) {
                    println!("  {:>6.0}  {:>+6.1}  {}", m.hz[i], m.db[i], if m.guessed[i] { "guessed" } else { "" });
                }
                m
            } else {
                println!("measuring with \"{}\": a sweep from 20 Hz to 20 kHz, and once more 10 dB quieter; about {:.0} seconds ...", mic.name, measure::TAKES);
                let m = measure::measure(&mic, play_to, level, through().as_ref()).unwrap_or_else(|e| fail(&e));
                said(&m);
                println!("      Hz      dB   above the noise   off in the quieter sweep");
                for i in (0..m.hz.len()).step_by(8) {
                    println!("  {:>6.0}  {:>+6.1}   {:>5.0} dB   {:>+6.1} dB  {}", m.hz[i], m.db[i], m.over_noise[i], m.off_level[i], if m.certain[i] { "" } else { "?" });
                }
                println!("\"?\" marks what is not to be believed");
                m
            };
            let rows = |m: &measure::Measured| -> Vec<String> { (0..m.hz.len()).map(|i| format!("{:.2},{:.3},{},{:.1},{:.2},{:.3},{}", m.hz[i], m.db[i], m.certain[i] as u8, m.over_noise[i], m.off_level.get(i).copied().unwrap_or(0.0), m.eq[i], m.guessed.get(i).copied().unwrap_or(false) as u8)).collect() };
            let keep = |path: &str, m: &measure::Measured| {
                std::fs::write(path, format!("hz,db,certain,above_noise_db,off_in_quieter_db,eq_db,guessed\n{}\n", rows(m).join("\n"))).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
                println!("kept: {path}");
            };
            if let Some(path) = &keep_text {
                keep(path, &m);
            }
            if correct || guessing {
                // the measured curve turned over into the equalizer's
                let want = calibrate::wanted(&m, how);
                let fitted = calibrate::fit(&m.hz, &want.db, &want.counts, target.rate as f64, how);
                gains.set_all(&fitted.gains);
                gains.set_points(&fitted.points);
                println!("the correction (the curve lies {:.2} dB from what is wanted, {:.2} dB at the most):", fitted.off, fitted.most_off);
                show_gains(&gains, target.rate);
                let before = calibrate::spread(&calibrate::raw(&m), &want.reach);
                let (from, to) = ((0..m.hz.len()).find(|i| want.reach[*i]).map_or(0.0, |i| m.hz[i]), (0..m.hz.len()).rev().find(|i| want.reach[*i]).map_or(0.0, |i| m.hz[i]));
                println!("within reach (believed, and not more than {} dB under the rest): {} frequencies from {from:.0} Hz to {to:.0} Hz", how.reach, want.reach.iter().filter(|r| **r).count());
                if running.is_some() {
                    // and measured again through it, at the same frequencies
                    std::thread::sleep(Duration::from_secs(1));
                    println!("measuring again through the correction, in full (two sweeps, about {:.0} seconds) ...", measure::TAKES);
                    let again = measure::measure(&mic, play_to, level, through().as_ref()).unwrap_or_else(|e| fail(&e));
                    said(&again);
                    println!("      Hz   before    after   the correction");
                    for i in (0..m.hz.len()).step_by(8) {
                        println!("  {:>6.0}  {:>+6.1}   {:>+6.1}   {:>+6.1}  {}", m.hz[i], calibrate::raw(&m)[i], again.db[i], again.eq[i], if want.reach[i] { "" } else { "-" });
                    }
                    let both: Vec<bool> = (0..m.hz.len()).map(|i| want.reach[i] && again.certain[i]).collect();
                    println!(
                        "\"-\" marks what is out of reach. From flat, over the {} frequencies within reach and believed again: {:.2} dB before, {:.2} dB after (the usual distance of the level from its mean)",
                        both.iter().filter(|b| **b).count(),
                        calibrate::spread(&calibrate::raw(&m), &both),
                        calibrate::spread(&again.db, &both)
                    );
                    if let Some(path) = &keep_text {
                        keep(&path.replace(".csv", "_after.csv"), &again);
                    }
                } else {
                    println!("from flat before: {before:.2} dB. Not measured again: without --from the sweep does not pass the equalizer");
                }
                if let Some(name) = save_text {
                    let set = gains.get();
                    // what was guessed is not kept as measured: the guesser must not learn from its own guesses
                    let measured = (0..m.hz.len()).map(|i| m.certain[i] && !m.guessed.get(i).copied().unwrap_or(false)).collect();
                    let calibration = calibrate::Calibration { name, kind: kind.clone(), device: target.name.clone(), microphone: mic.name.clone(), day: calibrate::today(), gains: set.gains, points: set.points().to_vec(), hz: m.hz.clone(), raw: calibrate::raw(&m), certain: measured };
                    println!("kept: {} (use it with --calibration \"{}\")", calibration.save().unwrap_or_else(|e| fail(&e)), calibration.name);
                }
            }
            drop(running);
        }
        Some("correct") => {
            // the correction a kept measurement asks for, and how flat it should make it (not measured)
            let path = keep_text.unwrap_or_else(|| fail("correct needs --keep <the csv file a measurement was kept in>"));
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
            let rows: Vec<Vec<f32>> = text.lines().skip(1).map(|l| l.split(',').filter_map(|n| n.trim().parse().ok()).collect()).filter(|r: &Vec<f32>| r.len() >= 3).collect();
            let column = |c: usize| -> Vec<f32> { rows.iter().map(|r| r.get(c).copied().unwrap_or(0.0)).collect() };
            let n = rows.len();
            let m = measure::Measured { hz: column(0), db: column(1), certain: column(2).iter().map(|c| *c != 0.0).collect(), over_noise: column(3), off_level: column(4), eq: column(5), guessed: Vec::new(), peak: 0.0, arrived: 0.0, level: 0.0 };
            if n == 0 {
                fail(&format!("{path}: no measurement in it"));
            }
            let want = calibrate::wanted(&m, how);
            let fitted = calibrate::fit(&m.hz, &want.db, &want.counts, 48000.0, how);
            gains.set_all(&fitted.gains);
            gains.set_points(&fitted.points);
            println!("the correction (the curve lies {:.2} dB from what is wanted, {:.2} dB at the most):", fitted.off, fitted.most_off);
            show_gains(&gains, 48000);
            let (alone, set) = (calibrate::raw(&m), gains.get());
            let after: Vec<f32> = (0..n).map(|i| alone[i] + set.response_db(48000.0, m.hz[i] as f64)).collect();
            println!("      Hz   measured   wanted   the curve   with it");
            for i in (0..n).step_by(8) {
                println!("  {:>6.0}   {:>+6.1}   {:>+6.1}   {:>+6.1}   {:>+6.1}  {}", m.hz[i], alone[i], want.db[i], set.response_db(48000.0, m.hz[i] as f64), after[i], if want.reach[i] { "" } else { "-" });
            }
            println!("\"-\" marks what is out of reach. From flat within reach ({} frequencies): {:.2} dB as measured, {:.2} dB with the correction if the speakers do as the curve says (not measured)", want.reach.iter().filter(|r| **r).count(), calibrate::spread(&alone, &want.reach), calibrate::spread(&after, &want.reach));
        }
        Some("calibrations") => {
            let all = calibrate::calibrations();
            if all.is_empty() {
                println!("no calibration is kept yet: bottle-eq measure --mic <microphone> --from <device> --to <device> --correct --save <name>");
            }
            for c in all {
                println!("{:<20} {}: {}, measured {} with {}: {} points", c.name, c.kind, c.device, c.day, c.microphone, c.points.len());
            }
        }
        Some("run") => {
            let (source, target) = pair();
            // the app asks for exclusive mode: from the one who runs it first, then from Windows
            let take_it = exclusive || (cfg!(windows) && !shared && {
                print!(
                    "Bottle EQ wants \"{}\" for itself (exclusive mode): the delay is then about 6 ms instead of 44.\nNothing else can play to that device while Bottle EQ runs. Take it? [Y/n] ",
                    target.name
                );
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                let _ = std::io::stdin().read_line(&mut answer);
                !answer.trim().to_lowercase().starts_with('n')
            });
            let (mut equalizer, g) = (Eq::new(target.rate), gains.clone());
            let running = run(&source, &target, cushion_ms, take_it, |_, _| {}, move |sound, channels| equalizer.process(sound, channels, &g)).unwrap_or_else(|e| fail(&e));
            describe(&running, &source, &target);
            show_gains(&gains, target.rate);
            println!("A line sets the curve: \"mode rock\" (\"modes\" lists them), \"points 100:cut 1k:-2 12k:+8:spike\", \"1k +3\" (a band), \"flat\", \"off\", \"on\"; an empty line stops.");
            let stop = Arc::new(AtomicBool::new(false));
            let (s, g, rate) = (stop.clone(), gains.clone(), target.rate);
            std::thread::spawn(move || loop {
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    s.store(true, Ordering::Relaxed);
                    return;
                }
                let words: Vec<&str> = line.split_whitespace().collect();
                match words.as_slice() {
                    ["modes"] => match eq::modes() {
                        Ok(all) => println!("{}", all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")),
                        Err(e) => println!("{e}"),
                    },
                    ["mode", name @ ..] if !name.is_empty() => match eq::mode(&name.join(" ")) {
                        Ok(mode) => {
                            println!("mode: {}", mode.name);
                            g.set_all(&mode.gains);
                        }
                        Err(e) => println!("{e}"),
                    },
                    ["points", points @ ..] if !points.is_empty() => match points_of(&points.join(" ")) {
                        Ok(points) => g.set_points(&points),
                        Err(e) => println!("{e}"),
                    },
                    ["volume", db] if db.parse::<f32>().is_ok() => g.set_volume(db.parse().unwrap_or(0.0)),
                    ["flat"] => g.flat(),
                    ["off"] => g.on.store(false, Ordering::Relaxed),
                    ["on"] => g.on.store(true, Ordering::Relaxed),
                    [band, db] => match (band_of(band), db.parse::<f32>()) {
                        (Some(band), Ok(db)) => g.set(band, db),
                        _ => println!("not understood: a band is 1 to 10 or its centre (31.5 ... 16k), then its gain in dB"),
                    },
                    _ => println!("not understood: \"mode rock\", \"modes\", \"points 100:cut 1k:-2 12k:+8:spike\", \"1k +3\", \"flat\", \"off\", \"on\", or an empty line to stop"),
                }
                show_gains(&g, rate);
            });
            let began = Instant::now();
            let m = &running.meters;
            while !stop.load(Ordering::Relaxed) && seconds.is_none_or(|s| began.elapsed().as_secs_f32() < s) {
                std::thread::sleep(Duration::from_secs(1));
                let (mean, longest) = running.delay();
                println!(
                    "in {} dB  out {} dB  delay {:4.1} ms (longest {:4.1})  ring {:4.1} ms  reading {:+5.0} ppm  dry {}  dropped {}  late {}",
                    db(value(&m.peak_in)),
                    db(value(&m.peak_out)),
                    mean,
                    longest,
                    m.fill.load(Ordering::Relaxed) as f32 * 1000.0 / running.in_rate as f32,
                    value(&m.drift_ppm),
                    m.dry.load(Ordering::Relaxed),
                    m.dropped.load(Ordering::Relaxed),
                    m.late.load(Ordering::Relaxed)
                );
                m.peak_in.store(0, Ordering::Relaxed);
                m.peak_out.store(0, Ordering::Relaxed);
            }
            println!("stopped: {} frames captured at {} Hz, {} played at {} Hz", m.captured.load(Ordering::Relaxed), running.in_rate, m.played.load(Ordering::Relaxed), running.out_rate);
        }
        Some("selftest") => {
            let (source, target) = pair();
            const HZ: f64 = 1000.0;
            let length = seconds.unwrap_or(1.5) as f64;
            let heard = Arc::new(Mutex::new(Heard::default()));
            let (h, rate) = (heard.clone(), target.rate as f64);
            let running = run(&source, &target, cushion_ms, !shared, |_, _| {}, move |sound, channels| h.lock().unwrap().hear(sound, channels, HZ, rate)).unwrap_or_else(|e| fail(&e));
            describe(&running, &source, &target);
            std::thread::sleep(Duration::from_millis(400));
            if heard.lock().unwrap().began {
                fail("something is already playing to the source: the test needs it quiet");
            }
            // the tone: 1 kHz at -40 dB, played to the source like any program would
            println!("a quiet 1 kHz tone (-40 dB, {length} s) is played to the source ...");
            let tone = audio::tone(&source, HZ, 0.01, length);
            let _ = running.delay();
            std::thread::sleep(Duration::from_secs_f64(length + 0.3));
            let (mean, longest) = running.delay();
            let _ = tone.join();
            std::thread::sleep(Duration::from_millis(500));

            let h = heard.lock().unwrap();
            let m = &running.meters;
            let share = if h.squares > 0.0 { (h.tone / h.squares).min(1.0) } else { 0.0 };
            // a 1 kHz tone at this level moves at most this much from one frame to the next; a click moves more
            let smooth = 0.01 * std::f64::consts::TAU * HZ / rate;
            println!("captured at the source: peak {} dB", db(value(&m.peak_in)));
            match h.began {
                true => println!("handed to the target:   {} dB, {:.0}% of the sound is 1 kHz", db((2.0 * h.first / (rate / 2.0)).sqrt() as f32), 100.0 * share),
                false => println!("handed to the target:   nothing"),
            }
            println!(
                "loudest jump between two frames: {:.5} as captured ({:.2} s into the capture; the tone ran {:.1} s), {:.5} as handed to the target (the tone alone: {smooth:.5})",
                value(&m.jump_in),
                m.jump_at.load(Ordering::Relaxed) as f64 / running.in_rate as f64,
                length,
                h.jump
            );
            println!(
                "ring: {} frames captured, {} played, {} dropped, ran dry {} times, woken late {} times, {} breaks in what was captured, read {:+.0} ppm off nominal at the end",
                m.captured.load(Ordering::Relaxed),
                m.played.load(Ordering::Relaxed),
                m.dropped.load(Ordering::Relaxed),
                m.dry.load(Ordering::Relaxed),
                m.late.load(Ordering::Relaxed),
                m.breaks.load(Ordering::Relaxed),
                value(&m.drift_ppm)
            );
            println!("delay while the tone played: {mean:.1} ms on average, {longest:.1} ms at the longest (from the source's engine handing the sound over to the target beginning to play it)");
            // the path is judged by what it adds: a jump that was already in the captured sound is the
            // source's (or this test's tone's), and is said, not failed
            let captured = value(&m.jump_in) as f64;
            if captured > 1.5 * smooth {
                println!("NOTE: the captured sound itself had a jump; the path passed it on as it came");
            }
            let ok = h.began && share > 0.9 && h.jump < (1.5 * smooth).max(1.1 * captured) && m.dry.load(Ordering::Relaxed) == 0 && m.dropped.load(Ordering::Relaxed) == 0 && m.late.load(Ordering::Relaxed) == 0;
            println!("{}", if ok { "PASS" } else { "FAIL" });
            if !ok {
                std::process::exit(1);
            }
        }
        Some("modes") => {
            for mode in eq::modes().unwrap_or_else(|e| fail(&e)) {
                println!("{:<16} {}", mode.name, mode.gains.iter().map(|g| format!("{g:>+5.0}")).collect::<String>());
            }
        }
        Some("curve") => {
            // the curve the gains make: every third of an octave, or every twelfth as "hz,dB" with --csv
            let set = gains.get();
            let (rate, csv) = (48000.0, args.iter().any(|a| a == "--csv"));
            if !csv {
                show_gains(&gains, rate as u32);
            }
            let step = if csv { 12 } else { 3 };
            for i in 0..=10 * step {
                let hz = 20.0 * 2f64.powf(i as f64 / step as f64);
                if hz < 0.45 * rate {
                    let db = set.response_db(rate, hz);
                    match csv {
                        true => println!("{hz:.2},{db:.3}"),
                        false => println!("{hz:>8.0} Hz {db:>+6.2} dB  {}", "#".repeat(((db.max(-24.0) + 24.0) * 1.5).round() as usize)),
                    }
                }
            }
        }
        Some("playtest") => {
            // real music through the whole path, with what was captured and what was handed to the target
            // kept as two WAV files to look at and to listen to
            let (source, target) = pair();
            let file = file.unwrap_or_else(|| fail("playtest needs --file <wav: two channels, 32-bit float, at the source's rate>"));
            let (rate, mut music) = read_wav(&file).unwrap_or_else(|e| fail(&format!("{file}: {e}")));
            if rate != source.rate {
                fail(&format!("{file} is at {rate} Hz and the source at {} Hz", source.rate));
            }
            music.iter_mut().for_each(|s| *s *= level);
            let seconds = music.len() as f64 / 2.0 / rate as f64;
            let room = (seconds + 3.0) as usize;
            let captured = Arc::new(Mutex::new(Vec::<f32>::with_capacity(room * source.rate as usize * source.channels)));
            let handed = Arc::new(Mutex::new((Vec::<f32>::with_capacity(room * target.rate as usize * 2), 2usize)));
            let (c, h) = (captured.clone(), handed.clone());
            let (mut equalizer, g) = (Eq::new(target.rate), gains.clone());
            show_gains(&gains, target.rate);
            let running = run(
                &source,
                &target,
                cushion_ms,
                !shared,
                move |sound, _| {
                    let mut c = c.lock().unwrap();
                    if c.len() + sound.len() <= c.capacity() {
                        c.extend_from_slice(sound);
                    }
                },
                move |sound, channels| {
                    equalizer.process(sound, channels, &g);
                    let mut h = h.lock().unwrap();
                    h.1 = channels;
                    if h.0.len() + sound.len() <= h.0.capacity() {
                        h.0.extend_from_slice(sound);
                    }
                },
            )
            .unwrap_or_else(|e| fail(&e));
            describe(&running, &source, &target);
            std::thread::sleep(Duration::from_millis(500));
            println!("playing {file} ({seconds:.0} s, at {level} of its level) to the source ...");
            let playing = audio::play(&source, music);
            std::thread::sleep(Duration::from_secs(3));
            let _ = running.delay();
            let _ = playing.join();
            let (mean, longest) = running.delay();
            std::thread::sleep(Duration::from_millis(500));
            let m = &running.meters;
            println!(
                "{} frames captured, {} played, {} dropped, ran dry {} times, woken late {} times, {} breaks in what was captured, read {:+.0} ppm off nominal at the end",
                m.captured.load(Ordering::Relaxed),
                m.played.load(Ordering::Relaxed),
                m.dropped.load(Ordering::Relaxed),
                m.dry.load(Ordering::Relaxed),
                m.late.load(Ordering::Relaxed),
                m.breaks.load(Ordering::Relaxed),
                value(&m.drift_ppm)
            );
            println!("delay after the first 3 s: {mean:.1} ms on average, {longest:.1} ms at the longest");
            println!("peaks: {} dB captured, {} dB handed to the target", db(value(&m.peak_in)), db(value(&m.peak_out)));
            if let Some(dir) = record {
                let _ = std::fs::create_dir_all(&dir);
                let (c, h) = (captured.lock().unwrap(), handed.lock().unwrap());
                write_wav(&format!("{dir}/captured.wav"), source.rate, source.channels, &c).unwrap_or_else(|e| fail(&e));
                write_wav(&format!("{dir}/handed.wav"), target.rate, h.1, &h.0).unwrap_or_else(|e| fail(&e));
                println!("kept: {dir}/captured.wav (what came from the source), {dir}/handed.wav (what the target was given)");
            }
        }
        _ => fail("commands: window | devices | measure --mic <microphone> --to <device> [--correct --save <name>] | guess | import <folder> --type <kind> | training | calibrations | run --from <device> --to <device> | curve --gains <ten numbers> | selftest --from <device> --to <device> | playtest --from <device> --to <device> --file <wav>"),
    }
}
