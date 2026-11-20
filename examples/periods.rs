//! What each output device allows: the periods (block lengths) Windows can run its audio engine at
//! for it in shared mode (IAudioClient3), and the device's own periods (shared default, exclusive
//! minimum). The delay through Bottle EQ cannot be shorter than these blocks.
//!
//!   cargo run --release --example periods

use windows::core::Result;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

fn main() -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let all: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let outputs = all.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..outputs.GetCount()? {
            let device = outputs.Item(i)?;
            let name = device.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?.to_string();
            let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let rate = (*format).nSamplesPerSec as f64;
            let (mut usual, mut step, mut least, mut most) = (0u32, 0u32, 0u32, 0u32);
            let engine = client.GetSharedModeEnginePeriod(format, &mut usual, &mut step, &mut least, &mut most);
            let (mut shared, mut exclusive) = (0i64, 0i64);
            client.GetDevicePeriod(Some(&mut shared), Some(&mut exclusive))?;
            let ms = |frames: u32| frames as f64 * 1000.0 / rate;
            println!("{name}  ({} Hz)", rate);
            match engine {
                Ok(()) => println!("  shared engine period: usual {usual} frames ({:.2} ms), least {least} ({:.2} ms), most {most} ({:.2} ms), in steps of {step}", ms(usual), ms(least), ms(most)),
                Err(e) => println!("  shared engine period: not told ({e})"),
            }
            println!("  device period: shared {:.2} ms, exclusive at least {:.2} ms", shared as f64 / 10_000.0, exclusive as f64 / 10_000.0);
        }
    }
    Ok(())
}
