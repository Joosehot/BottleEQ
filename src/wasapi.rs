//! System audio in, system audio out, straight on WASAPI, for the shortest delay Windows allows.
//!
//! # WASAPI loopback, plainly
//!
//! Windows mixes every program's sound for one output device (an "endpoint": the speakers, the monitor,
//! S/PDIF) into one stream in that device's own "mix format" (32-bit float). Its audio engine does that
//! in blocks: one "period" of sound at a time, 10 ms unless someone asks for less.
//!
//! Loopback is a capture stream opened on an *output* endpoint: WASAPI hands over a copy of the finished
//! mix on its way to that device, one engine period at a time. Five things follow:
//!
//! 1. **It is a copy.** The device still plays the original, so Bottle EQ captures from one endpoint
//!    (the *source*: what Windows plays to, which you do not hear) and plays the processed sound to
//!    another (the *target*: what you listen to).
//! 2. **Silence is no data.** When nothing plays to the source, loopback delivers nothing. Playback then
//!    plays silence and waits for a cushion of sound before it starts again.
//! 3. **Two devices, two clocks.** Their "48000 Hz" differ by a few parts in a hundred thousand, so the
//!    ring between them is read a hair fast or slow to hold its fill at the cushion (`Reader`).
//! 4. **The delay is made of blocks.** A sound waits for the source's engine period to end before it is
//!    handed over, then in the ring, then for the target's buffer. At 10 ms periods that cannot go under
//!    about 30 ms. So:
//!    - the source's engine is asked to run at its *least* period (IAudioClient3; 2.67 ms on HD Audio):
//!      a silent stream opened at that period is enough, and loopback then arrives in those blocks;
//!    - the target is opened in *exclusive mode* at its least period (about 3 ms): Bottle EQ then owns
//!      the device, no engine mixes in between, and nothing else can play to it while it runs. In
//!      exclusive mode the device takes integer samples, so the float sound is converted (with dither
//!      at 16 bits).
//! 5. **Exclusive mode cannot be listened to by loopback** (there is no engine mix to copy), so the
//!    delay is measured on the clock: from a block arriving from the source's engine to its frames being
//!    handed to the target, plus the target's block (a buffer handed over begins to play when the one
//!    before it ends). That is the delay Bottle EQ adds; the device's own converter comes on top, as it
//!    does for any program.
//!
//! The path: capture thread -> lock-free ring -> playback thread. Both threads run at the "Pro Audio"
//! priority Windows keeps for this, wake on the device's own event, and never lock or allocate.

use crate::ring::Ring;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows::core::{w, Result as Win, GUID, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::System::Com::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, CreateEventW, WaitForSingleObject, AVRT_PRIORITY_CRITICAL};

/// What the two audio threads tell the rest of the program.
#[derive(Default)]
pub struct Meters {
    pub captured: AtomicU64,
    pub played: AtomicU64,
    /// frames let go because the ring was too full (playback behind)
    pub dropped: AtomicU64,
    /// times the ring ran dry while playing (a gap in the sound, or simply nothing playing any more)
    pub dry: AtomicU64,
    /// frames in the ring, as the playback thread last saw it
    pub fill: AtomicU64,
    /// how much faster (+) or slower (-) than nominal the ring is being read, in parts per million
    pub drift_ppm: AtomicU32,
    pub peak_in: AtomicU32,
    pub peak_out: AtomicU32,
    /// the mean and the longest time, in microseconds, between a frame arriving from the source's engine
    /// and being handed to the target (since the last look)
    pub wait_sum: AtomicU64,
    pub wait_count: AtomicU64,
    pub wait_longest: AtomicU64,
    /// times the playback thread was woken more than a block late: the device had nothing to play
    pub late: AtomicU64,
    /// times Windows said a captured block does not follow the one before (sound was lost on the way in)
    pub breaks: AtomicU64,
    /// the loudest jump from one frame to the next in what was captured (first channel), and how many
    /// frames had been captured when it came
    pub jump_in: AtomicU32,
    pub jump_at: AtomicU64,
    /// audio threads that Windows gave its "Pro Audio" priority (2 when both have it)
    pub priority: AtomicU64,
    /// when (in 100 ns of the performance counter) the latest captured block arrived, and how many
    /// frames had been captured before it
    stamp_time: AtomicU64,
    stamp_frames: AtomicU64,
}

fn bits(x: f32) -> u32 {
    x.to_bits()
}

pub fn value(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

/// The performance counter now, in 100 ns.
fn now() -> u64 {
    let (mut count, mut per_second) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut per_second);
    }
    (count as u128 * 10_000_000 / per_second.max(1) as u128) as u64
}

