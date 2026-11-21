//! The sound's way in and out. The rest of the program (the equalizer, the window, the tests) sees
//! only this: the output devices, and `run`, which captures what plays to one and plays it, through
//! `process`, to another. Under it there is one way for each system:
//!
//! - Windows: `wasapi.rs`, WASAPI loopback capture and exclusive-mode playback;
//! - Linux: `pipewire.rs`, a device of Bottle EQ's own in the PipeWire graph.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

#[cfg(windows)]
mod wasapi;
#[cfg(windows)]
pub use wasapi::{outputs, play, run, tone};
/// what the system is called when the one who uses it is told what to do there
#[cfg(windows)]
pub const SYSTEM: &str = "Windows";

#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(target_os = "linux")]
pub use pipewire::{outputs, play, run, tone};
#[cfg(target_os = "linux")]
pub const SYSTEM: &str = "Linux";

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
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) stamp_time: AtomicU64,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) stamp_frames: AtomicU64,
}

pub(crate) fn bits(x: f32) -> u32 {
    x.to_bits()
}

pub fn value(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
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
    pub(super) stop: Arc<AtomicBool>,
    pub(super) threads: Vec<JoinHandle<()>>,
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
        // Linux: what was put in the meters is the whole of it already (see `pipewire.rs`)
        if cfg!(target_os = "linux") {
            return (mean, longest);
        }
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
