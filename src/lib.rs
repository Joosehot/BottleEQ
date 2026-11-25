//! Bottle EQ's sound engine as a library, for programs that make their own sound (Bottle Roar): the
//! equalizer with its correction, Bass, Sharpness, Softness, Loudness, volume, headroom and limiter
//! (`eq`), and the calibrations kept by name (`calibration`). Without the app's window, devices or
//! measuring: `bottle-eq = { path = "...", default-features = false }`.

pub mod calibration;
pub mod eq;