#[derive(Clone)]
pub struct Output {
    pub id: String,
    pub name: String,
    pub channels: usize,
    pub rate: u32,
    pub default: bool,
    /// engine periods in frames: (usual, least)
    pub periods: (u32, u32),
}

unsafe fn enumerator() -> Win<IMMDeviceEnumerator> {
    // per thread; a second call on the same thread is harmless
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
}

/// Every active output endpoint, with its mix format and engine periods.
pub fn outputs() -> Result<Vec<Output>, String> {
    let list = || -> Win<Vec<Output>> {
        unsafe {
            let all = enumerator()?;
            let default = all.GetDefaultAudioEndpoint(eRender, eConsole).and_then(|d| d.GetId()).ok().map(|id| id.to_string().unwrap_or_default());
            let found = all.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut out = Vec::new();
            for i in 0..found.GetCount()? {
                let device = found.Item(i)?;
                let id = device.GetId()?.to_string().unwrap_or_default();
                let name = device.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?.to_string();
                let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
                let format = client.GetMixFormat()?;
                let (mut usual, mut step, mut least, mut most) = (0u32, 0u32, 0u32, 0u32);
                if client.GetSharedModeEnginePeriod(format, &mut usual, &mut step, &mut least, &mut most).is_err() {
                    (usual, least) = (0, 0);
                }
                out.push(Output { default: default.as_deref() == Some(id.as_str()), id, name, channels: (*format).nChannels as usize, rate: (*format).nSamplesPerSec, periods: (usual, least) });
            }
            Ok(out)
        }
    };
    list().map_err(|e| format!("listing the output devices: {e}"))
}

/// An output by its number in the list or by a part of its name.
pub fn find(wanted: &str) -> Result<Output, String> {
    let all = outputs()?;
    let by_number = wanted.parse::<usize>().ok().filter(|n| (1..=all.len()).contains(n)).map(|n| n - 1);
    let by_name = || all.iter().position(|o| o.name.to_lowercase().contains(&wanted.to_lowercase()));
    match by_number.or_else(by_name) {
        Some(i) => Ok(all[i].clone()),
        None => Err(format!("no output device matches \"{wanted}\" (there are: {})", all.iter().map(|o| o.name.as_str()).collect::<Vec<_>>().join("; "))),
    }
}

/// 4-point, 3rd-order Hermite: the value `t` of the way from `b` to `c`.
fn hermite(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let c1 = 0.5 * (c - a);
    let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
    let c3 = 0.5 * (d - a) + 1.5 * (b - c);
    ((c3 * t + c2) * t + c1) * t + b
}

/// The playback side of the ring: reads it at the target's rate, holding its fill at the cushion.
struct Reader {
    ring: Arc<Ring>,
    meters: Arc<Meters>,
    in_ch: usize,
    in_rate: u32,
    out_ch: usize,
    nominal: f64,
    /// frames the ring should hold, on average, when a block is about to be read
    cushion: usize,
    /// frames over the cushion at which the oldest sound is let go
    slack: usize,
    /// frames in a block from the source
    in_block: u32,
    phase: f64,
    primed: bool,
    usual: f64,
    lean: f64,
}

