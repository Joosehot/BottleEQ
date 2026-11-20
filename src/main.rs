//! Bottle EQ: a system-wide audio equalizer for Windows.
//!
//!   bottle-eq devices                                  the output devices and their formats
//!   bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other
//!   bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
//!
//! A device is its number in `devices` or a part of its name. Options: --buffer-ms <n> (the cushion of
//! sound kept between capture and playback, default 25), --seconds <n> (run: stop by itself; selftest:
//! the tone's length).
//!
//! Step 1 of 5: the stream flows, nothing is done to it yet.

mod audio;
mod ring;

use audio::{endpoints, find, pass, value, Endpoint};
use cpal::traits::{DeviceTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

fn db(x: f32) -> String {
    if x <= 0.0 { "  -inf".to_string() } else { format!("{:6.1}", 20.0 * x.log10()) }
}

/// What a listener heard on an endpoint (by loopback): when sound first came, and how much of it was the tone.
#[derive(Default)]
struct Heard {
    onset: Option<Instant>,
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

/// Listen to what an endpoint plays and look for a tone of `hz` in it.
fn listen(endpoint: &Endpoint, hz: f64) -> Result<(cpal::Stream, Arc<Mutex<Heard>>), String> {
    let heard = Arc::new(Mutex::new(Heard::default()));
    let (h, ch, rate) = (heard.clone(), endpoint.config.channels as usize, endpoint.config.sample_rate.0 as f64);
    let stream = endpoint
        .device
        .build_input_stream(
            &endpoint.config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut h = h.lock().unwrap();
                let frames = data.len() / ch;
                for (i, frame) in data.chunks(ch).enumerate() {
                    let s = frame[0] as f64;
                    if h.onset.is_none() && s.abs() > 0.002 {
                        // this block ended about now, so the frame was heard (frames - i) frames ago
                        h.onset = Some(Instant::now() - Duration::from_secs_f64((frames - i) as f64 / rate));
                    }
                    if h.onset.is_some() {
                        let angle = std::f64::consts::TAU * hz * h.n / rate;
                        h.squares += s * s;
                        if h.n < rate / 2.0 {
                            h.first += s * s;
                        }
                        h.sin += s * angle.sin();
                        h.cos += s * angle.cos();
                        h.n += 1.0;
                        h.jump = h.jump.max((s - h.last).abs());
                        if h.n % (rate / 10.0).floor() == 0.0 {
                            h.tone += 2.0 * (h.sin * h.sin + h.cos * h.cos) / (rate / 10.0).floor();
                            (h.sin, h.cos) = (0.0, 0.0);
                        }
                    }
                    h.last = s;
                }
            },
            |e| eprintln!("listen: {e}"),
            None,
        )
        .map_err(|e| format!("loopback capture on {}: {e}", endpoint.name))?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, heard))
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let (from, to) = (take("--from"), take("--to"));
    let buffer_ms: f32 = take("--buffer-ms").map(|s| s.parse().unwrap_or_else(|_| fail("--buffer-ms takes a number"))).unwrap_or(25.0);
    let seconds: Option<f32> = take("--seconds").map(|s| s.parse().unwrap_or_else(|_| fail("--seconds takes a number")));
    let pair = || {
        let (Some(from), Some(to)) = (&from, &to) else { fail("give --from <device> and --to <device> (see `bottle-eq devices`)") };
        let source = find(endpoints().unwrap_or_else(|e| fail(&e)), from).unwrap_or_else(|e| fail(&e));
        let target = find(endpoints().unwrap_or_else(|e| fail(&e)), to).unwrap_or_else(|e| fail(&e));
        (source, target)
    };

    match args.first().map(String::as_str) {
        Some("devices") => {
            for (i, e) in endpoints().unwrap_or_else(|e| fail(&e)).iter().enumerate() {
                println!("{}. {}{}  ({} channels, {} Hz, 32-bit float)", i + 1, e.name, if e.default { "  [Windows plays here]" } else { "" }, e.config.channels, e.config.sample_rate.0);
            }
        }
        Some("run") => {
            let (source, target) = pair();
            let running = pass(&source, &target, buffer_ms, |_, _| {}).unwrap_or_else(|e| fail(&e));
            println!("capturing what plays to: {}\nplaying it to:           {}\ncushion {buffer_ms} ms. Press Enter to stop.", source.name, target.name);
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
                println!(
                    "in {} dB  out {} dB  ring {:5.1} ms  reading {:+6.0} ppm  dry {}  dropped {}",
                    db(value(&m.peak_in)),
                    db(value(&m.peak_out)),
                    m.fill.load(Ordering::Relaxed) as f32 * 1000.0 / running.in_rate as f32,
                    value(&m.drift_ppm),
                    m.dry.load(Ordering::Relaxed),
                    m.dropped.load(Ordering::Relaxed)
                );
                m.peak_in.store(0, Ordering::Relaxed);
                m.peak_out.store(0, Ordering::Relaxed);
            }
            println!("stopped: {} frames captured at {} Hz, {} played at {} Hz", m.captured.load(Ordering::Relaxed), running.in_rate, m.played.load(Ordering::Relaxed), running.out_rate);
        }
        Some("selftest") => {
            let (source, target) = pair();
            const HZ: f64 = 1000.0;
            let running = pass(&source, &target, buffer_ms, |_, _| {}).unwrap_or_else(|e| fail(&e));
            let (_at_source, sent) = listen(&source, HZ).unwrap_or_else(|e| fail(&e));
            let (_at_target, arrived) = listen(&target, HZ).unwrap_or_else(|e| fail(&e));
            std::thread::sleep(Duration::from_millis(300));
            if sent.lock().unwrap().onset.is_some() || arrived.lock().unwrap().onset.is_some() {
                fail("something is already playing on one of the two devices: the test needs them quiet");
            }
            // the tone: 1 kHz at -40 dB, played to the source like any program would
            let length = seconds.unwrap_or(1.5) as f64;
            let (ch, rate) = (source.config.channels as usize, source.config.sample_rate.0 as f64);
            let mut n = 0u64;
            let tone = source
                .device
                .build_output_stream(
                    &source.config,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        for frame in out.chunks_mut(ch) {
                            let t = n as f64 / rate;
                            let s = if (0.5..0.5 + length).contains(&t) { 0.01 * (std::f64::consts::TAU * HZ * t).sin() } else { 0.0 };
                            frame.fill(s as f32);
                            n += 1;
                        }
                    },
                    |e| eprintln!("tone: {e}"),
                    None,
                )
                .unwrap_or_else(|e| fail(&format!("tone on {}: {e}", source.name)));
            tone.play().unwrap_or_else(|e| fail(&e.to_string()));
            println!("a quiet 1 kHz tone (-40 dB, {length} s): {} -> loopback -> ring -> {}", source.name, target.name);
            std::thread::sleep(Duration::from_secs_f64(length + 1.5));

            let share = |h: &Heard| if h.squares > 0.0 { (h.tone / h.squares).min(1.0) } else { 0.0 };
            let level = |h: &Heard, rate: u32| db((2.0 * h.first / (rate as f64 / 2.0)).sqrt() as f32);
            let (sent, arrived) = (sent.lock().unwrap(), arrived.lock().unwrap());
            let m = &running.meters;
            println!("at the source: {}", if sent.onset.is_some() { format!("tone heard at {} dB, {:.0}% of the sound is 1 kHz", level(&sent, source.config.sample_rate.0), 100.0 * share(&sent)) } else { "nothing heard".to_string() });
            println!("at the target: {}", if arrived.onset.is_some() { format!("tone heard at {} dB, {:.0}% of the sound is 1 kHz", level(&arrived, target.config.sample_rate.0), 100.0 * share(&arrived)) } else { "nothing heard".to_string() });
            // a 1 kHz tone at this level moves at most this much from one frame to the next; a click moves more
            let smooth = 0.01 * std::f64::consts::TAU * HZ / target.config.sample_rate.0 as f64;
            println!("loudest jump between two frames at the target: {:.5} (the tone alone: {smooth:.5})", arrived.jump);
            println!(
                "ring: {} frames captured, {} played, {} dropped, ran dry {} times, read {:+.0} ppm off nominal at the end",
                m.captured.load(Ordering::Relaxed),
                m.played.load(Ordering::Relaxed),
                m.dropped.load(Ordering::Relaxed),
                m.dry.load(Ordering::Relaxed),
                value(&m.drift_ppm)
            );
            match (sent.onset, arrived.onset) {
                (Some(a), Some(b)) if share(&arrived) > 0.9 && arrived.jump < 2.0 * smooth => {
                    println!("delay added by the path: {:.0} ms (cushion {buffer_ms} ms)", b.saturating_duration_since(a).as_secs_f64() * 1000.0);
                    println!("PASS");
                }
                _ => {
                    println!("FAIL");
                    std::process::exit(1);
                }
            }
        }
        _ => fail("commands: devices | run --from <device> --to <device> | selftest --from <device> --to <device>"),
    }
}
