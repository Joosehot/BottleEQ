"""Is the sound the same at two points of the path? Compares two WAV files window by window.

    python tools/compare.py <before.wav> <after.wav> [gain]

The second file may be late against the first and a hair faster or slower (two clocks), so each window
of the first (about 43 ms) is looked for in the second near where it should be, and how alike the two
are is the normalized correlation at the best place: 1.0 is the same sound. A click, a gap or a
repeated piece shows as a window that is not alike. Windows that are nearly silent are left out.
`gain` scales the first file (the level it was played at).
"""
import sys

import numpy as np
import soundfile as sf
from scipy.signal import correlate

N, SEARCH = 2048, 96


def load(path):
    x, rate = sf.read(path, dtype="float32")
    return (x[:, 0] if x.ndim > 1 else x).astype(np.float64), rate


a, rate = load(sys.argv[1])
b, rate_b = load(sys.argv[2])
a *= float(sys.argv[3]) if len(sys.argv) > 3 else 1.0
assert rate == rate_b, "different rates"

# where the first file's sound begins in the second: the loudest stretch of the first, looked for everywhere
power = np.cumsum(np.concatenate(([0.0], a * a)))
loud = int(np.argmax(power[rate:] - power[:-rate]))
piece = a[loud:loud + rate]
corr = correlate(b, piece, "valid", method="fft")
lag = int(np.argmax(corr)) - loud
print(f"{sys.argv[1]} -> {sys.argv[2]}")
print(f"  the second begins {lag} frames ({lag * 1000 / rate:.1f} ms) after the first in the files")

alike, lags, quiet, levels = [], [], 0, []
floor = np.sqrt(np.mean(a * a)) * 0.05
for start in range(0, len(a) - N, N):
    w = a[start:start + N]
    if np.sqrt(np.mean(w * w)) < floor:
        quiet += 1
        continue
    lo = start + lag - SEARCH
    if lo < 0 or lo + N + 2 * SEARCH > len(b):
        continue
    seg = b[lo:lo + N + 2 * SEARCH]
    c = np.correlate(seg, w, "valid")
    k = int(np.argmax(c))
    m = seg[k:k + N]
    alike.append(c[k] / (np.linalg.norm(w) * np.linalg.norm(m) + 1e-20))
    lags.append(lag + k - SEARCH)
    levels.append(20 * np.log10(np.linalg.norm(m) / np.linalg.norm(w)))
    lag = lags[-1]  # follow the drift

alike, lags, levels = np.array(alike), np.array(lags), np.array(levels)
print(f"  {len(alike)} windows of {N * 1000 / rate:.0f} ms compared ({quiet} nearly silent ones left out)")
print(f"  how alike: mean {alike.mean():.5f}, least {alike.min():.5f}; windows under 0.999: {(alike < 0.999).sum()}, under 0.99: {(alike < 0.99).sum()}")
print(f"  level of the second against the first: {np.median(levels):+.2f} dB (from {levels.min():+.2f} to {levels.max():+.2f})")
print(f"  the second's lag went from {lags[0]} to {lags[-1]} frames over {len(a) / rate:.0f} s; biggest step between two windows: {np.abs(np.diff(lags)).max()} frames")
for i in np.argsort(alike)[:3]:
    print(f"    least alike: {alike[i]:.5f} at {i * N / rate:.2f} s of the windows compared")