impl Reader {
    /// Fill one block of interleaved output frames.
    fn read(&mut self, out: &mut [f32]) {
        let (r, m, in_ch) = (&self.ring, &self.meters, self.in_ch);
        let fill = r.len() / in_ch;
        m.fill.store(fill as u64, Ordering::Relaxed);
        // Start from the cushion exactly: what gathered in the ring while the target was being opened
        // would otherwise stay there as delay. And when the ring is far too full (playback was held up),
        // let go of the oldest sound at once.
        let starting = !self.primed && fill >= self.cushion;
        if starting || fill > self.cushion + self.slack {
            r.skip((fill - self.cushion) * in_ch);
            if !starting {
                m.dropped.fetch_add((fill - self.cushion) as u64, Ordering::Relaxed);
            }
            (self.primed, self.phase, self.usual) = (true, 0.0, self.cushion as f64);
        }
        let fill = r.len() / in_ch;
        // The fill swings by a whole block from one read to the next, so it is its slow average that is
        // held at the cushion: an error is taken out over about ten seconds, by reading at most 0.1% fast
        // or slow. That is under 2 cents of pitch.
        self.usual += 0.002 * (fill as f64 - self.usual);
        // (and what stays wrong for long, because the two clocks really differ, is learned as `lean`, so
        // that the fill comes back to the cushion itself and not to a little beside it)
        let off = (self.usual - self.cushion as f64) / self.in_rate as f64; // seconds of sound
        self.lean = (self.lean + off * 0.000_02).clamp(-0.0005, 0.0005);
        let speed = (off / 10.0 + self.lean).clamp(-0.001, 0.001);
        m.drift_ppm.store(bits((speed * 1e6) as f32), Ordering::Relaxed);
        let step = self.nominal * (1.0 + speed);

        // How long has the frame about to be played waited since the source's engine handed it over? The
        // latest block came at `stamp_time`; a frame some blocks before it came that many blocks earlier.
        if self.primed {
            let behind = m.stamp_frames.load(Ordering::Relaxed) as i64 - (r.read_total() / in_ch) as i64;
            let blocks_before = if behind > 0 { (behind as u64).div_ceil(self.in_block.max(1) as u64) } else { 0 };
            let came = m.stamp_time.load(Ordering::Relaxed) as f64 - blocks_before as f64 * self.in_block as f64 * 1e7 / self.in_rate as f64;
            let waited = ((now() as f64 - came) / 10.0).max(0.0) as u64; // microseconds
            if waited < 1_000_000 {
                m.wait_sum.fetch_add(waited, Ordering::Relaxed);
                m.wait_count.fetch_add(1, Ordering::Relaxed);
                m.wait_longest.fetch_max(waited, Ordering::Relaxed);
            }
        }

        let mut frames = 0;
        for frame in out.chunks_mut(self.out_ch) {
            if self.primed && r.len() / in_ch < 4 {
                self.primed = false;
                m.dry.fetch_add(1, Ordering::Relaxed);
            }
            if !self.primed {
                frame.fill(0.0);
                continue;
            }
            for (c, s) in frame.iter_mut().enumerate() {
                // front left and right are kept; a mono source goes to both; other channels stay silent
                let from = if c < in_ch.min(2) || (c < in_ch && in_ch == self.out_ch) { Some(c) } else if in_ch == 1 && c < 2 { Some(0) } else { None };
                *s = from.map_or(0.0, |c| hermite(r.peek(c), r.peek(in_ch + c), r.peek(2 * in_ch + c), r.peek(3 * in_ch + c), self.phase as f32));
            }
            self.phase += step;
            let whole = self.phase as usize;
            r.skip(whole.min(r.len() / in_ch) * in_ch);
            self.phase -= whole as f64;
            frames += 1;
        }
        m.played.fetch_add(frames, Ordering::Relaxed);
    }
}

/// How the path was opened.
#[derive(Clone, Default)]
pub struct Opened {
    /// frames per block arriving from the source
    pub in_block: u32,
    /// frames per block going to the target
    pub out_block: u32,
    pub exclusive: bool,
    /// bits per sample the target takes (32 float in shared mode)
    pub out_bits: u16,
    /// why exclusive mode was not used, when it was asked for
    pub note: String,
    /// each captured block is played as it arrives, from the capture thread itself (no ring between two
    /// threads)
    pub direct: bool,
}

/// The running path: it lives as long as this is kept.
pub struct Running {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub meters: Arc<Meters>,
    pub opened: Opened,
    pub in_rate: u32,
    pub out_rate: u32,
}

impl Running {
    /// The delay Bottle EQ adds, in milliseconds, (mean, longest), since the last look: from a frame
    /// arriving from the source's engine to its beginning to play at the target.
    ///
    /// Through the ring: the wait until the frame is handed over, plus one block of the target's (a
    /// buffer handed over at an event begins to play when the one before it ends).
    /// Direct: the sound queued ahead of the frame in the target's buffer, plus the wait for the device's
    /// next period to begin, which is anything from nothing to one block: half a block in the mean.
    pub fn delay(&self) -> (f32, f32) {
        let m = &self.meters;
        let (sum, n, longest) = (m.wait_sum.swap(0, Ordering::Relaxed), m.wait_count.swap(0, Ordering::Relaxed), m.wait_longest.swap(0, Ordering::Relaxed));
        let (mean, longest) = (sum as f32 / n.max(1) as f32 / 1000.0, longest as f32 / 1000.0);
        let block = self.opened.out_block as f32 * 1000.0 / self.out_rate as f32;
        if self.opened.direct { (mean + block / 2.0, longest + block) } else { (mean + block, longest + block) }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

unsafe fn device(id: &str) -> Win<IMMDevice> {
    enumerator()?.GetDevice(&HSTRING::from(id))
}

/// Put this thread in Windows' "Pro Audio" class at its highest priority. Says whether Windows agreed.
unsafe fn pro_audio() -> bool {
    let mut task = 0u32;
    match AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) {
        Ok(handle) => AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL).is_ok(),
        Err(_) => false,
    }
}

