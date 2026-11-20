"""Does the path change the sound? Compares the spectra of two WAV files of the same music.

    python tools/spectra.py <before.wav> <after.wav>

The second file may be read between the first's samples (two clocks), which moves the phase of high
notes, so the two are not compared wave against wave but by how much sound each has in ten octave bands
(31.5 Hz to 16 kHz), window by window (85 ms, the second file lined up with the first to the frame).

- Over the whole file: the level of each band in the second against the first. Flat is 0 dB everywhere.
- Window by window: the biggest difference in any band that has sound in it. A click, a gap or a
  repeated piece is a window where the two differ by many dB.
"""
import sys

import numpy as np
import soundfile as sf
from scipy.signal import correlate

N = 4096
CENTERS = [31.5, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]


def load(path):
    x, rate = sf.read(path, dtype="float32")
    return (x[:, 0] if x.ndim > 1 else x).astype(np.float64), rate


a, rate = load(sys.argv[1])
b, _ = load(sys.argv[2])
power = np.cumsum(np.concatenate(([0.0], a * a)))
loud = int(np.argmax(power[rate:] - power[:-rate]))
lag = int(np.argmax(correlate(b, a[loud:loud + rate], "valid", method="fft"))) - loud

freqs = np.fft.rfftfreq(N, 1 / rate)
bands = [(freqs >= c / 2 ** 0.5) & (freqs < c * 2 ** 0.5) for c in CENTERS]
window = np.hanning(N)


def band_energy(x):
    spectrum = np.abs(np.fft.rfft(x * window)) ** 2
    return np.array([spectrum[m].sum() for m in bands])


total_a, total_b, worst = np.zeros(len(CENTERS)), np.zeros(len(CENTERS)), []
loudest = None
rows = []
for start in range(max(0, -lag), len(a) - N, N // 2):
    if start + lag + N > len(b):
        break
    ea, eb = band_energy(a[start:start + N]), band_energy(b[start + lag:start + lag + N])
    rows.append((start, ea, eb))
    total_a += ea
    total_b += eb
loudest = max(r[1].max() for r in rows)
for start, ea, eb in rows:
    heard = ea > loudest * 1e-5  # bands within 50 dB of the loudest band of the file
    if heard.any():
        diff = np.abs(10 * np.log10((eb[heard] + 1e-30) / (ea[heard] + 1e-30)))
        worst.append((diff.max(), start / rate))

print(f"{sys.argv[1]} -> {sys.argv[2]}  (the second is {lag} frames after the first)")
print("  band (Hz):      " + "".join(f"{c:>8g}" for c in CENTERS))
print("  second - first: " + "".join(f"{10 * np.log10(tb / ta):>+8.2f}" for ta, tb in zip(total_a, total_b)) + "  dB")
worst.sort(reverse=True)
d = np.array([w[0] for w in worst])
print(f"  {len(d)} windows of {N * 1000 / rate:.0f} ms: biggest difference in a band, median {np.median(d):.2f} dB, 99th percentile {np.percentile(d, 99):.2f} dB, worst {d.max():.2f} dB")
print(f"  windows differing by more than 3 dB: {(d > 3).sum()}, more than 6 dB: {(d > 6).sum()}")
for diff, at in worst[:5]:
    print(f"    {diff:.2f} dB at {at:.2f} s")
