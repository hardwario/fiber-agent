#!/usr/bin/env python3
"""Read-only AN3155 probe: sync (0x7F), GET_ID, GET_VERSION. Erases and writes nothing."""
import os, sys, termios, time, select
port = sys.argv[1]
fd = os.open(port, os.O_RDWR | os.O_NOCTTY)
a = termios.tcgetattr(fd)
a[0] = 0; a[1] = 0; a[3] = 0
a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL | termios.PARENB   # 8E1
a[4] = a[5] = termios.B115200
a[6][termios.VMIN] = 0; a[6][termios.VTIME] = 0
termios.tcsetattr(fd, termios.TCSANOW, a)
termios.tcflush(fd, termios.TCIOFLUSH)
def rd(n, t=0.5):
    out = b""; end = time.time() + t
    while len(out) < n and time.time() < end:
        r, _, _ = select.select([fd], [], [], end - time.time())
        if r: out += os.read(fd, n - len(out))
    return out
os.write(fd, b"\x7f"); r = rd(1)
print("sync:", r.hex() or "no answer", "(79=ACK, 1f=NACK: already synced)")
if r in (b"\x79", b"\x1f"):
    os.write(fd, b"\x02\xfd"); r = rd(5)
    print("GET_ID:", r.hex(), "-> pid", r[2:4].hex() if len(r) >= 4 else "?")
    os.write(fd, b"\x01\xfe"); r = rd(5)
    print("GET_VERSION:", r.hex())