/// The capture thread: the source's engine at its least period, and its mix by loopback into the ring.
unsafe fn capture(source: &Output, ring: &Ring, m: &Meters, stop: &AtomicBool, ready: &Sender<Result<u32, String>>) -> Win<()> {
    let dev = device(&source.id)?;
    // a silent stream at the least period: while it runs, the engine mixes (and loopback arrives) in
    // blocks that short
    let holder: IAudioClient3 = dev.Activate(CLSCTX_ALL, None)?;
    let format = holder.GetMixFormat()?;
    let mut block = source.periods.0;
    if source.periods.1 > 0 && source.periods.1 < source.periods.0 && holder.InitializeSharedAudioStream(0, source.periods.1, format, None).is_ok() {
        holder.Start()?;
        block = source.periods.1;
    }
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
    // room for 200 ms: if this thread is ever held up, the blocks wait here instead of being lost
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 2_000_000, 0, format, None)?;
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let packets: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    let _ = ready.send(Ok(block));

    let ch = source.channels;
    let silence = vec![0.0f32; 4096 * ch];
    let mut frames_so_far = 0u64;
    let mut last = 0.0f32;
    while !stop.load(Ordering::Relaxed) {
        if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
            continue;
        }
        while packets.GetNextPacketSize()? > 0 {
            let (mut data, mut n, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            packets.GetBuffer(&mut data, &mut n, &mut flags, None, None)?;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && frames_so_far > 0 {
                m.breaks.fetch_add(1, Ordering::Relaxed);
            }
            let samples: &[f32] = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 { &silence[..(n as usize * ch).min(silence.len())] } else { std::slice::from_raw_parts(data as *const f32, n as usize * ch) };
            // (the time stamp WASAPI gives with a loopback block is not the moment it was mixed: measured
            // against it, sound left the ring before it had come. So the block is stamped as it arrives.)
            m.stamp_frames.store(frames_so_far, Ordering::Relaxed);
            m.stamp_time.store(now(), Ordering::Relaxed);
            let fitted = ring.push(samples) / ch;
            frames_so_far += fitted as u64;
            m.captured.fetch_add(fitted as u64, Ordering::Relaxed);
            m.dropped.fetch_add((n as usize - fitted) as u64, Ordering::Relaxed);
            let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            m.peak_in.store(bits(value(&m.peak_in).max(peak)), Ordering::Relaxed);
            let mut jump = value(&m.jump_in);
            for frame in samples.chunks(ch) {
                jump = jump.max((frame[0] - last).abs());
                last = frame[0];
            }
            m.jump_in.store(bits(jump), Ordering::Relaxed);
            packets.ReleaseBuffer(n)?;
        }
    }
    client.Stop()?;
    let _ = holder.Stop();
    Ok(())
}

/// A PCM format for exclusive mode: `bits` in the container, `valid` of them used.
fn pcm(rate: u32, channels: usize, bits: u16, valid: u16) -> WAVEFORMATEXTENSIBLE {
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels as u16,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * channels as u32 * bits as u32 / 8,
            nBlockAlign: channels as u16 * bits / 8,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
        dwChannelMask: if channels == 1 { 4 } else { 3 },
        SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
    }
}

/// The target in exclusive mode at its least period: the client, (container bits, valid bits) and the
/// period's frames. `pushed`: no event; the buffer is three periods long and is written into whenever
/// there is sound (else one buffer of one period is filled at each event).
unsafe fn exclusive(dev: &IMMDevice, target: &Output, pushed: bool) -> Result<(IAudioClient, (u16, u16), u32), String> {
    let probe: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
    let (mut shared, mut least) = (0i64, 0i64);
    probe.GetDevicePeriod(Some(&mut shared), Some(&mut least)).map_err(|e| e.to_string())?;
    let mut why = String::from("no integer format at its own rate is taken");
    for (bits, valid) in [(32u16, 32u16), (32, 24), (24, 24), (16, 16)] {
        let wave = pcm(target.rate, target.channels.min(2), bits, valid);
        let format = &wave as *const _ as *const WAVEFORMATEX;
        let mut client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        if client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, format, None).is_err() {
            continue;
        }
        let mut opened = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, least, least, format, None::<*const GUID>);
        if opened.as_ref().is_err_and(|e| e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED) {
            // the device wants a whole number of its own blocks: ask how many frames that is, and open again
            let aligned = client.GetBufferSize().map_err(|e| e.to_string())?;
            let period = (10_000_000.0 * aligned as f64 / target.rate as f64 + 0.5) as i64;
            client = dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
            opened = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, period, format, None::<*const GUID>);
        }
        if opened.is_ok() && pushed {
            // now that the period's true length is known: the same device again, three periods long, no event
            let frames = client.GetBufferSize().map_err(|e| e.to_string())?;
            let period = (10_000_000.0 * frames as f64 / target.rate as f64 + 0.5) as i64;
            drop(client);
            client = dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
            opened = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, 3 * period, period, format, None::<*const GUID>);
            if opened.is_ok() {
                return Ok((client, (bits, valid), frames));
            }
        }
        match opened {
            Ok(()) => {
                let frames = client.GetBufferSize().map_err(|e| e.to_string())?;
                return Ok((client, (bits, valid), frames));
            }
            Err(e) if e.code() == AUDCLNT_E_DEVICE_IN_USE => return Err("another program holds the device".to_string()),
            Err(e) if e.code() == AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => return Err("exclusive mode is switched off for this device in the Windows sound settings".to_string()),
            Err(e) => why = e.to_string(),
        }
    }
    Err(why)
}

