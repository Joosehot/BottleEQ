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

Windows mixes every program's sound for one output device (an endpoint: the speakers, S/PDIF) into one
stream, one block ("period") at a time: 10 ms unless someone asks for less. Loopback is a capture
stream opened on an *output* endpoint: Windows hands over a copy of the finished mix on its way to that
device. Five things follow:

1. **It is a copy.** The device still plays the original, so Bottle EQ captures from one endpoint, the
   **source** (the one Windows plays to, which you do not hear), and plays the processed sound to
   another, the **target** (the one you listen to). On this machine the source is S/PDIF (nothing is
   plugged into it) and the target is the USB speakers.
2. **Silence is no data.** When nothing plays, loopback delivers nothing; playback then plays silence.
3. **Two devices, two clocks.** Their 48,000 Hz differ by some tens of parts per million. The ring
   between capture and playback is read a hair fast or slow (at most 0.1%, under 2 cents of pitch) so
   that its fill stays at the cushion, and the lasting difference is learned.
4. **The delay is made of blocks.** A sound waits for the source's block to end, then in the ring, then
   for the target's buffer. With 10 ms blocks on both sides that was 44 ms. So the source's engine is
   asked for its least block (IAudioClient3: 2.67 ms on S/PDIF; a silent stream at that period is
   enough), and the target is opened in **exclusive mode** at its least block (3 ms): Bottle EQ then
   owns the speakers and nothing else can play to them while it runs. `run` asks before taking them
   (`--exclusive` and `--shared` answer in advance).
5. **Exclusive mode cannot be listened to by loopback**, so the delay is measured on the clock: from a
   block arriving from the source's engine to its frames beginning to play at the target.

The path is: capture thread -> lock-free ring -> playback thread (`src/wasapi.rs`, `src/ring.rs`), on
WASAPI directly through the `windows` crate. Both threads have Windows' "Pro Audio" priority, wake on
the device's own event, and never lock or allocate.

## Use

```
bottle-eq devices                                  the output devices, their formats and blocks
bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other (Enter stops)
bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
```

A device is its number in `devices` or a part of its name. `--cushion-ms <n>` is the sound kept in the
ring on top of what the two devices' blocks need (default 3). To use it: make the source the Windows
default output device, then `run`.

## Measured (2026-11-20, this machine)

`selftest` plays a 1 kHz tone at -40 dB to S/PDIF like any program would, and looks at what is captured
and at what is handed to the target: how much of it is the tone, the loudest jump between two frames (a
click shows there), whether the ring ran dry, and the delay.

To the USB speakers, exclusive mode, 16-bit, blocks of 144 frames (3 ms):

| cushion | tone | delay, mean | longest | ran dry | clean |
|---|---|---|---|---|---|
| 3 ms | 20 s | 11.9 ms | 17.6 ms | 0 | yes |
| 2 ms | 20 s | 10.9 ms | 16.8 ms | 0 | yes |
| 1 ms | 20 s | 9.5 ms | 14.1 ms | 0 | yes |
| 1 ms | 60 s | 8.4 ms | 13.6 ms | 0 | no: one jump |
| 1 ms | 60 s | 8.2 ms | 15.7 ms | 16 | no: clicks |

So under 10 ms is reached but not cleanly: at a 1 ms cushion the ring sometimes runs dry. The default
cushion is 3 ms: 11.9 ms, clean in these runs (down from 44 ms with cpal). The longest delays are from
the first seconds, while the clocks' difference is still being learned.

To the monitor's output (it has no speakers; a comparison only): 8.1 ms mean, 9.8 ms longest over 30 s,
clean while the tone played.

What the devices allow (`cargo run --release --example periods`): S/PDIF shared blocks down to 2.67 ms;
the USB speakers 10 ms shared, 3 ms exclusive.

Why 12 ms and not less: the two threads wake on two unrelated clocks, so the ring must hold a whole
target block (3 ms) plus half a source block (1.3 ms) plus the cushion before each read, and then the
target's block (3 ms) comes on top. Not yet built: playing straight from the capture thread, each block
as it arrives, which takes the target's block out of the ring.

Wrong turns, for the record: the time stamp WASAPI gives with a loopback block is not the moment it was
mixed (measured against it the delay came out as 4 ms); the ring kept what had gathered while the
target was being opened (14 ms); the test's own tone was cut mid-wave at its end and read as a click.

From the toolkit: Vuuvee (`Desktop/Projektit/Vuuvee`) has its own biquad, EQ, matching EQ, FFT,
resampler and declicker (`src/dsp/`) for steps 2 to 4, and a live path on cpal at about 30 ms.
StrikeUFP pins threads to cores (`gpu/src/remote/win.rs`).
