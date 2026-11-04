#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
Two-probe P2P HIL procedure: drive THIS northbridge firmware over J-Link RTT and
play the P2P *central* against a real STICKER device under test -- no Proximos
control-radio, no CM4, no PTY bridge. One process, two probes.

  probe A (--nb-sn)  : northbridge, flashed with the bench build
                       (`west build ... -- -DBENCH_RTT_BRIDGE=ON`). Driven over
                       RTT as the radio modem: HDLC host<->modem frames go in as
                       `p2p_inject <hex>` and come back as `P2P_OUT <hex>` lines.
  probe B (--dut-sn) : STICKER under test, main app in `radio-mode p2p`, sharing
                       the DUT's LoRaWAN AppKey. Optional -- used only to trigger
                       a fresh join (`ats radio unjoin`) and read `ats radio
                       status`. Without it, reset/observe the DUT by hand.

The central schedules its JoinAccept/ACK at an ABSOLUTE modem-clock timestamp
(uplink_end + rx1_delay) via TX_SCHEDULE, so the northbridge fires the downlink
precisely inside the node's RX1 window; the host only has to deliver the
schedule in time, not hit the window itself.

All P2P crypto/framing is in p2p_proto.py, verified offline against the
firmware's own known-answer vectors (test_p2p_proto.py) -- run that first.

Needs: pylink-square, pycryptodome. Example:
  python3 nb_hil.py --nb-sn 822005110 --dut-sn 822005109 \\
      --app-key f6a809a429576f49fce2a946367ab641 --dev-eui 70b3d57ed0000abe
