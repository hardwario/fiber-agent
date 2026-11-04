#!/usr/bin/env python3
"""Passive listen: open PORT at 115200 with PARITY (N or E), print what arrives in SECS. Writes nothing unless SEND hex given."""
import os, sys, termios, time, select
port, par, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
send = bytes.fromhex(sys.argv[4]) if len(sys.argv) > 4 else b""
fd = os.open(port, os.O_RDWR | os.O_NOCTTY)
a = termios.tcgetattr(fd); a[0] = 0; a[1] = 0; a[3] = 0
a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL | (termios.PARENB if par == "E" else 0)
a[4] = a[5] = termios.B115200; a[6][termios.VMIN] = 0; a[6][termios.VTIME] = 0
termios.tcsetattr(fd, termios.TCSANOW, a); termios.tcflush(fd, termios.TCIOFLUSH)
if send: os.write(fd, send)
out = b""; end = time.time() + secs
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r: out += os.read(fd, 4096)
print(f"{port} {par}: {len(out)} bytes:", out[:200].hex(" "))
