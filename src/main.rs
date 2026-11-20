//! Bottle EQ: a system-wide audio equalizer for Windows.
//!
//!   bottle-eq devices                                  the output devices, their formats and blocks
//!   bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other
//!   bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
//!
//! A device is its number in `devices` or a part of its name. Options:
//!   --exclusive | --shared   take the target for Bottle EQ alone (short delay) or share it (long delay);
//!                            without either, `run` asks
//!   --cushion-ms <n>         sound kept in the ring on top of what the devices' blocks need (default 3)
//!   --seconds <n>            run: stop by itself; selftest: the tone's length
//!
//! Step 1 of 5: the stream flows, nothing is done to it yet.

mod ring;
mod wasapi;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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

/// The delay Bottle EQ adds, in milliseconds, (mean, longest), since the last look: the wait from a
/// frame arriving from the source's engine to its being handed to the target, plus the target's block
/// (in exclusive mode a buffer handed over begins to play when the one before it ends).
fn delay(running: &Running) -> (f32, f32) {
    let (mean, longest) = running.waited();
    let block = running.opened.out_block as f32 * 1000.0 / running.out_rate as f32;
    (mean + block, longest + block)
}

fn describe(running: &Running, source: &Output, target: &Output) {
    let o = &running.opened;
    println!("capturing what plays to: {}  (in blocks of {} frames, {:.2} ms)", source.name, o.in_block, o.in_block as f32 * 1000.0 / running.in_rate as f32);
    println!(
        "playing it to:           {}  ({}, blocks of {} frames, {:.2} ms, {})",
        target.name,
        if o.exclusive { "exclusive mode" } else { "shared mode" },
        o.out_block,
        o.out_block as f32 * 1000.0 / running.out_rate as f32,
        if o.exclusive { format!("{}-bit", o.out_bits) } else { "32-bit float".to_string() }
    );
    if !o.exclusive && !o.note.is_empty() {
        println!("exclusive mode was not to be had: {}", o.note);
    }
    let with = running.meters.priority.load(Ordering::Relaxed);
    println!("audio threads with Windows' Pro Audio priority: {with} of 2");
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let (from, to) = (take("--from"), take("--to"));
    let cushion_ms: f32 = take("--cushion-ms").map(|s| s.parse().unwrap_or_else(|_| fail("--cushion-ms takes a number"))).unwrap_or(3.0);
    let seconds: Option<f32> = take("--seconds").map(|s| s.parse().unwrap_or_else(|_| fail("--seconds takes a number")));
    let mut flag = |f: &str| match args.iter().position(|a| a == f) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    };
    let (exclusive, shared) = (flag("--exclusive"), flag("--shared"));
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
                    "Bottle EQ wants \"{}\" for itself (exclusive mode): the delay is then about 12 ms instead of 44.\nNothing else can play to that device while Bottle EQ runs. Take it? [Y/n] ",
                    target.name
                );
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                let _ = std::io::stdin().read_line(&mut answer);
                !answer.trim().to_lowercase().starts_with('n')
            });
            let running = run(&source, &target, cushion_ms, take_it, |_, _| {}).unwrap_or_else(|e| fail(&e));
            describe(&running, &source, &target);
            println!("Press Enter to stop.");
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            std::thread::spawn(move || {
                let _ = std::io::stdin().read_line(&mut String::new());
                s.store(true, Ordering::Relaxed);
            });
            let began = Instant::now();
            let m = &running.meters;
            while !stop.load(Ordering::Relaxed) && seconds.is_none_or(|s| began.elapsed().as_secs_f32() < s) {
                std::thread::sleep(Duration::from_secs(1));
                let (mean, longest) = delay(&running);
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
            let running = run(&source, &target, cushion_ms, !shared, move |sound, channels| h.lock().unwrap().hear(sound, channels, HZ, rate)).unwrap_or_else(|e| fail(&e));
            describe(&running, &source, &target);
            std::thread::sleep(Duration::from_millis(400));
            if heard.lock().unwrap().began {
                fail("something is already playing to the source: the test needs it quiet");
            }
            // the tone: 1 kHz at -40 dB, played to the source like any program would
            println!("a quiet 1 kHz tone (-40 dB, {length} s) is played to the source ...");
            let tone = wasapi::tone(&source, HZ, 0.01, length);
            let _ = running.waited();
            std::thread::sleep(Duration::from_secs_f64(length + 0.3));
            let (mean, longest) = delay(&running);
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
            println!("loudest jump between two frames: {:.5} as captured, {:.5} as handed to the target (the tone alone: {smooth:.5})", value(&m.jump_in), h.jump);
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
            let ok = h.began && share > 0.9 && h.jump < 1.5 * smooth && m.dry.load(Ordering::Relaxed) == 0 && m.dropped.load(Ordering::Relaxed) == 0 && m.late.load(Ordering::Relaxed) == 0;
            println!("{}", if ok { "PASS" } else { "FAIL" });
            if !ok {
                std::process::exit(1);
            }
        }
        _ => fail("commands: devices | run --from <device> --to <device> | selftest --from <device> --to <device>"),
    }
}