/// Float sound into the device's buffer, in the device's samples: `bits` is (container, valid), (0, _)
/// for float as it is.
unsafe fn hand_over(sound: &[f32], data: *mut u8, bits: (u16, u16), noise: &mut u32) {
    match bits {
        (0, _) => std::slice::from_raw_parts_mut(data as *mut f32, sound.len()).copy_from_slice(sound),
        (16, _) => {
            let out = std::slice::from_raw_parts_mut(data as *mut i16, sound.len());
            for (o, s) in out.iter_mut().zip(sound.iter()) {
                // 16 bits are few: a little triangular noise keeps the rounding from following the sound
                let mut next = || {
                    *noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (*noise >> 8) as f32 / 16_777_216.0
                };
                let dither = next() - next();
                *o = (s * 32767.0 + dither).round().clamp(-32768.0, 32767.0) as i16;
            }
        }
        (24, _) => {
            let out = std::slice::from_raw_parts_mut(data, sound.len() * 3);
            for (o, s) in out.chunks_mut(3).zip(sound.iter()) {
                let v = (s.clamp(-1.0, 1.0) as f64 * 8_388_607.0).round() as i32;
                o.copy_from_slice(&v.to_le_bytes()[..3]);
            }
        }
        (_, valid) => {
            let out = std::slice::from_raw_parts_mut(data as *mut i32, sound.len());
            let (scale, shift) = if valid == 24 { (8_388_607.0, 8) } else { (2_147_483_647.0, 0) };
            for (o, s) in out.iter_mut().zip(sound.iter()) {
                *o = ((s.clamp(-1.0, 1.0) as f64 * scale).round() as i32) << shift;
            }
        }
    }
}

/// The playback thread: the ring, read at the target's rate, processed, and handed to the target.
#[allow(clippy::too_many_arguments)]
unsafe fn playback(
    target: &Output,
    mut reader: Reader,
    want_exclusive: bool,
    in_block: u32,
    cushion_ms: f32,
    process: &mut dyn FnMut(&mut [f32], usize),
    m: &Meters,
    stop: &AtomicBool,
    ready: &Sender<Result<Opened, String>>,
) -> Win<()> {
    let dev = device(&target.id)?;
    let mut opened = Opened::default();
    let taken = if want_exclusive { exclusive(&dev, target, false) } else { Err(String::new()) };
    let (client, format_bits, block) = match taken {
        Ok((client, bits, frames)) => {
            opened.exclusive = true;
            (client, bits, frames)
        }
        Err(why) => {
            opened.note = why;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, format, None)?;
            let frames = client.GetBufferSize()?;
            (client, (0, 0), frames)
        }
    };
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let render: IAudioRenderClient = client.GetService()?;
    // exclusive mode is opened with at most two channels
    if opened.exclusive {
        reader.out_ch = target.channels.min(2);
    }
    let out_ch = reader.out_ch;
    (opened.out_block, opened.out_bits) = (block, if opened.exclusive { format_bits.1 } else { 32 });
    // What the ring must hold, on average, when a block is about to be read: that whole block, half a
    // block of the source's (its blocks come at any moment between two reads), and the cushion asked for.
    let leaving = if opened.exclusive { block as f64 } else { block as f64 / 2.0 } * reader.nominal;
    reader.cushion = (leaving + in_block as f64 / 2.0 + cushion_ms as f64 / 1000.0 * reader.in_rate as f64) as usize + 4;
    reader.slack = 2 * (leaving as usize + in_block as usize);
    // exclusive mode takes one whole buffer per event; the first is silence
    if opened.exclusive {
        render.GetBuffer(block)?;
        render.ReleaseBuffer(block, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
    }
    client.Start()?;
    let _ = ready.send(Ok(opened.clone()));

    let mut sound = vec![0.0f32; block as usize * out_ch];
    let mut noise = 0x2545_F491u32; // for the dither
    let block_time = block as u64 * 10_000_000 / target.rate as u64;
    let (mut woken, mut wakes) = (0u64, 0u64);
    while !stop.load(Ordering::Relaxed) {
        if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
            continue;
        }
        let at = now();
        // (the first wakes, while the device is starting, are not counted)
        wakes += 1;
        if opened.exclusive && wakes > 100 && at - woken > 2 * block_time {
            m.late.fetch_add(1, Ordering::Relaxed);
        }
        woken = at;
        let frames = if opened.exclusive { block } else { block - client.GetCurrentPadding()? };
        if frames == 0 {
            continue;
        }
        let sound = &mut sound[..frames as usize * out_ch];
        reader.read(sound);
        process(sound, out_ch);
        let peak = sound.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        m.peak_out.store(bits(value(&m.peak_out).max(peak)), Ordering::Relaxed);
        let data = render.GetBuffer(frames)?;
        hand_over(sound, data, format_bits, &mut noise);
        render.ReleaseBuffer(frames, 0)?;
    }
    client.Stop()?;
    Ok(())
}

