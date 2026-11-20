# Bottle EQ

A system-wide audio equalizer for Windows with a live visualizer, in Rust. Built in five steps; step 1
is done.

1. **Audio capture** (done): what Windows plays is captured by WASAPI loopback and played back out,
   untouched.
2. EQ engine: a bank of biquad filters, 10 bands.
3. "Flat" button, with a hook for per-device correction curves.
4. Visualizer: an always-on-top window with the live spectrum and the EQ curve.
5. Draggable curve.

## How the sound gets through (WASAPI loopback)

Windows mixes every program's sound for one output device (an endpoint: the speakers, the monitor,
S/PDIF) into one stream. Loopback is a capture stream opened on an *output* endpoint: Windows hands
over a copy of the finished mix on its way to that device. Four things follow:

1. **It is a copy.** The device still plays the original; loopback cannot change what that device
   plays. So Bottle EQ captures from one endpoint, the **source** (the one Windows plays to), and plays
   the processed sound to another, the **target** (the one you listen to). The source must be a device
   you do not hear: an output with nothing plugged in, or a virtual audio cable.
2. **Silence is no data.** When nothing plays, loopback delivers nothing. Playback then plays silence
   and waits for a cushion of sound before it starts again.
3. **Two devices, two clocks.** The source's 48,000 Hz and the target's are two crystals a few parts
   in a hundred thousand apart. The ring between capture and playback is read a hair fast or slow (at
   most 0.1%, under 2 cents of pitch) so that its fill stays at the cushion.
4. **Formats may differ.** Channels are mapped (front left and right kept); a different sample rate is
   covered by the same interpolating read.

The path is: capture callback -> lock-free ring -> playback callback (`src/audio.rs`, `src/ring.rs`).
Neither audio thread locks, allocates or waits for the other.

## Use

```
bottle-eq devices                                  the output devices, their formats, where Windows plays
bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other (Enter stops)
bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
```

A device is its number in `devices` or a part of its name. `--buffer-ms <n>` sets the cushion
(default 25). To use it: make the source the Windows default output device, then `run`.

## Measured (2026-11-20, this machine)

Three outputs, all stereo 48,000 Hz float: S/PDIF (nothing plugged in), USB speakers, the monitor.
`selftest` plays a 1 kHz tone at -40 dB to the source like any program would, and listens by loopback
at the source and at the target:

| source -> target | tone | at the source | at the target | 1 kHz share | clicks | ran dry / dropped | added delay |
|---|---|---|---|---|---|---|---|
| S/PDIF -> monitor | 1.5 s | -40.8 dB | -41.4 dB | 99% | none | 0 / 0 | 44 ms |
| S/PDIF -> monitor | 20 s | -40.8 dB | -41.4 dB | 100% | none | 0 / 0 | 46 ms |
| S/PDIF -> USB speakers | 1.5 s | -40.8 dB | -40.8 dB | 99% | none | 0 / 0 | 44 ms |

After 20 s the ring was being read 51 ppm fast: that is the two clocks' difference being followed.
The tone arrives 0.8 dB lower at the S/PDIF loopback than it was played, and 0.6 dB lower again at the
monitor: not yet explained (the devices' own volume settings would do it; not checked).

The first version of the clock correction followed the ring's fill from callback to callback and made
the pitch wander (the tone was only 84% at 1 kHz); it now follows the fill's one-second average.
