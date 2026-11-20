"""Does the equalizer do, in the running path, what its curve says?

    python tools/eqcheck.py <captured.wav> <handed.wav> <the curve, as given to bottle-eq>

    e.g. ... --gains 6,0,-4,0,3,-6,0,4,0,-3     or     ... --mode rock --points "100:cut 12k:+8:spike"

Noise is played through Bottle EQ with those gains (`bottle-eq playtest --gains ...`). The spectrum of
what the target was given, over the spectrum of what was captured, is what the equalizer did. It is set
against the curve the program says those gains make (`bottle-eq curve --gains ... --csv`), every third
of an octave. The headroom (the whole sound turned down by the curve's top) is the same at every
frequency and is taken as the median difference.
"""
import subprocess
import sys

import numpy as np
import soundfile as sf
from scipy.signal import correlate, welch

EXE = r"C:\Users\joose\BottleEQ\target\release\bottle-eq.exe"


def load(path):
    x, rate = sf.read(path, dtype="float32")
    return (x[:, 0] if x.ndim > 1 else x).astype(np.float64), rate


a, rate = load(sys.argv[1])
b, _ = load(sys.argv[2])
curve = sys.argv[3:] if sys.argv[3].startswith("--") else ["--gains", sys.argv[3]]
power = np.cumsum(np.concatenate(([0.0], a * a)))
loud = int(np.argmax(power[rate:] - power[:-rate]))
lag = int(np.argmax(correlate(b, a[loud:loud + rate], "valid", method="fft"))) - loud
# the stretch where both have the noise, past the first two seconds (the gains are still sliding in there)
start = max(loud - 5 * rate, 0, -lag) + 2 * rate
n = min(len(a) - start, len(b) - start - lag) - rate
a, b = a[start:start + n], b[start + lag:start + lag + n]

f, pa = welch(a, rate, nperseg=32768)
_, pb = welch(b, rate, nperseg=32768)
said = [line.split(",") for line in subprocess.run([EXE, "curve", *curve, "--csv"], capture_output=True, text=True).stdout.split() if "," in line]
said = [(float(hz), float(db)) for hz, db in said]

rows = []
for hz, db in said:
    band = (f >= hz * 2 ** (-1 / 24)) & (f < hz * 2 ** (1 / 24))
    if band.sum() >= 2 and 25 <= hz <= 19000:
        rows.append((hz, db, 10 * np.log10(pb[band].sum() / pa[band].sum())))
rows = np.array(rows)
room = np.median(rows[:, 2] - rows[:, 1])
error = rows[:, 2] - rows[:, 1] - room
print(f"{' '.join(curve)}: {n / rate:.0f} s of noise compared")
print(f"  the whole sound is {room:+.2f} dB (headroom); the curve's top is {max(db for _, db in said):+.2f} dB")
print("      Hz   the curve   the sound   difference")
for i in range(0, len(rows), 4):
    hz, db, got = rows[i]
    print(f"  {hz:6.0f}   {db:+7.2f}   {got - room:+9.2f}   {error[i]:+10.2f}")
print(f"  over {len(rows)} points from {rows[0, 0]:.0f} Hz to {rows[-1, 0]:.0f} Hz: mean difference {np.abs(error).mean():.2f} dB, biggest {np.abs(error).max():.2f} dB at {rows[np.argmax(np.abs(error)), 0]:.0f} Hz")
