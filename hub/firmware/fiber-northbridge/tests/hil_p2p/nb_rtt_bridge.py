#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Bridge the northbridge's BENCH_RTT_BRIDGE link onto a PTY, so `control-radio` can drive it.

    control-radio  <-PTY->  this script  <-RTT ch.0->  northbridge  <-868.1 MHz->  STICKER

The bench build (`west build ... -- -DBENCH_RTT_BRIDGE=ON`) carries the host<->modem HDLC link
over the RTT *shell* channel as text: the host writes `p2p_inject <hex>` and the modem answers
`P2P_OUT <hex>`. `control-radio` wants a serial device and honours `PROXIMOS_P2P_UART`, so this
translates between the two and nothing else.

**It is a byte pipe with no protocol knowledge and no crypto, deliberately.** The whole point of
the bench is to exercise the Rust central; a bridge that understood P2P would be a second
implementation of it (see `p2p_proto.py`, which is one, and is bench tooling only).

Usage:
    nb_rtt_bridge.py --nb-sn 822005108 [--elf build-nb-rtt/zephyr/zephyr.elf | --rtt-addr 0x...]
                     [--link /tmp/p2p-modem] [--device STM32WL55JC] [--speed 4000] [--verbose]
                     [--raw-log ~/p2p-bench/nb-raw.log]

then, in another shell:
    PROXIMOS_P2P_UART=/tmp/p2p-modem control-radio-p2p-host --no-mqtt

