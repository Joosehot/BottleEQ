//! System audio in, system audio out, on PipeWire (Linux).
//!
//! # How the sound gets through here, plainly
//!
//! PipeWire is one graph: every program that plays is a node in it, every device is a node, and the
//! session manager draws the links between them. The whole graph runs in cycles, one "quantum" of
//! frames at a time (1024 unless someone asks for less), every node of it within the same cycle.
//!
//! So nothing has to be listened to from the side, as on Windows. Bottle EQ puts two nodes of its own
//! into the graph:
//!
//! 1. **a device of its own**, "Bottle EQ" (a stream of the class `Audio/Sink`): programs play to it
//!    like to any sound card, and it is chosen as the output device in the system's sound settings. It
//!    is there while Bottle EQ runs;
//! 2. **a playback stream** to the device that is listened to (the *target*).
//!
//! In each cycle the first is handed what the programs played; it gives that to `process` (the
//! equalizer) and wakes the second (`trigger_process`), which hands it on to the target, still within
//! the same cycle. Three things follow:
//!
//! - **One clock.** The two nodes are in one group, so they run from the same driver at the same rate:
//!   nothing is read between the samples and no cushion is kept (compare `wasapi.rs`).
//! - **The delay is the graph's.** Bottle EQ holds no sound from one cycle to the next. What is said as
//!   the delay is what PipeWire says of the playback stream: how long what was just handed over takes
//!   to reach the device, as for any program that plays.
//! - **The quantum is asked for, not taken.** Both nodes ask for 128 frames (2.7 ms); the graph runs at
//!   the least any of its nodes asks for, within what the system allows.
//!
//! The sound is 32-bit float, two channels, 48,000 Hz; where the graph runs at another rate PipeWire's
//! own converter stands between.

use super::{bits, value, Meters, Opened, Output, Running};
use crate::ring::Ring;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// the name of Bottle EQ's own device in the graph
const OWN: &str = "bottle_eq";
const RATE: u32 = 48000;
const CHANNELS: usize = 2;
/// the quantum asked for, in frames
const QUANTUM: u32 = 128;
/// the most frames taken from one block (a quantum is 8192 at the very most)
const MOST: usize = 8192;

/// The one format spoken: 32-bit float, two channels (left, right), 48,000 Hz.
fn format() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Object { type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(), id: spa::param::ParamType::EnumFormat.as_raw(), properties: info.into() };
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object)).expect("the format is written").0.into_inner()
}

/// A connection to PipeWire: its loop, and the core at the other end.
fn connect() -> Result<(pw::main_loop::MainLoopRc, pw::core::CoreRc), String> {
    pw::init();
    let wrong = |e: pw::Error| format!("PipeWire is not to be reached (is it running?): {e}");
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(wrong)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(wrong)?;
    let core = context.connect_rc(None).map_err(wrong)?;
    Ok((mainloop, core))
}

/// Run the loop until PipeWire has answered everything asked so far.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> Result<(), String> {
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0).map_err(|e| e.to_string())?;
    let (d, l) = (done.clone(), mainloop.clone());
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                d.set(true);
                l.quit();
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
    Ok(())
}

/// The name in a default-device value, which is written `{"name":"alsa_output..."}`.
fn name_in(value: &str) -> Option<String> {
    let rest = &value[value.find("\"name\"")? + 6..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
}

/// Bottle EQ's own device first (what is captured from), then every output device of the graph.
pub fn outputs() -> Result<Vec<Output>, String> {
    let (mainloop, core) = connect()?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    let sinks = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let default = Rc::new(RefCell::new(None::<String>));
    // the metadata objects bound, kept alive with their listeners until the answers are in
    let kept = Rc::new(RefCell::new(Vec::new()));
    let (s, d, k, r) = (sinks.clone(), default.clone(), kept.clone(), registry.downgrade());
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            match global.type_ {
                pw::types::ObjectType::Node if props.get("media.class") == Some("Audio/Sink") => {
                    let name = props.get("node.name").unwrap_or_default().to_string();
                    let said = props.get("node.description").or(props.get("node.nick")).unwrap_or(&name).to_string();
                    s.borrow_mut().push((name, said));
                }
                // where the system plays is kept in the metadata called "default"
                pw::types::ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                    let Some(registry) = r.upgrade() else { return };
                    if let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) {
                        let d = d.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_, key, _, value| {
                                if key == Some("default.audio.sink") {
                                    *d.borrow_mut() = value.and_then(name_in);
                                }
                                0
                            })
                            .register();
                        k.borrow_mut().push((metadata, listener));
                    }
                }
                _ => {}
            }
        })
        .register();
    // once for the objects, once more for what the metadata bound in the first holds
    roundtrip(&mainloop, &core)?;
    roundtrip(&mainloop, &core)?;
    let default = default.borrow().clone();
    let device = |id: &str, name: String| Output { default: default.as_deref() == Some(id), id: id.to_string(), name, channels: CHANNELS, rate: RATE, periods: (0, 0) };
    let mut out = vec![device(OWN, "Bottle EQ (its own device, there while it runs)".to_string())];
    out.extend(sinks.borrow().iter().filter(|(id, _)| id != OWN).map(|(id, name)| device(id, name.clone())));
    Ok(out)
}

