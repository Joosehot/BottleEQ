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

In exclusive mode the path is one thread (`direct` in `src/wasapi.rs`): wait for a block from the
source, read it at the target's rate, process it, write it into the target's buffer. In shared mode it
is capture thread -> lock-free ring -> playback thread. The audio threads have Windows' "Pro Audio"
priority, wake on the device's own event, and never lock or allocate. All of it is on WASAPI directly,
through the `windows` crate.

## Use

```
bottle-eq devices                                  the output devices, their formats and blocks
bottle-eq run --from <device> --to <device>        capture what plays to one, play it to the other (Enter stops)
bottle-eq selftest --from <device> --to <device>   a quiet 1 kHz tone through the whole path, measured
```

A device is its number in `devices` or a part of its name. `--cushion-ms <n>` is the sound kept in the
target's buffer on top of what the two devices' blocks need (default 2). To use it: make the source the Windows
default output device, then `run`.

## Measured (2026-11-20, this machine)

`selftest` plays a 1 kHz tone at -40 dB to S/PDIF like any program would, and looks at what is captured
and at what is handed to the target: how much of it is the tone, the loudest jump between two frames (a
click shows there), whether the device ran out of sound, and the delay Bottle EQ adds (from a block
arriving from the source's engine to its beginning to play).

To the USB speakers, exclusive mode, 16-bit, periods of 144 frames (3 ms):

| path | cushion | tone | delay, mean | longest | ran out | clean |
|---|---|---|---|---|---|---|
| cpal, shared mode (the first version) | 25 ms | 1.5 s | 44 ms | | 0 | yes |
| two threads and a ring | 3 ms | 20 s | 11.9 ms | 17.6 ms | 0 | yes |
| two threads and a ring | 1 ms | 60 s | 8.2 ms | 15.7 ms | 16 | no: clicks |
| **direct** | 3 ms | 5 s | 9.1 ms | 11.4 ms | 0 | yes |
| **direct** | 2 ms (default) | 30 s | 6.0 ms | 12.7 ms | 0 | yes |
| **direct** | 1 ms | 30 s | 4.8 ms | 9.7 ms | 0 | yes |
| **direct** | 1 ms | 60 s | 4.6 ms | 9.9 ms | 0 | yes |
| **direct** | 1 ms | 20 s, three runs | | | 0 | yes |

The direct path (the default in exclusive mode): one thread; each block is read at the target's rate,
processed and written into the target's buffer as it arrives from the source, behind what is already
queued. With two threads the ring had to hold a whole block of the target's before every read, because
the two threads woke on two unrelated clocks; that block is what was saved. The longest delays are from
the first seconds, while the two clocks' difference is still being learned. Shared mode still goes
through the ring and two threads.

Not explained: in 2 runs of 10 the *captured* sound itself had one jump (0.00792 both times, where the
tone alone moves 0.0013 a frame), with no break reported by Windows; the path passed it on as it came.
It is either this test's tone or the source's engine at its short period. `selftest` says so when it
sees it.

### With music

`playtest` plays a WAV file to the source and keeps what was captured and what the target was given:

```
bottle-eq playtest --from "S/PDIF" --to "Speakers" --exclusive --file test/music.wav --level 0.1 --record test/out
python tools/compare.py test/music.wav test/out/captured.wav 0.1    # wave against wave
python tools/spectra.py test/out/captured.wav test/out/handed.wav   # band against band
```

40 s of Bach's Toccata and Fugue in D minor, BWV 565 (a public-domain recording from Wikimedia Commons,
`Toccata_et_Fugue_BWV565.ogg`, performed by Ashtar Moïra), played 20 dB down:

- No gap anywhere: ran out 0 times, 0 dropped, 0 late, 0 breaks. Delay 5.3 ms on average after the first
  3 s, 10.4 ms at the longest (the queue at its fullest and a whole period's wait at once).
- **Played -> captured** (the source side): the same sound, wave for wave (alike 1.00000 in every 43 ms
  window but the first, where the comparison had not yet found its place), 0.78 dB lower: the S/PDIF
  device's own volume setting, most likely.
- **Captured -> given to the speakers** (Bottle EQ itself, with nothing switched on: there is no
  equalizer yet, so flat is what it should be), level by octave band over the whole piece:

  | band (Hz) | 31.5 | 63 | 125 | 250 | 500 | 1k | 2k | 4k | 8k | 16k |
  |---|---|---|---|---|---|---|---|---|---|---|
  | 4-point read (first) | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | -0.01 | -0.12 | -0.78 |
  | windowed sinc (now) | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | +0.01 |

  And through the piece, in stretches of 5 s: the 16 kHz band was -0.68 to -0.90 dB with the 4-point
  read (and from one 85 ms window to the next anything from +0.06 to -2.06 dB, as the two clocks slid
  past each other); with the sinc it is 0.00 to +0.03 dB in every stretch. Window by window (937) the
  biggest difference in any band is 0.04 dB in the median (was 0.53), 1 window differs by more than
  3 dB (at 0.73 s, where the two files are least well lined up), none by 6 dB: no click or gap shows.
- The sinc reads 16 frames behind the newest sound (0.33 ms), which is counted in the delay: 5.8 ms on
  average, 9.9 ms at the longest, for this piece.

Not done: nobody has listened yet, and what the speakers themselves made of it was not recorded (there
are microphones on this machine; they were not used). `test/out/handed.wav` is what the speakers were
given, to listen to.

What the devices allow (`cargo run --release --example periods`): S/PDIF shared blocks down to 2.67 ms;
the USB speakers 10 ms shared, 3 ms exclusive. The monitor's output has no speakers.

Wrong turns, for the record: the time stamp WASAPI gives with a loopback block is not the moment it was
mixed (measured against it the delay came out as 4 ms); the ring kept what had gathered while the
target was being opened (14 ms); the test's own tone was cut mid-wave at its end and read as a click; a
counter that hid one "ran dry" hid sixteen.

From the toolkit: Vuuvee (`Desktop/Projektit/Vuuvee`) has its own biquad, EQ, matching EQ, FFT,
resampler and declicker (`src/dsp/`) for steps 2 to 4, and a live path on cpal at about 30 ms.
StrikeUFP pins threads to cores (`gpu/src/remote/win.rs`).