/// The direct path, one thread: each block is played as it arrives from the source.
///
/// With two threads the ring had to hold a whole block of the target's before every read, because the
/// two threads woke on two unrelated clocks. Here the target's buffer is three of its periods long and
/// is not filled at its events: whenever a block comes from the source (every 2.67 ms) it is read at the
/// target's rate, processed and written straight in behind what is already queued. What is queued is
/// held, on average, at a little over what the device takes in one period.
unsafe fn direct(source: &Output, target: &Output, cushion_ms: f32, process: &mut dyn FnMut(&mut [f32], usize), m: &Meters, stop: &AtomicBool, ready: &Sender<Result<Opened, String>>) -> Win<()> {
    // the target first: if exclusive mode is not to be had, say why and leave
    let out_dev = device(&target.id)?;
    let (out, format_bits, period) = match exclusive(&out_dev, target, true) {
        Ok(taken) => taken,
        Err(why) => {
            let _ = ready.send(Err(why));
            return Ok(());
        }
    };
    let room = out.GetBufferSize()?;
    let render: IAudioRenderClient = out.GetService()?;
    let out_ch = target.channels.min(2);

    // the source: its engine at its least period, and its mix by loopback
    let dev = device(&source.id)?;
    let holder: IAudioClient3 = dev.Activate(CLSCTX_ALL, None)?;
    let format = holder.GetMixFormat()?;
    let mut in_block = source.periods.0;
    if source.periods.1 > 0 && source.periods.1 < source.periods.0 && holder.InitializeSharedAudioStream(0, source.periods.1, format, None).is_ok() {
        holder.Start()?;
        in_block = source.periods.1;
    }
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 2_000_000, 0, format, None)?;
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let packets: IAudioCaptureClient = client.GetService()?;

    let (in_ch, in_rate) = (source.channels, source.rate);
    let nominal = in_rate as f64 / target.rate as f64;
    // What should be queued, on average, when a block is about to be written: the device takes one
    // period at a time at moments unrelated to ours, so before our write the queue has swung down by up
    // to one period; half a period is its mean, and the cushion keeps the lowest point off the floor.
    let cushion = (period as f64 / 2.0 + in_block as f64 / nominal / 4.0 + cushion_ms as f64 / 1000.0 * target.rate as f64) as u32;
    if cushion + 2 * in_block > room {
        let _ = ready.send(Err("the device's buffer is too short for the direct path".to_string()));
        return Ok(());
    }
    // the first sound queued is silence, up to the cushion
    render.GetBuffer(cushion)?;
    render.ReleaseBuffer(cushion, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
    out.Start()?;
    client.Start()?;
    let _ = ready.send(Ok(Opened { in_block, out_block: period, exclusive: true, out_bits: format_bits.1, note: String::new(), direct: true }));

    // a short store of the source's frames, to read between them at the target's rate
    let store = Ring::new(in_rate as usize / 4 * in_ch);
    let silence = vec![0.0f32; 4096 * in_ch];
    let mut sound = vec![0.0f32; room as usize * out_ch];
    let (mut phase, mut usual, mut lean, mut last, mut noise) = (0.0f64, cushion as f64, 0.0f64, 0.0f32, 0x2545_F491u32);
    while !stop.load(Ordering::Relaxed) {
        if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
            continue;
        }
        while packets.GetNextPacketSize()? > 0 {
            let (mut data, mut n, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            packets.GetBuffer(&mut data, &mut n, &mut flags, None, None)?;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && m.captured.load(Ordering::Relaxed) > 0 {
                m.breaks.fetch_add(1, Ordering::Relaxed);
            }
            let samples: &[f32] = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 { &silence[..(n as usize * in_ch).min(silence.len())] } else { std::slice::from_raw_parts(data as *const f32, n as usize * in_ch) };
            let fitted = store.push(samples) / in_ch;
            m.captured.fetch_add(fitted as u64, Ordering::Relaxed);
            let (mut peak, mut jump) = (value(&m.peak_in), value(&m.jump_in));
            for (i, frame) in samples.chunks(in_ch).enumerate() {
                peak = frame.iter().fold(peak, |p, s| p.max(s.abs()));
                if (frame[0] - last).abs() > jump {
                    jump = (frame[0] - last).abs();
                    m.jump_at.store(m.captured.load(Ordering::Relaxed) - fitted as u64 + i as u64, Ordering::Relaxed);
                }
                last = frame[0];
            }
            m.peak_in.store(bits(peak), Ordering::Relaxed);
            m.jump_in.store(bits(jump), Ordering::Relaxed);
            packets.ReleaseBuffer(n)?;

            // how much is queued at the target right now
            let mut queued = out.GetCurrentPadding()?;
            if queued == 0 {
                // the device ran out: there is a gap in the sound. Begin again from the cushion.
                m.dry.fetch_add(1, Ordering::Relaxed);
                render.GetBuffer(cushion)?;
                render.ReleaseBuffer(cushion, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                (queued, usual) = (cushion, cushion as f64);
            }
            m.fill.store(queued as u64, Ordering::Relaxed);
            // the queue's slow average is held at the cushion by reading the source a hair fast or slow, and
            // the lasting difference of the two clocks is learned as `lean`
            usual += 0.002 * (queued as f64 - usual);
            let off = (usual - cushion as f64) / target.rate as f64;
            lean = (lean + off * 0.000_02).clamp(-0.0005, 0.0005);
            let speed = (off / 10.0 + lean).clamp(-0.001, 0.001);
            m.drift_ppm.store(bits((speed * 1e6) as f32), Ordering::Relaxed);
            let step = nominal * (1.0 + speed);

            // read everything that has come, at the target's rate
            let mut frames = 0usize;
            while store.len() / in_ch >= 4 && frames < (room - queued) as usize {
                let frame = &mut sound[frames * out_ch..(frames + 1) * out_ch];
                for (c, s) in frame.iter_mut().enumerate() {
                    let from = if c < in_ch { c } else { 0 };
                    *s = hermite(store.peek(from), store.peek(in_ch + from), store.peek(2 * in_ch + from), store.peek(3 * in_ch + from), phase as f32);
                }
                phase += step;
                let whole = phase as usize;
                store.skip(whole.min(store.len() / in_ch) * in_ch);
                phase -= whole as f64;
                frames += 1;
            }
            // what did not fit (the queue is full: we are far ahead of the device) is let go
            let left = store.len() / in_ch;
            if left > 8 {
                store.skip((left - 3) * in_ch);
                m.dropped.fetch_add((left - 3) as u64, Ordering::Relaxed);
            }
            if frames == 0 {
                continue;
            }
            let block = &mut sound[..frames * out_ch];
            process(block, out_ch);
            let peak = block.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            m.peak_out.store(bits(value(&m.peak_out).max(peak)), Ordering::Relaxed);
            let data = render.GetBuffer(frames as u32)?;
            hand_over(block, data, format_bits, &mut noise);
            render.ReleaseBuffer(frames as u32, 0)?;
            m.played.fetch_add(frames as u64, Ordering::Relaxed);
            // the first of these frames plays when what was queued ahead of it has been played
            let waits = queued as u64 * 1_000_000 / target.rate as u64;
            m.wait_sum.fetch_add(waits, Ordering::Relaxed);
            m.wait_count.fetch_add(1, Ordering::Relaxed);
            m.wait_longest.fetch_max(waits, Ordering::Relaxed);
        }
    }
    client.Stop()?;
    let _ = holder.Stop();
    out.Stop()?;
    Ok(())
}

/// Capture the source's mix by loopback and play it to the target. `cushion_ms` is the sound kept in
/// the ring on top of what the two devices' blocks need. `process` is called in the playback thread on
/// every block of interleaved output frames just before it is played.
pub fn run(source: &Output, target: &Output, cushion_ms: f32, want_exclusive: bool, process: impl FnMut(&mut [f32], usize) + Send + 'static) -> Result<Running, String> {
    if source.id == target.id {
        return Err("the source and the target are the same device: loopback is a copy of what that device plays, so playing it back there would double it and feed it round again".to_string());
    }
    let meters = Arc::new(Meters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    // `process` goes to whichever thread plays
    let process = Arc::new(std::sync::Mutex::new(process));
    let mut why_not = String::new();

    if want_exclusive {
        let (said, heard) = channel::<Result<Opened, String>>();
        let (src, tgt, m, s, pr) = (source.clone(), target.clone(), meters.clone(), stop.clone(), process.clone());
        let thread = std::thread::spawn(move || unsafe {
            m.priority.fetch_add(2 * pro_audio() as u64, Ordering::Relaxed);
            // this thread alone uses `process` from here on: the lock is taken once and kept
            let mut process = pr.lock().unwrap();
            if let Err(e) = direct(&src, &tgt, cushion_ms, &mut *process, &m, &s, &said) {
                let _ = said.send(Err(e.to_string()));
                eprintln!("the audio thread stopped: {e}");
            }
        });
        match heard.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(opened)) => {
                threads.push(thread);
                return Ok(Running { stop, threads, meters, opened, in_rate: source.rate, out_rate: target.rate });
            }
            Ok(Err(why)) => {
                let _ = thread.join();
                meters.priority.store(0, Ordering::Relaxed);
                why_not = why;
            }
            Err(_) => return Err("the audio thread did not start".to_string()),
        }
    }
    let ring = Arc::new(Ring::new(source.rate as usize * source.channels)); // one second

    let (said, heard) = channel::<Result<u32, String>>();
    let (src, r, m, s) = (source.clone(), ring.clone(), meters.clone(), stop.clone());
    threads.push(std::thread::spawn(move || unsafe {
        m.priority.fetch_add(pro_audio() as u64, Ordering::Relaxed);
        if let Err(e) = capture(&src, &r, &m, &s, &said) {
            let _ = said.send(Err(format!("loopback capture on {}: {e}", src.name)));
            eprintln!("capture stopped: {e}");
        }
    }));
    let in_block = match heard.recv() {
        Ok(Ok(block)) => block,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("the capture thread ended".to_string()),
    };

    let (said, heard) = channel::<Result<Opened, String>>();
    let (tgt, m, s) = (target.clone(), meters.clone(), stop.clone());
    let reader = Reader {
        ring,
        meters: meters.clone(),
        in_ch: source.channels,
        in_rate: source.rate,
        out_ch: target.channels,
        nominal: source.rate as f64 / target.rate as f64,
        cushion: 0,
        slack: 0,
        in_block,
        phase: 0.0,
        primed: false,
        usual: 0.0,
        lean: 0.0,
    };
    threads.push(std::thread::spawn(move || unsafe {
        m.priority.fetch_add(pro_audio() as u64, Ordering::Relaxed);
        let mut process = process.lock().unwrap();
        if let Err(e) = playback(&tgt, reader, false, in_block, cushion_ms, &mut *process, &m, &s, &said) {
            let _ = said.send(Err(format!("playback on {}: {e}", tgt.name)));
            eprintln!("playback stopped: {e}");
        }
    }));
    let mut opened = match heard.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("the playback thread did not start".to_string()),
    };
    opened.in_block = in_block;
    opened.note = why_not;
    Ok(Running { stop, threads, meters, opened, in_rate: source.rate, out_rate: target.rate })
}