"""
from __future__ import annotations
import argparse
import re
import subprocess
import sys
import time

import pylink

import p2p_proto as P

P2P_OUT_RE = re.compile(rb"P2P_OUT ([0-9a-fA-F]+)")


def read_rtt_addr_from_elf(elf_path: str) -> int:
    """_SEGGER_RTT moves when the down-buffer size changes; read it from the .elf."""
    out = subprocess.check_output(
        ["arm-zephyr-eabi-nm", elf_path], stderr=subprocess.DEVNULL).decode()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[2] == "_SEGGER_RTT":
            return int(parts[0], 16)
    raise RuntimeError("_SEGGER_RTT not found in " + elf_path)


class Rtt:
    """Thin pylink RTT wrapper: chunked shell writes + line-buffered reads."""

    def __init__(self, sn, device, rtt_addr, speed=4000):
        jl = pylink.JLink()
        jl.open(serial_no=sn)                     # ALWAYS by SN (see CLAUDE.md)
        jl.set_tif(pylink.enums.JLinkInterfaces.SWD)
        jl.connect(device, speed=speed)
        jl.rtt_start(rtt_addr)
        time.sleep(0.3)
        try:
            jl.rtt_read(0, 4096)                  # drain stale
        except Exception:
            pass
        self.jl = jl
        self._buf = b""

    def write_line(self, s: str):
        data = (s + "\r\n").encode()
        i = 0
        while i < len(data):
            n = self.jl.rtt_write(0, list(data[i:i + 240]))
            if n > 0:
                i += n
            else:
                time.sleep(0.003)

    def read(self) -> bytes:
        try:
            d = self.jl.rtt_read(0, 2048)
        except Exception:
            d = []
        return bytes(d)

    def close(self):
        try:
            self.jl.rtt_stop()
        finally:
            self.jl.close()


class NbModem:
    """The northbridge as a P2P radio modem, over the bench RTT bridge."""

    def __init__(self, rtt: Rtt):
        self.rtt = rtt
        self._acc = b""

    def send(self, hdlc_frame: bytes):
        self.rtt.write_line("p2p_inject " + hdlc_frame.hex())

    def poll(self, timeout_s: float):
        """Yield (type, seq, body) for host<->modem frames seen within timeout."""
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            self._acc += self.rtt.read()
            while b"\n" in self._acc:
                line, self._acc = self._acc.split(b"\n", 1)
                m = P2P_OUT_RE.search(line)
                if m:
                    raw = bytes.fromhex(m.group(1).decode())
                    for f in P.hdlc_decode_frames(raw):
                        yield f
            time.sleep(0.02)

    def wait_frame(self, want_type, timeout_s):
        for ftype, seq, body in self.poll(timeout_s):
            if ftype == want_type:
                return seq, body
        return None

    # --- host->modem commands (fire, then optionally confirm the RSP) --------
    def set_radio_config(self, freq, sf, bw, cr, preamble, tx_power, flags):
        self.send(P.enc_set_radio_config(freq, sf, bw, cr, preamble, tx_power, flags))

    def rx_start(self):
        self.send(P.enc_rx_start())

    def tx_schedule(self, tx_id, t_ms, frame):
        self.send(P.enc_tx_schedule(tx_id, t_ms, frame))


class DutShell:
    """Optional STICKER-under-test control over its own RTT shell."""

    def __init__(self, rtt: Rtt):
        self.rtt = rtt

    def cmd(self, s: str, wait_s=2.0) -> str:
        self.rtt.write_line(s)
        t0 = time.time()
        out = b""
        while time.time() - t0 < wait_s:
            out += self.rtt.read()
            time.sleep(0.03)
        return re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", out.decode("latin1"))


def run(args) -> int:
    app_key = bytes.fromhex(args.app_key)
    dev_eui = args.dev_eui.replace(":", "").replace("-", "")
    net_id = int(args.net_id, 0)
    dev_addr = int(args.dev_addr, 0)
    central_nonce = int(args.central_nonce, 0)
    rx1_delay = args.rx1_delay

    nb = NbModem(Rtt(args.nb_sn, "STM32WL55JC", int(args.rtt_addr, 0)))
    dut = DutShell(Rtt(args.dut_sn, "STM32WLE5CC", int(args.dut_rtt_addr, 0))) if args.dut_sn else None
    fails = []

    print(f"[central] radio {args.freq} Hz SF{args.sf} BW125; net_id={net_id:#x} "
          f"dev_addr={dev_addr:#x} rx1={rx1_delay}s")
    nb.set_radio_config(args.freq, args.sf, P.BW_125K, 1, 8, args.tx_power, 0)
    time.sleep(0.2)
    nb.rx_start()
    time.sleep(0.2)

    if dut:
        print("[dut] ats radio unjoin (reboots -> fresh join)")
        print(dut.cmd("ats radio unjoin"))
        time.sleep(3.0)   # let it reboot + start the boot-window join

    # ---- 1) JOIN ---------------------------------------------------------
    print("[central] waiting for JoinRequest ...")
    hit = None
    for ftype, seq, body in nb.poll(args.join_timeout):
        if ftype != P.EVT_RX:
            continue
        rx = P.parse_evt_rx(body)
        try:
            jr = P.parse_join_request(rx["frame"], app_key)
        except ValueError:
            continue
        hit = (rx, jr)
        break
    if not hit:
        print("FAIL: no valid JoinRequest seen")
        return 2
    rx, jr = hit
    print(f"[central] JoinRequest dev_eui={jr['dev_eui']} dev_nonce={jr['dev_nonce']} "
          f"fw={'.'.join(map(str, jr['fw']))} RSSI={rx['rssi']} SNR={rx['snr']}")
    if dev_eui and jr["dev_eui"] != dev_eui.lower():
        fails.append(f"dev_eui mismatch: got {jr['dev_eui']}, expected {dev_eui.lower()}")

    ja = P.build_join_accept(app_key, jr["dev_nonce"], net_id, dev_addr,
                             central_nonce, rx1_delay)
    aim = rx["t_ms"] + rx1_delay * 1000
    nb.tx_schedule(1, aim, ja)
    print(f"[central] JoinAccept scheduled at modem t={aim} ms")
    td = nb.wait_frame(P.EVT_TX_DONE, 5.0)
    if not td or P.parse_evt_tx_done(td[1])["status"] != 0:
        fails.append("JoinAccept EVT_TX_DONE not OK")
    sk = P.session_key(app_key, jr["dev_nonce"], central_nonce, bytes.fromhex(jr["dev_eui"]))

    # ---- 2) DATA PLANE + ACK --------------------------------------------
    print("[central] waiting for a data-plane uplink ...")
    up = None
    for ftype, seq, body in nb.poll(args.data_timeout):
        if ftype != P.EVT_RX:
            continue
        rx = P.parse_evt_rx(body)
        f = rx["frame"]
        if len(f) < P.P2P_HDR_LEN or f[6] in (P.FRAME_JOIN_REQUEST, P.FRAME_JOIN_ACCEPT):
            continue
        try:
            up = (rx, P.decrypt_data_frame(sk, f, P.DIR_TX))
            break
        except ValueError:
            continue
    if not up:
        fails.append("no decryptable data-plane uplink seen")
    else:
        rx, d = up
        print(f"[central] uplink type=0x{d['frame_type']:02x} counter={d['counter']} "
              f"body={d['body'].hex()} RSSI={rx['rssi']} SNR={rx['snr']}")
        ack = P.build_ack(sk, net_id, d["dev_addr"], d["counter"],
                          rssi=rx["rssi"], snr=rx["snr"],
                          unix_time=(args.unix_time if args.unix_time else None))
        nb.tx_schedule(2, rx["t_ms"] + rx1_delay * 1000, ack)
        td = nb.wait_frame(P.EVT_TX_DONE, 5.0)
        if not td or P.parse_evt_tx_done(td[1])["status"] != 0:
            fails.append("ACK EVT_TX_DONE not OK")
        else:
            print("[central] ACK transmitted")

    # ---- 3) confirm on the DUT ------------------------------------------
    if dut:
        st = dut.cmd("ats radio status")
        print("[dut] ats radio status:\n" + st)
        if "PAIRED" not in st.upper():
            fails.append("DUT ats radio status is not PAIRED")

    print("\n=== RESULT: " + ("PASS" if not fails else "FAIL ===") )
    for f in fails:
        print("  - " + f)
    return 0 if not fails else 1


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nb-sn", type=int, required=True, help="northbridge J-Link serial")
    ap.add_argument("--dut-sn", type=int, help="STICKER DUT J-Link serial (optional)")
    ap.add_argument("--app-key", required=True, help="DUT LoRaWAN AppKey, 32 hex digits")
    ap.add_argument("--dev-eui", default="",
                    help="expected DUT DevEUI, 16 hex MSB-first (empty = don't check)")
    ap.add_argument("--rtt-addr", default="0x20000000",
                    help="northbridge _SEGGER_RTT addr (read from the bench .elf; moves with the 256 B down-buffer)")
    ap.add_argument("--dut-rtt-addr", default="0x20000000", help="STICKER DUT _SEGGER_RTT addr")
    ap.add_argument("--freq", type=int, default=868100000)
    ap.add_argument("--sf", type=int, default=10)
    ap.add_argument("--tx-power", type=int, default=14)
    ap.add_argument("--net-id", default="0xb595cf19")
    ap.add_argument("--dev-addr", default="0x0001")
    ap.add_argument("--central-nonce", default="0xdeadbeef")
    ap.add_argument("--rx1-delay", type=int, default=1)
    ap.add_argument("--unix-time", type=int, default=0, help="deliver this Unix time in the ACK (0 = none)")
    ap.add_argument("--join-timeout", type=float, default=140.0, help="DUT boot-window join is 120 s")
    ap.add_argument("--data-timeout", type=float, default=90.0)
    args = ap.parse_args()
    try:
        sys.exit(run(args))
    except KeyboardInterrupt:
        sys.exit(130)


if __name__ == "__main__":
    main()