/// The loop is left when `stop` is set: it is looked at twenty times a second.
fn until_stopped(mainloop: &pw::main_loop::MainLoopRc, stop: impl Fn() -> bool + 'static) {
    let l = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop() {
            l.quit();
        }
    });
    let _ = timer.update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(50)));
    mainloop.run();
}

/// The frames in a block that was handed over: its samples, as they lie in its bytes.
fn samples(bytes: &[u8], into: &mut Vec<f32>) {
    into.clear();
    into.extend(bytes.chunks_exact(4).take(MOST * CHANNELS).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
}

/// The whole path, in the thread that calls it, until `stop`.
fn path(
    target: &Output,
    mut listen: impl FnMut(&[f32], usize) + 'static,
    mut process: impl FnMut(&mut [f32], usize) + 'static,
    meters: &Arc<Meters>,
    stop: &Arc<AtomicBool>,
    said: &Sender<Result<Opened, String>>,
) -> Result<(), String> {
    let (mainloop, core) = connect()?;
    // one group: the two nodes run in the same cycle from the same driver, and are never linked to
    // each other by the session manager
    let group = format!("bottle-eq-{}", std::process::id());
    let latency = format!("{QUANTUM}/{RATE}");
    let playback = pw::stream::StreamRc::new(
        core.clone(),
        "bottle-eq-out",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "bottle_eq_out",
            *pw::keys::NODE_DESCRIPTION => "Bottle EQ, to the speakers",
            *pw::keys::TARGET_OBJECT => target.id.clone(),
            *pw::keys::NODE_DONT_RECONNECT => "true",
            *pw::keys::NODE_GROUP => group.clone(),
            *pw::keys::NODE_LINK_GROUP => group.clone(),
            *pw::keys::NODE_LATENCY => latency.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    let capture = pw::stream::StreamRc::new(
        core.clone(),
        "bottle-eq",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => OWN,
            *pw::keys::NODE_DESCRIPTION => "Bottle EQ",
            *pw::keys::NODE_VIRTUAL => "true",
            *pw::keys::AUDIO_CHANNELS => "2",
            "audio.position" => "FL,FR",
            *pw::keys::NODE_GROUP => group.clone(),
            *pw::keys::NODE_LINK_GROUP => group,
            *pw::keys::NODE_LATENCY => latency,
        },
    )
    .map_err(|e| e.to_string())?;

    // between the two, within one cycle: what came in, equalized, waiting to be handed on
    let between = Rc::new(Ring::new(2 * MOST * CHANNELS));

    // in: what the programs played to Bottle EQ's device
    let (m, ring, out) = (meters.clone(), between.clone(), playback.clone());
    let (mut sound, mut last) = (Vec::with_capacity(MOST * CHANNELS), 0f32);
    let _in = capture
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(data) = buffer.datas_mut().first_mut() else { return };
                let (from, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data() else { return };
                let to = (from + size).min(bytes.len());
                samples(&bytes[from.min(to)..to], &mut sound);
            }
            let frames = sound.len() / CHANNELS;
            listen(&sound, CHANNELS);
            let (mut peak, mut jump) = (value(&m.peak_in), value(&m.jump_in));
            for (i, frame) in sound.chunks(CHANNELS).enumerate() {
                peak = frame.iter().fold(peak, |p, s| p.max(s.abs()));
                if (frame[0] - last).abs() > jump {
                    jump = (frame[0] - last).abs();
                    m.jump_at.store(m.captured.load(Ordering::Relaxed) + i as u64, Ordering::Relaxed);
                }
                last = frame[0];
            }
            m.peak_in.store(bits(peak), Ordering::Relaxed);
            m.jump_in.store(bits(jump), Ordering::Relaxed);
            m.captured.fetch_add(frames as u64, Ordering::Relaxed);
            process(&mut sound, CHANNELS);
            let fitted = ring.push(&sound);
            m.dropped.fetch_add(((sound.len() - fitted) / CHANNELS) as u64, Ordering::Relaxed);
            // on to the target, within this same cycle
            let _ = out.trigger_process();
        })
        .register()
        .map_err(|e| e.to_string())?;

    // out: to the target
    let (m, ring, sent) = (meters.clone(), between.clone(), Cell::new(false));
    let tell = said.clone();
    let _out = playback
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, _, now| match now {
            // the stream is made and linked: the path is open
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming if !sent.get() => {
                sent.set(true);
                let _ = tell.send(Ok(Opened { in_block: QUANTUM, out_block: QUANTUM, exclusive: false, out_bits: 32, note: String::new(), direct: true }));
            }
            pw::stream::StreamState::Error(why) => {
                let _ = tell.send(Err(format!("playing to the target: {why}")));
                eprintln!("the audio path stopped: {why}");
            }
            _ => {}
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let stride = 4 * CHANNELS;
            let frames = {
                let Some(bytes) = data.data() else { return };
                let frames = (ring.len() / CHANNELS).min(bytes.len() / stride);
                let mut peak = value(&m.peak_out);
                for (i, to) in bytes.chunks_exact_mut(4).take(frames * CHANNELS).enumerate() {
                    let sample = ring.peek(i);
                    peak = peak.max(sample.abs());
                    to.copy_from_slice(&sample.to_le_bytes());
                }
                ring.skip(frames * CHANNELS);
                m.peak_out.store(bits(peak), Ordering::Relaxed);
                frames
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
            if frames == 0 {
                m.dry.fetch_add(1, Ordering::Relaxed);
            }
            m.played.fetch_add(frames as u64, Ordering::Relaxed);
            m.fill.store((ring.len() / CHANNELS) as u64, Ordering::Relaxed);
            // how long what was just handed over takes to reach the device, as PipeWire says it
            if let Ok(time) = stream.time() {
                let rate = time.rate();
                if rate.denom > 0 {
                    let micros = (time.delay().max(0) as u64 * 1_000_000 * rate.num as u64) / rate.denom as u64;
                    m.wait_sum.fetch_add(micros, Ordering::Relaxed);
                    m.wait_count.fetch_add(1, Ordering::Relaxed);
                    m.wait_longest.fetch_max(micros, Ordering::Relaxed);
                }
            }
        })
        .register()
        .map_err(|e| e.to_string())?;

    let bytes = format();
    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
    let pod = || spa::pod::Pod::from_bytes(&bytes).expect("the format is read");
    capture.connect(spa::utils::Direction::Input, None, flags, &mut [pod()]).map_err(|e| format!("making Bottle EQ's own device: {e}"))?;
    playback.connect(spa::utils::Direction::Output, None, flags | pw::stream::StreamFlags::TRIGGER, &mut [pod()]).map_err(|e| format!("playing to {}: {e}", target.name))?;
    let stop = stop.clone();
    until_stopped(&mainloop, move || stop.load(Ordering::Relaxed));
    Ok(())
}

