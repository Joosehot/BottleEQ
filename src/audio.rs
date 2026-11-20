//! Step 1: system audio in, system audio out, nothing done to it yet.
//!
//! # WASAPI loopback, plainly
//!
//! Windows mixes every program's sound for one output device (an "endpoint": the speakers, the monitor,
//! S/PDIF) into one stream in that device's own format (its "mix format", here 32-bit float). WASAPI is
//! the API for that; in "shared mode" a program hands its sound to the mixer or takes sound from it.
//!
//! Loopback is a capture stream opened on an *output* endpoint: WASAPI then hands us a copy of the
//! finished mix that is on its way to that device. (cpal does this for us: asked for an input stream on
//! an output device, it initializes the IAudioClient with AUDCLNT_STREAMFLAGS_LOOPBACK.) Four things
//! follow, and all four shape this file:
//!
//! 1. **It is a copy.** The device still plays the original; loopback cannot change what that device
//!    plays. So an equalizer built on loopback must capture from one endpoint (the *source*: what
//!    Windows and the programs play to) and play the processed sound to another (the *target*: what you
//!    listen to). Same endpoint for both would play the sound twice and then capture its own output
//!    again, round and round.
//! 2. **Silence is no data.** When nothing is playing to the source, loopback delivers nothing at all,
//!    not zeros. The playback side must expect the ring to run dry and play silence, then wait for a
//!    cushion of sound before it starts again.
//! 3. **Two devices, two clocks.** "48000 Hz" on the source and on the target are two crystals that
//!    differ by a few parts in a hundred thousand. Uncorrected, the ring between them slowly fills or
//!    empties until it clicks. The playback side reads the ring a hair faster or slower to hold its
//!    fill at a target (a 4-point interpolating read), which also covers devices with different rates.
//! 4. **Formats may differ.** Each endpoint has its own channel count and rate. Channels are mapped
//!    (front left/right kept); rates are covered by the same interpolating read.
//!
//! The path: capture callback -> ring -> playback callback. Added delay = one capture period (about
//! 10 ms) + the ring's cushion (`buffer_ms`) + one playback period (about 10 ms).

use crate::ring::Ring;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// What the two audio threads tell the rest of the program.
#[derive(Default)]
pub struct Meters {
    pub captured: AtomicU64,
    pub played: AtomicU64,
    /// frames dropped because the ring was full (playback behind)
    pub dropped: AtomicU64,
    /// times the ring ran dry while playing (a gap in the sound, or simply nothing playing any more)
    pub dry: AtomicU64,
    /// frames in the ring, as the playback callback last saw it
    pub fill: AtomicU64,
    /// how much faster (+) or slower (-) than nominal the ring is being read, in parts per million
    pub drift_ppm: AtomicU32,
    pub peak_in: AtomicU32,
    pub peak_out: AtomicU32,
}

fn bits(x: f32) -> u32 {
    x.to_bits()
}

