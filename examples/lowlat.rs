//! Two things the under-10-ms path rests on, tried without making a sound:
//!
//! 1. Does loopback on the source arrive in small blocks when the source's engine is asked to run at its
//!    least period (IAudioClient3)? A silent stream at the least period is opened on the source, then
//!    loopback is captured for two seconds while a second, ordinary silent stream keeps the engine fed.
//! 2. Can the target be opened in exclusive mode at its least period, and how long is its buffer then?
//!
//!   cargo run --release --example lowlat -- "<source name part>" "<target name part>"

use std::time::{Duration, Instant};
use windows::core::{Result, GUID};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

unsafe fn device(part: &str) -> Result<(IMMDevice, String)> {
    let all: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let outputs = all.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    for i in 0..outputs.GetCount()? {
        let d = outputs.Item(i)?;
        let name = d.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?.to_string();
        if name.to_lowercase().contains(&part.to_lowercase()) {
            return Ok((d, name));
        }
    }
    panic!("no output device matches {part}");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        // 1. the source: least engine period, then loopback
        let (source, name) = device(&args[0])?;
        println!("source: {name}");
        let holder: IAudioClient3 = source.Activate(CLSCTX_ALL, None)?;
        let format = holder.GetMixFormat()?;
        let rate = (*format).nSamplesPerSec as f64;
        let (mut usual, mut step, mut least, mut most) = (0u32, 0u32, 0u32, 0u32);
        holder.GetSharedModeEnginePeriod(format, &mut usual, &mut step, &mut least, &mut most)?;
        for (label, hold) in [("engine at its usual period", false), ("a silent stream asks for the least period", true)] {
            if hold {
                holder.InitializeSharedAudioStream(0, least, format, None)?;
                holder.Start()?;
                let (mut f, mut now) = (std::ptr::null_mut(), 0u32);
                holder.GetCurrentSharedModeEnginePeriod(&mut f, &mut now)?;
                println!("  engine period now: {now} frames ({:.2} ms)", now as f64 * 1000.0 / rate);
            }
            let capture: IAudioClient = source.Activate(CLSCTX_ALL, None)?;
            capture.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, format, None)?;
            let event: HANDLE = CreateEventW(None, false, false, None)?;
            capture.SetEventHandle(event)?;
            let packets: IAudioCaptureClient = capture.GetService()?;
            // loopback is silent-as-no-data, so something must play: an ordinary silent stream of zeros
            let feeder: IAudioClient = source.Activate(CLSCTX_ALL, None)?;
            feeder.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None)?;
            let zeros: IAudioRenderClient = feeder.GetService()?;
            let room = feeder.GetBufferSize()?;
            zeros.GetBuffer(room)?;
            zeros.ReleaseBuffer(room, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            feeder.Start()?;
            capture.Start()?;
            let began = Instant::now();
            let (mut blocks, mut frames, mut biggest, mut last, mut longest) = (0u32, 0u64, 0u32, Instant::now(), Duration::ZERO);
            while began.elapsed() < Duration::from_secs(2) {
                if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                    continue;
                }
                while packets.GetNextPacketSize()? > 0 {
                    let (mut data, mut n, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    packets.GetBuffer(&mut data, &mut n, &mut flags, None, None)?;
                    packets.ReleaseBuffer(n)?;
                    if blocks > 0 {
                        longest = longest.max(last.elapsed());
                    }
                    last = Instant::now();
                    blocks += 1;
                    frames += n as u64;
                    biggest = biggest.max(n);
                }
                // keep the feeder's buffer full of silence
                let pad = feeder.GetCurrentPadding()?;
                if room > pad {
                    zeros.GetBuffer(room - pad)?;
                    zeros.ReleaseBuffer(room - pad, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                }
            }
            capture.Stop()?;
            feeder.Stop()?;
            println!(
                "  {label}: {blocks} loopback blocks in 2 s, {:.0} frames each on average ({:.2} ms), biggest {biggest}, longest wait {:.2} ms",
                frames as f64 / blocks.max(1) as f64,
                frames as f64 / blocks.max(1) as f64 * 1000.0 / rate,
                longest.as_secs_f64() * 1000.0
            );
        }
        holder.Stop()?;

        // 2. the target: exclusive mode at its least period
        let (target, name) = device(&args[1])?;
        println!("target: {name}");
        let probe: IAudioClient = target.Activate(CLSCTX_ALL, None)?;
        let (mut shared, mut least) = (0i64, 0i64);
        probe.GetDevicePeriod(Some(&mut shared), Some(&mut least))?;
        for bits in [16u16, 24, 32] {
            let wave = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                    nChannels: 2,
                    nSamplesPerSec: 48000,
                    nAvgBytesPerSec: 48000 * 2 * bits as u32 / 8,
                    nBlockAlign: 2 * bits / 8,
                    wBitsPerSample: bits,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: bits },
                dwChannelMask: 3,
                SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
            };
            let format = &wave as *const _ as *const WAVEFORMATEX;
            let client: IAudioClient = target.Activate(CLSCTX_ALL, None)?;
            let ok = client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, format, None);
            if ok.is_err() {
                println!("  {bits}-bit PCM 48000 Hz stereo: not supported in exclusive mode ({:?})", ok);
                continue;
            }
            let mut client = client;
            let mut opened = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, least, least, format, None::<*const GUID>);
            if opened.as_ref().is_err_and(|e| e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED) {
                // the device wants a whole number of its own blocks: ask how many frames that is, and open again
                let aligned = client.GetBufferSize()?;
                let period = (10_000_000.0 * aligned as f64 / 48000.0 + 0.5) as i64;
                println!("  {bits}-bit PCM: {:.2} ms is not a whole block for this device; it wants {aligned} frames ({:.2} ms)", least as f64 / 10_000.0, aligned as f64 / 48.0);
                client = target.Activate(CLSCTX_ALL, None)?;
                opened = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, period, format, None::<*const GUID>);
            }
            match opened {
                Ok(()) => {
                    let frames = client.GetBufferSize()?;
                    let latency = client.GetStreamLatency()?;
                    println!(
                        "  {bits}-bit PCM: exclusive at {:.2} ms opens; buffer {frames} frames ({:.2} ms), Windows says the stream's latency is {:.2} ms",
                        least as f64 / 10_000.0,
                        frames as f64 / 48.0,
                        latency as f64 / 10_000.0
                    );
                }
                Err(e) => println!("  {bits}-bit PCM: exclusive at {:.2} ms does not open: {e}", least as f64 / 10_000.0),
            }
        }
    }
    Ok(())
}