/// Play a tone to an output like any program would (shared mode): a fifth of a second of silence, the
/// tone for `seconds`, and done.
pub fn tone(output: &Output, hz: f64, level: f32, seconds: f64) -> JoinHandle<()> {
    let output = output.clone();
    std::thread::spawn(move || unsafe {
        let play = || -> Win<()> {
            let client: IAudioClient = device(&output.id)?.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, format, None)?;
            let render: IAudioRenderClient = client.GetService()?;
            let room = client.GetBufferSize()?;
            client.Start()?;
            let (ch, rate) = (output.channels, output.rate as f64);
            let (begins, ends) = ((0.2 * rate) as u64, ((0.2 + seconds) * rate) as u64);
            let mut n = 0u64;
            while n < ends {
                let free = room - client.GetCurrentPadding()?;
                if free > 0 {
                    let out = std::slice::from_raw_parts_mut(render.GetBuffer(free)? as *mut f32, free as usize * ch);
                    for frame in out.chunks_mut(ch) {
                        let s = if n >= begins && n < ends { level as f64 * (std::f64::consts::TAU * hz * n as f64 / rate).sin() } else { 0.0 };
                        frame.fill(s as f32);
                        n += 1;
                    }
                    render.ReleaseBuffer(free, 0)?;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            // let what is still in the buffer play out: stopping at once would cut the tone mid-wave
            while client.GetCurrentPadding()? > 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
            client.Stop()
        };
        if let Err(e) = play() {
            eprintln!("tone on {}: {e}", output.name);
        }
    })
}
