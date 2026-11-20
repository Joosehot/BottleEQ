//! Bottle EQ: a system-wide audio equalizer for Windows.
//!
//!   bottle-eq devices                                  the output devices, their formats and blocks
//!   bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other
//!   bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
//!   bottle-eq playtest --from <device> --to <device> --file <wav> [--record <folder>] [--level <0..1>]
//!                                                      music through the whole path; what was captured and
//!                                                      what the target was given are kept as WAV files
//!
//! A device is its number in `devices` or a part of its name. Options:
//!   --exclusive | --shared   take the target for Bottle EQ alone (short delay) or share it (long delay);
//!                            without either, `run` asks
//!   --cushion-ms <n>         sound kept queued on top of what the devices' blocks need (default 2)
//!   --seconds <n>            run: stop by itself; selftest: the tone's length
//!
//!   --gains <ten numbers>    the ten bands' gains in dB, low to high, e.g. --gains 4,2,0,0,0,0,0,0,2,4
//!
//!   bottle-eq curve --gains <ten numbers>              the curve those gains make, every third of an octave
//!
//! While `run` plays, a line sets a band: "1k +3", "31.5 -2", "8 4" (the 8th band), "flat", "off", "on";
//! an empty line stops.
//!
//! Step 2 of 5: the equalizer.

mod eq;
mod ring;
mod wasapi;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use eq::{Eq, Gains, BANDS, CENTRES};
use wasapi::{find, outputs, run, value, Output, Running};

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

/// A band named as its place (1 to 10) or its centre ("31.5", "1k", "16000").
fn band_of(name: &str) -> Option<usize> {
    let hz: f32 = match name.to_lowercase().strip_suffix('k') {
        Some(thousands) => thousands.parse::<f32>().ok()? * 1000.0,
        None => name.parse().ok()?,
    };
    if hz.fract() == 0.0 && (1.0..=BANDS as f32).contains(&hz) {
        return Some(hz as usize - 1);
    }
    CENTRES.iter().position(|c| (c / hz).log2().abs() < 0.25)
}

fn show_gains(gains: &Gains, rate: u32) {
    let set = gains.get();
    let names: Vec<String> = CENTRES.iter().zip(set).map(|(c, g)| format!("{}:{g:+.1}", if *c >= 1000.0 { format!("{}k", c / 1000.0) } else { format!("{c}") })).collect();
    match gains.on.load(Ordering::Relaxed) {
        true => println!("bands (dB): {}  | turned down {:.1} dB for headroom", names.join(" "), -eq::headroom(&set, rate as f64)),
        false => println!("bands (dB): {}  | off: the sound passes untouched", names.join(" ")),
    }
}

fn describe(running: &Running, source: &Output, target: &Output) {
    let o = &running.opened;
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
    let cushion_ms: f32 = take("--cushion-ms").map(|s| s.parse().unwrap_or_else(|_| fail("--cushion-ms takes a number"))).unwrap_or(2.0);
    let (file, record) = (take("--file"), take("--record"));
    let level: f32 = take("--level").map(|s| s.parse().unwrap_or_else(|_| fail("--level takes a number"))).unwrap_or(1.0);
    let gains_text = take("--gains");
    let seconds: Option<f32> = take("--seconds").map(|s| s.parse().unwrap_or_else(|_| fail("--seconds takes a number")));
    let mut flag = |f: &str| match args.iter().position(|a| a == f) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    };
    let (exclusive, shared) = (flag("--exclusive"), flag("--shared"));
    let gains = Arc::new(Gains::default());
    let given = gains_text.map(|g| parse_gains(&g));
    given.iter().flatten().enumerate().for_each(|(b, g)| gains.set(b, *g));
    let pair = || {
        let (Some(from), Some(to)) = (&from, &to) else { fail("give --from <device> and --to <device> (see `bottle-eq devices`)") };
        (find(from).unwrap_or_else(|e| fail(&e)), find(to).unwrap_or_else(|e| fail(&e)))
    };

    match args.first().map(String::as_str) {
        Some("devices") => {
            for (i, o) in outputs().unwrap_or_else(|e| fail(&e)).iter().enumerate() {
                let ms = |frames: u32| frames as f32 * 1000.0 / o.rate as f32;
                println!(
                    "{}. {}{}  ({} channels, {} Hz; shared blocks {:.2} ms, least {:.2} ms)",
                    i + 1,
                    o.name,
                    if o.default { "  [Windows plays here]" } else { "" },
                    o.channels,
                    o.rate,
                    ms(o.periods.0),
                    ms(o.periods.1)
                );
            }
        }
        Some("run") => {
            let (source, target) = pair();
            // the app asks for exclusive mode: from the one who runs it first, then from Windows
            let take_it = exclusive || (!shared && {
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
            println!("A line sets a band (\"1k +3\", \"flat\", \"off\", \"on\"); an empty line stops.");
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
                    ["flat"] => g.flat(),
                    ["off"] => g.on.store(false, Ordering::Relaxed),
                    ["on"] => g.on.store(true, Ordering::Relaxed),
                    [band, db] => match (band_of(band), db.parse::<f32>()) {
                        (Some(band), Ok(db)) => g.set(band, db),
                        _ => println!("not understood: a band is 1 to 10 or its centre (31.5 ... 16k), then its gain in dB"),
                    },
                    _ => println!("not understood: \"1k +3\", \"flat\", \"off\", \"on\", or an empty line to stop"),
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
            let tone = wasapi::tone(&source, HZ, 0.01, length);
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
                    let db = eq::response_db(&set, rate, hz);
                    match csv {
                        true => println!("{hz:.2},{db:.3}"),
                        false => println!("{hz:>8.0} Hz {db:>+6.2} dB  {}", "#".repeat(((db + 12.0) * 2.0).round().max(0.0) as usize)),
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
            let playing = wasapi::play(&source, music);
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
        _ => fail("commands: devices | run --from <device> --to <device> | curve --gains <ten numbers> | selftest --from <device> --to <device> | playtest --from <device> --to <device> --file <wav>"),
    }
}