/// Make Bottle EQ's own device, and play what is played to it, through `process`, to the target.
/// `source` is that own device (the first of `outputs`). `listen` is called on every block of
/// interleaved frames as it arrives, `process` on the same block just before it is handed to the target;
/// both in PipeWire's audio thread. There is no cushion and no exclusive mode here: see the top.
pub fn run(
    source: &Output,
    target: &Output,
    _cushion_ms: f32,
    _want_exclusive: bool,
    listen: impl FnMut(&[f32], usize) + Send + 'static,
    process: impl FnMut(&mut [f32], usize) + Send + 'static,
) -> Result<Running, String> {
    if source.id != OWN {
        return Err(format!("on Linux the sound is captured from Bottle EQ's own device, not from \"{}\": programs play to \"Bottle EQ\"", source.name));
    }
    if target.id == OWN {
        return Err("the source and the target are the same device: Bottle EQ's own device is where the sound comes from, choose the speakers to play it to".to_string());
    }
    let (meters, stop) = (Arc::new(Meters::default()), Arc::new(AtomicBool::new(false)));
    let (said, heard) = channel::<Result<Opened, String>>();
    let (tgt, m, s) = (target.clone(), meters.clone(), stop.clone());
    let thread = std::thread::spawn(move || {
        if let Err(e) = path(&tgt, listen, process, &m, &s, &said) {
            let _ = said.send(Err(e));
        }
    });
    match heard.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(opened)) => Ok(Running { stop, threads: vec![thread], meters, opened, in_rate: RATE, out_rate: RATE }),
        Ok(Err(why)) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(why)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err(format!("the path to \"{}\" did not open (is that device there?)", target.name))
        }
    }
}