One RTT client per probe: stop `rttt` and `nb_hil.py` before starting this.
"""
from __future__ import annotations

import argparse
import errno
import os
import pty
import re
import select
import subprocess
import sys
import time
import tty

import pylink

# Same line shape `nb_hil.py` parses; kept identical so the two cannot drift apart.
P2P_OUT_RE = re.compile(rb"P2P_OUT ([0-9a-fA-F]+)")

HDLC_FLAG = 0x7E
# proto.h: P2P_MAX_ENCODED_LEN = 2 + 2 * P2P_MAX_FRAME_LEN = 570. Anything longer than this
# between two flags is noise, not a frame we failed to keep up with.
MAX_ENCODED_LEN = 570
# The firmware's `cmd_p2p_inject` decodes into a 512-byte buffer and refuses more by name; worth
# saying so here rather than letting the modem answer with a shell error nobody is reading.
INJECT_DECODE_LIMIT = 512


def read_rtt_addr_from_elf(elf_path: str) -> int:
    """_SEGGER_RTT moves when the buffer sizes change; read it from the .elf, never hardcode."""
    out = subprocess.check_output(
        ["arm-zephyr-eabi-nm", elf_path], stderr=subprocess.DEVNULL).decode()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[2] == "_SEGGER_RTT":
            return int(parts[0], 16)
    raise RuntimeError("_SEGGER_RTT not found in " + elf_path)


class Rtt:
    """Thin pylink RTT wrapper: chunked writes on channel 0, buffered reads. As in `nb_hil.py`."""

    def __init__(self, sn: int, device: str, rtt_addr: int, speed: int = 4000):
        jl = pylink.JLink()
        jl.open(serial_no=sn)                      # ALWAYS by serial number
        jl.set_tif(pylink.enums.JLinkInterfaces.SWD)
        jl.connect(device, speed=speed)
        jl.rtt_start(rtt_addr)
        time.sleep(0.3)
        try:
            jl.rtt_read(0, 4096)                   # drain whatever was buffered before we attached
        except Exception:
            pass
        self.jl = jl

    def write_line(self, s: str) -> None:
        data = (s + "\r\n").encode()
        i = 0
        while i < len(data):
            n = self.jl.rtt_write(0, list(data[i:i + 240]))
            if n > 0:
                i += n
            else:
                time.sleep(0.003)                  # down-buffer full; the target is draining it

    def read(self) -> bytes:
        try:
            return bytes(self.jl.rtt_read(0, 4096))
        except Exception:
            return b""

    def close(self) -> None:
        try:
            self.jl.rtt_stop()
        finally:
            self.jl.close()


def split_frames(buf: bytearray) -> tuple[list[bytes], bytearray]:
    """Peel whole HDLC frames off the front of `buf`.

    A frame is `7E <escaped payload> 7E`, and 7E cannot occur inside the payload because the
    encoder escapes it — so flag-to-flag is unambiguous. One `p2p_inject` must carry exactly one
    whole frame: half a frame would be fed to the modem's decoder as garbage, and two frames in
    one command would make the second unreachable.
    """
    frames: list[bytes] = []
    while True:
        start = buf.find(HDLC_FLAG)
        if start < 0:
            return frames, bytearray()             # nothing but noise so far
        end = buf.find(HDLC_FLAG, start + 1)
        if end < 0:
            del buf[:start]                        # keep the partial frame, drop noise before it
            return frames, buf
        if end == start + 1:                       # back-to-back flags: an empty run, skip one
            del buf[:start + 1]
            continue
        frame = bytes(buf[start:end + 1])
        del buf[:end + 1]
        if len(frame) <= MAX_ENCODED_LEN:
            frames.append(frame)
        # else: longer than the wire allows — a desync, not a frame. Dropped silently; the
        # central will retry, and a bridge that guessed here would corrupt the next real frame.


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nb-sn", type=int, required=True, help="northbridge J-Link serial number")
    ap.add_argument("--elf", default="build-nb-rtt/zephyr/zephyr.elf",
                    help="read _SEGGER_RTT from this .elf (default: %(default)s)")
    ap.add_argument("--rtt-addr", help="RTT control block address, instead of reading the .elf")
    ap.add_argument("--device", default="STM32WL55JC")
    ap.add_argument("--speed", type=int, default=4000, help="SWD kHz (EDU Mini maxes at 4000)")
    ap.add_argument("--link", default="/tmp/p2p-modem",
                    help="stable symlink to the PTY slave (default: %(default)s)")
    ap.add_argument("--verbose", action="store_true",
                    help="log every frame, its inject latency, and periodic byte counts")
    ap.add_argument("--raw-log", metavar="PATH",
                    help="append every byte the modem writes on RTT ch.0 to this file - shell "
                         "output, log lines and a fatal-handler register dump included. Put it "
                         "somewhere that survives a reboot; the only copy of the 2026-09-09 bus "
                         "fault lived in /tmp and was lost with it")
    a = ap.parse_args()

    addr = int(a.rtt_addr, 16) if a.rtt_addr else read_rtt_addr_from_elf(a.elf)
    print(f"_SEGGER_RTT @ {addr:#010x}", flush=True)
    rtt = Rtt(a.nb_sn, a.device, addr, a.speed)

    master, slave = pty.openpty()
    # Raw: the line discipline would otherwise translate CR/LF and act on flow-control bytes, and
    # HDLC frames contain arbitrary binary — including 0x11/0x13 (XON/XOFF) and 0x0D.
    tty.setraw(slave)
    slave_path = os.ttyname(slave)
    # Close the slave *now*, before anything opens it. `control-radio` opens the device through
    # tokio-serial, which asks for exclusive access; a slave fd still held here makes that open
    # fail. The master keeps the pair alive on its own.
    os.close(slave)
    os.set_blocking(master, False)

    try:
        if os.path.islink(a.link) or os.path.exists(a.link):
            os.unlink(a.link)
        os.symlink(slave_path, a.link)
        shown = f"{a.link} -> {slave_path}"
        uart = a.link
    except OSError:
        shown = slave_path
        uart = slave_path
    print(f"RTT ch.0 <-> {shown}\n  PROXIMOS_P2P_UART={uart}", flush=True)

    raw_log = open(os.path.expanduser(a.raw_log), "ab", buffering=0) if a.raw_log else None
    if raw_log is not None:
        raw_log.write(f"\n--- bridge attached {time.strftime('%Y-%m-%d %H:%M:%S')} ---\n".encode())
        print(f"raw RTT ch.0 -> {a.raw_log}", flush=True)

    pending = bytearray()        # host bytes not yet a whole frame
    rx_text = bytearray()        # modem text not yet a whole line
    to_modem = to_host = frames_in = frames_out = 0
    last_report = time.time()

    try:
        while True:
            # host -> modem
            r, _, _ = select.select([master], [], [], 0.005)
            if r:
                try:
                    data = os.read(master, 4096)
                except OSError as e:
                    # EIO simply means no process currently has the slave open — before
                    # control-radio starts, and again after it exits. Not an error here.
                    data = b"" if e.errno in (errno.EIO, errno.EAGAIN) else None
                    if data is None:
                        raise
                if data:
                    pending.extend(data)
                    frames, pending = split_frames(pending)
                    for f in frames:
                        if len(f) > INJECT_DECODE_LIMIT:
                            print(f"  [bridge] frame of {len(f)} B exceeds the firmware's "
                                  f"{INJECT_DECODE_LIMIT} B p2p_inject buffer — not injected",
                                  file=sys.stderr, flush=True)
                            continue
                        t0 = time.time()
                        rtt.write_line("p2p_inject " + f.hex())
                        dt = (time.time() - t0) * 1000
                        to_modem += len(f)
                        frames_in += 1
                        if a.verbose:
                            print(f"  [bridge] -> {len(f)} B in {dt:.1f} ms", file=sys.stderr,
                                  flush=True)

            # modem -> host
            chunk = rtt.read()
            if chunk:
                if raw_log is not None:
                    raw_log.write(chunk)
                rx_text.extend(chunk)
                while b"\n" in rx_text:
                    line, _, rest = rx_text.partition(b"\n")
                    rx_text = bytearray(rest)
                    m = P2P_OUT_RE.search(line)
                    if not m:
                        continue                   # shell prompts, `OK: injected ...`, log lines
                    try:
                        raw = bytes.fromhex(m.group(1).decode())
                    except ValueError:
                        continue                   # a truncated line; the central will retry
                    os.write(master, raw)
                    to_host += len(raw)
                    frames_out += 1
                    if a.verbose:
                        print(f"  [bridge] <- {len(raw)} B", file=sys.stderr, flush=True)

            if a.verbose and time.time() - last_report > 10:
                print(f"  [bridge] host->modem {frames_in} frames / {to_modem} B, "
                      f"modem->host {frames_out} frames / {to_host} B",
                      file=sys.stderr, flush=True)
                last_report = time.time()
    except KeyboardInterrupt:
        print("\nbridge stopped", flush=True)
    finally:
        rtt.close()
        os.close(master)
        if raw_log is not None:
            raw_log.close()
        try:
            if os.path.islink(a.link):
                os.unlink(a.link)
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
