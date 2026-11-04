#!/usr/bin/env python3
"""Send one text line to PORT (115200 8N1) and print the decoded reply."""
import os, sys, termios, time, select, re
port, line, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
fd = os.open(port, os.O_RDWR | os.O_NOCTTY)
a = termios.tcgetattr(fd); a[0] = 0; a[1] = 0; a[3] = 0
a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
a[4] = a[5] = termios.B115200; a[6][termios.VMIN] = 0; a[6][termios.VTIME] = 0
termios.tcsetattr(fd, termios.TCSANOW, a); termios.tcflush(fd, termios.TCIOFLUSH)
os.write(fd, b"\r"); time.sleep(0.2); termios.tcflush(fd, termios.TCIFLUSH)
os.write(fd, line.encode() + b"\r")
out = b""; end = time.time() + secs
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r: out += os.read(fd, 4096)
txt = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", out.decode("utf-8", "replace"))
print(f"--- {line!r} ---"); print(txt.strip()[:1500])