/// Play sound to an output like any program would: a fifth of a second of silence, then `sound`
/// (interleaved, two channels, at the output's rate), three tenths of a second of silence, and done.
pub fn play(output: &Output, sound: Vec<f32>) -> JoinHandle<()> {
    let output = output.clone();
    std::thread::spawn(move || {
        let play = || -> Result<(), String> {
            let (mainloop, core) = connect()?;
            let stream = pw::stream::StreamBox::new(
                &core,
                "bottle-eq-test",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Playback",
                    *pw::keys::MEDIA_ROLE => "Music",
                    *pw::keys::NODE_NAME => "bottle_eq_test",
                    *pw::keys::TARGET_OBJECT => output.id.clone(),
                    *pw::keys::NODE_LATENCY => format!("{QUANTUM}/{RATE}"),
                },
            )
            .map_err(|e| e.to_string())?;
            // frames of silence still to come before the sound, the place in the sound, and frames of
            // silence played after it
            let (mut before, mut at, after) = (RATE as usize / 5, 0usize, Rc::new(Cell::new(0usize)));
            let a = after.clone();
            let _listener = stream
                .add_local_listener_with_user_data(())
                .process(move |stream, _| {
                    let Some(mut buffer) = stream.dequeue_buffer() else { return };
                    let asked = buffer.requested() as usize;
                    let Some(data) = buffer.datas_mut().first_mut() else { return };
                    let stride = 4 * CHANNELS;
                    let frames = {
                        let Some(bytes) = data.data() else { return };
                        let frames = if asked > 0 { asked.min(bytes.len() / stride) } else { bytes.len() / stride };
                        for frame in bytes.chunks_exact_mut(stride).take(frames) {
                            let silent = before > 0 || at + CHANNELS > sound.len();
                            for (c, to) in frame.chunks_exact_mut(4).enumerate() {
                                to.copy_from_slice(&(if silent { 0.0 } else { sound[at + c] }).to_le_bytes());
                            }
                            match (before > 0, silent) {
                                (true, _) => before -= 1,
                                (false, true) => a.set(a.get() + 1),
                                (false, false) => at += CHANNELS,
                            }
                        }
                        frames
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = stride as i32;
                    *chunk.size_mut() = (frames * stride) as u32;
                })
                .register()
                .map_err(|e| e.to_string())?;
            let bytes = format();
            let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
            stream.connect(spa::utils::Direction::Output, None, flags, &mut [spa::pod::Pod::from_bytes(&bytes).expect("the format is read")]).map_err(|e| e.to_string())?;
            until_stopped(&mainloop, move || after.get() > 3 * RATE as usize / 10);
            Ok(())
        };
        if let Err(e) = play() {
            eprintln!("playing to {}: {e}", output.name);
        }
    })
}

/// Play a tone to an output like any program would: a fifth of a second of silence, the tone for
/// `seconds`, and done.
pub fn tone(output: &Output, hz: f64, level: f32, seconds: f64) -> JoinHandle<()> {
    let frames = (seconds * RATE as f64) as usize;
    let sound = (0..frames).flat_map(|i| [(level as f64 * (std::f64::consts::TAU * hz * i as f64 / RATE as f64).sin()) as f32; CHANNELS]).collect();
    play(output, sound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_device_is_read_from_its_value() {
        assert_eq!(name_in(r#"{"name":"alsa_output.usb-speakers.analog-stereo"}"#).as_deref(), Some("alsa_output.usb-speakers.analog-stereo"));
        assert_eq!(name_in(r#"{ "name": "bottle_eq" }"#).as_deref(), Some("bottle_eq"));
        assert_eq!(name_in("{}"), None);
    }

    #[test]
    fn a_block_is_read_as_it_lies_in_its_bytes() {
        let bytes: Vec<u8> = [0.5f32, -0.25, 1.0, 0.0].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut sound = Vec::new();
        samples(&bytes, &mut sound);
        assert_eq!(sound, [0.5, -0.25, 1.0, 0.0]);
    }
}