pub fn value(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

pub struct Endpoint {
    pub device: Device,
    pub name: String,
    pub config: StreamConfig,
    pub default: bool,
}

/// Every active output endpoint, with its mix format.
pub fn endpoints() -> Result<Vec<Endpoint>, String> {
    let host = cpal::default_host();
    let default = host.default_output_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    for device in host.output_devices().map_err(|e| e.to_string())? {
        let name = device.name().map_err(|e| e.to_string())?;
        let supported = device.default_output_config().map_err(|e| format!("{name}: {e}"))?;
        if supported.sample_format() != SampleFormat::F32 {
            return Err(format!("{name}: its mix format is {:?}, not 32-bit float", supported.sample_format()));
        }
        out.push(Endpoint { default: default.as_deref() == Some(name.as_str()), config: supported.config(), name, device });
    }
    Ok(out)
}

/// An endpoint by its number in the list or by a part of its name.
pub fn find(endpoints: Vec<Endpoint>, wanted: &str) -> Result<Endpoint, String> {
    let names: Vec<String> = endpoints.iter().map(|e| e.name.clone()).collect();
    let by_number = wanted.parse::<usize>().ok().filter(|n| (1..=endpoints.len()).contains(n)).map(|n| n - 1);
    let by_name = || endpoints.iter().position(|e| e.name.to_lowercase().contains(&wanted.to_lowercase()));
    match by_number.or_else(by_name) {
        Some(i) => Ok(endpoints.into_iter().nth(i).unwrap()),
        None => Err(format!("no output device matches \"{wanted}\" (there are: {})", names.join("; "))),
    }
}

/// 4-point, 3rd-order Hermite: the value `t` of the way from `b` to `c`.
fn hermite(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let c0 = b;
    let c1 = 0.5 * (c - a);
    let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
    let c3 = 0.5 * (d - a) + 1.5 * (b - c);
    ((c3 * t + c2) * t + c1) * t + c0
}

/// The running passthrough: it lives as long as this is kept.
pub struct Pass {
    _capture: Stream,
    _playback: Stream,
    pub meters: Arc<Meters>,
    pub out_rate: u32,
    pub in_rate: u32,
}

/// Capture the source's mix by loopback and play it to the target. `process` is called in the playback
/// thread on every block of interleaved output frames just before they are played (step 1: nothing).
pub fn pass(source: &Endpoint, target: &Endpoint, buffer_ms: f32, mut process: impl FnMut(&mut [f32], usize) + Send + 'static) -> Result<Pass, String> {
    if source.name == target.name {
        return Err("the source and the target are the same device: loopback is a copy of what that device plays, so playing it back there would double it and feed it round again".to_string());
    }
    let (in_ch, out_ch) = (source.config.channels as usize, target.config.channels as usize);
    let (in_rate, out_rate) = (source.config.sample_rate.0, target.config.sample_rate.0);
    let ring = Arc::new(Ring::new(in_rate as usize * in_ch)); // one second
    let meters = Arc::new(Meters::default());

    // capture: the source's finished mix, as WASAPI hands it over, straight into the ring
    let (r, m) = (ring.clone(), meters.clone());
    let capture = source
        .device
        .build_input_stream(
            &source.config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let fitted = r.push(data);
                m.captured.fetch_add((fitted / in_ch) as u64, Ordering::Relaxed);
                m.dropped.fetch_add(((data.len() - fitted) / in_ch) as u64, Ordering::Relaxed);
                let peak = data.iter().fold(0.0f32, |p, s| p.max(s.abs()));
                m.peak_in.store(bits(value(&m.peak_in).max(peak)), Ordering::Relaxed);
            },
            |e| eprintln!("capture: {e}"),
            None,
        )
        .map_err(|e| format!("loopback capture on {}: {e}", source.name))?;

    // playback: read the ring at the target's rate, holding the ring's fill at the cushion
    let (r, m) = (ring, meters.clone());
    let cushion = (buffer_ms / 1000.0 * in_rate as f32) as usize; // frames
    let nominal = in_rate as f64 / out_rate as f64;
    let (mut phase, mut primed, mut usual) = (0.0f64, false, cushion as f64);
    let playback = target
        .device
        .build_output_stream(
            &target.config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let fill = r.len() / in_ch;
                m.fill.store(fill as u64, Ordering::Relaxed);
                if !primed && fill >= cushion {
                    (primed, phase, usual) = (true, 0.0, fill as f64);
                }
                // far too full (playback was held up): let go of the oldest sound at once
                if fill > 4 * cushion.max(64) {
                    r.skip((fill - cushion) * in_ch);
                    m.dropped.fetch_add((fill - cushion) as u64, Ordering::Relaxed);
                    usual = cushion as f64;
                }
                // The fill swings by a whole block from one callback to the next, so it is its slow average
                // (about a second) that is held at the cushion: an error of 10 ms is taken out over about
                // ten seconds, by reading at most 0.1% fast or slow. That is under 2 cents of pitch.
                usual += 0.01 * (fill as f64 - usual);
                let speed = ((usual - cushion as f64) / in_rate as f64 / 10.0).clamp(-0.001, 0.001);
                m.drift_ppm.store(bits((speed * 1e6) as f32), Ordering::Relaxed);
                let step = nominal * (1.0 + speed);
                let mut frames = 0;
                for frame in out.chunks_mut(out_ch) {
                    if primed && r.len() / in_ch < 4 {
                        primed = false;
                        m.dry.fetch_add(1, Ordering::Relaxed);
                    }
                    if !primed {
                        frame.fill(0.0);
                        continue;
                    }
                    for (c, s) in frame.iter_mut().enumerate() {
                        // front left and right are kept; a mono source goes to both; other channels stay silent
                        let from = if c < in_ch.min(2) || (c < in_ch && in_ch == out_ch) { Some(c) } else if in_ch == 1 && c < 2 { Some(0) } else { None };
                        *s = from.map_or(0.0, |c| hermite(r.peek(c), r.peek(in_ch + c), r.peek(2 * in_ch + c), r.peek(3 * in_ch + c), phase as f32));
                    }
                    phase += step;
                    let whole = phase as usize;
                    r.skip(whole.min(r.len() / in_ch) * in_ch);
                    phase -= whole as f64;
                    frames += 1;
                }
                process(out, out_ch);
                m.played.fetch_add(frames, Ordering::Relaxed);
                let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
                m.peak_out.store(bits(value(&m.peak_out).max(peak)), Ordering::Relaxed);
            },
            |e| eprintln!("playback: {e}"),
            None,
        )
        .map_err(|e| format!("playback on {}: {e}", target.name))?;

    capture.play().map_err(|e| e.to_string())?;
    playback.play().map_err(|e| e.to_string())?;
    Ok(Pass { _capture: capture, _playback: playback, meters, in_rate, out_rate })
}
