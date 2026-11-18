#!/usr/bin/env python3
"""Generate deterministic 16-bit stereo sine WAV sources (stdlib only).

usage: gen_signal.py OUT.wav RATE TOTAL_FRAMES START END
Writes frames [START, END) of the signal:
  L = 0.5*sin(2*pi*997*n/RATE), R = 0.5*sin(2*pi*1499*n/RATE + 0.3)
"""
import math
import struct
import sys
import wave


def main():
    out, rate, total, start, end = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
    assert 0 <= start <= end <= total
    w = wave.open(out, "wb")
    w.setnchannels(2)
    w.setsampwidth(2)
    w.setframerate(rate)
    chunk = 16384
    n = start
    while n < end:
        m = min(n + chunk, end)
        buf = bytearray()
        for i in range(n, m):
            l = int(round(0.5 * math.sin(2 * math.pi * 997 * i / rate) * 32767))
            r = int(round(0.5 * math.sin(2 * math.pi * 1499 * i / rate + 0.3) * 32767))
            buf += struct.pack("<hh", l, r)
        w.writeframes(bytes(buf))
        n = m
    w.close()


if __name__ == "__main__":
    main()
