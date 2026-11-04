#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
Flash the fiber southbridge (STM32C031C6) via the system bootloader over
USART1. Uses libgpiod's gpioset(1) for BOOT/RESET sequencing and pyserial
for AN3155 protocol. Both deps already ship in fiber-image-minimal.

Wiring on the RPi CM4 (BCM numbering):
  GPIO7  → MCU NRST   (MCU_RESET_EXT)
  GPIO26 → MCU BOOT0  (MCU_BOOT_EXT)
  /dev/ttyAMA4 ↔ USART1 (PA9/PA10)

Usage on the CM4:
  sudo ./flash_southbridge.py /tmp/zephyr.bin
  sudo ./flash_southbridge.py --port /dev/ttyAMA4 /tmp/zephyr.bin
  sudo ./flash_southbridge.py --no-run /tmp/zephyr.bin   # stay in bootloader

Assumes the STM32C031 option byte NBOOT_SEL=0 (BOOT0 pin selects boot mode).
If the chip is still factory-default (NBOOT_SEL=1) the handshake at 0x7F will
time out — in that case only SWD can recover.
"""

import argparse
import subprocess
import sys
import time
from contextlib import contextmanager

import serial

# -- AN3155 ------------------------------------------------------------------
INIT_BYTE      = 0x7F
ACK            = 0x79
NACK           = 0x1F
CMD_GET_ID     = 0x02
CMD_WRITE      = 0x31
CMD_ERASE_EXT  = 0x44
CMD_GO         = 0x21

FLASH_BASE     = 0x08000000
STM32C0_CHIP_ID = 0x453

# -- RPi CM4 BCM pin numbers -------------------------------------------------
GPIO_CHIP  = "gpiochip0"
GPIO_BOOT  = 26
GPIO_RESET = 7


# ---------------------------------------------------------------------------
# GPIO helpers via gpioset(1). libgpiod v2 (Yocto scarthgap) is the assumed
# default; falls back to v1 syntax if v2 args fail.
# ---------------------------------------------------------------------------

class GpioHolder:
    """Hold a GPIO line at `value` until release()."""

    def __init__(self, pin: int, value: int) -> None:
        self.pin = pin
        self._proc: subprocess.Popen | None = None
        self.set(value)

    def set(self, value: int) -> None:
        self.release()
        # libgpiod v2: gpioset holds the line until the process exits.
        # No need for --hold-period — terminate() releases the line.
        cmd = ["gpioset", "-c", GPIO_CHIP, f"{self.pin}={value}"]
        try:
            proc = subprocess.Popen(
                cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
        except FileNotFoundError:
            raise RuntimeError("gpioset not found — install libgpiod-tools")
        time.sleep(0.03)
        if proc.poll() is not None:
            err = proc.stderr.read().decode(errors="replace") if proc.stderr else ""
            raise RuntimeError(f"gpioset failed: {err}")
        self._proc = proc

    def release(self) -> None:
        if self._proc and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        self._proc = None


@contextmanager
def boot_into_bootloader():
    """Hold BOOT high, pulse RESET, leave BOOT held for the caller."""
    boot = GpioHolder(GPIO_BOOT, 1)
    rst = GpioHolder(GPIO_RESET, 0)
    try:
        time.sleep(0.05)            # NRST low long enough to reset
        rst.set(1)                  # release NRST, BOOT still high
        time.sleep(0.10)            # bootloader sample + USART setup
        yield
    finally:
        rst.release()
        boot.release()


def reset_to_app() -> None:
    """BOOT low, pulse RESET, then release both. Lets app run normally."""
    boot = GpioHolder(GPIO_BOOT, 0)
    rst = GpioHolder(GPIO_RESET, 0)
    time.sleep(0.05)
    rst.set(1)
    time.sleep(0.02)
    rst.release()
    boot.release()


# ---------------------------------------------------------------------------
# AN3155 bootloader protocol
# ---------------------------------------------------------------------------

def _wait_ack(ser: serial.Serial, what: str) -> None:
    b = ser.read(1)
    if not b:
        raise RuntimeError(f"{what}: timeout (no reply)")
    if b[0] == ACK:
        return
    if b[0] == NACK:
        raise RuntimeError(f"{what}: NACK")
    raise RuntimeError(f"{what}: unexpected reply 0x{b[0]:02X}")


def _cmd(ser: serial.Serial, c: int) -> None:
    ser.write(bytes([c, c ^ 0xFF]))
    _wait_ack(ser, f"cmd 0x{c:02X}")


def _send_addr(ser: serial.Serial, addr: int) -> None:
    a = addr.to_bytes(4, "big")
    chk = a[0] ^ a[1] ^ a[2] ^ a[3]
    ser.write(a + bytes([chk]))
    _wait_ack(ser, f"addr 0x{addr:08X}")


def handshake(ser: serial.Serial, retries: int = 10) -> None:
    """Send 0x7F until bootloader replies with ACK (sets baud)."""
    for i in range(retries):
        ser.reset_input_buffer()
        ser.write(bytes([INIT_BYTE]))
        b = ser.read(1)
        if b and b[0] == ACK:
            print(f"[bl] handshake OK after {i + 1} try", file=sys.stderr)
            return
        if b and b[0] == NACK:
            # Already initialized in this session — also OK
            print("[bl] already initialised", file=sys.stderr)
            return
        time.sleep(0.1)
    raise RuntimeError("0x7F handshake failed — bootloader not running. "
                       "Check NBOOT_SEL option byte and wiring.")


def get_chip_id(ser: serial.Serial) -> int:
    _cmd(ser, CMD_GET_ID)
    n = ser.read(1)
    if not n:
        raise RuntimeError("Get ID: no length")
    data = ser.read(n[0] + 1)
    _wait_ack(ser, "Get ID end")
    return int.from_bytes(data, "big")


def mass_erase(ser: serial.Serial) -> None:
    """Extended erase, mass-erase magic (0xFFFF)."""
    print("[bl] mass erase...", file=sys.stderr)
    _cmd(ser, CMD_ERASE_EXT)
    payload = bytes([0xFF, 0xFF])
    chk = payload[0] ^ payload[1]
    ser.write(payload + bytes([chk]))
    old_to = ser.timeout
    ser.timeout = 30
    try:
        _wait_ack(ser, "mass erase complete")
    finally:
        ser.timeout = old_to


def write_memory(ser: serial.Serial, addr: int, data: bytes) -> None:
    if not 1 <= len(data) <= 256:
        raise ValueError("chunk must be 1..256 bytes")
    _cmd(ser, CMD_WRITE)
    _send_addr(ser, addr)
    n_m1 = len(data) - 1
    chk = n_m1
    for b in data:
        chk ^= b
    ser.write(bytes([n_m1]) + data + bytes([chk & 0xFF]))
    _wait_ack(ser, f"write @ 0x{addr:08X}")


def flash_file(ser: serial.Serial, payload: bytes) -> None:
    # STM32C0 flash row = 8 bytes; pad with 0xFF.
    pad = (-len(payload)) & 7
    if pad:
        payload = payload + b"\xFF" * pad

    total = len(payload)
    chunk = 256
    print(f"[bl] writing {total} bytes...", file=sys.stderr)
    for off in range(0, total, chunk):
        block = payload[off:off + chunk]
        write_memory(ser, FLASH_BASE + off, block)
        pct = (off + len(block)) * 100 // total
        sys.stderr.write(f"\r[bl] {off + len(block)}/{total} ({pct}%)")
        sys.stderr.flush()
    sys.stderr.write("\n")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("bin_file", help="path to firmware .bin (raw flash image)")
    ap.add_argument("--port", default="/dev/ttyAMA4",
                    help="UART device (default: /dev/ttyAMA4)")
    ap.add_argument("--no-run", action="store_true",
                    help="leave MCU in bootloader after flashing")
    ap.add_argument("--no-erase", action="store_true",
                    help="skip mass erase (use only if you know what you're doing)")
    args = ap.parse_args()

    with open(args.bin_file, "rb") as f:
        payload = f.read()
    print(f"[in] {args.bin_file}: {len(payload)} bytes", file=sys.stderr)

    with boot_into_bootloader():
        # Bootloader uses 8E1 (even parity), 115200 baud (auto-detected from
        # the 0x7F init byte).
        ser = serial.Serial(args.port, baudrate=115200,
                            bytesize=8, parity=serial.PARITY_EVEN,
                            stopbits=1, timeout=2)
        try:
            handshake(ser)
            chip_id = get_chip_id(ser)
            print(f"[bl] chip ID = 0x{chip_id:03X}", file=sys.stderr)
            if chip_id != STM32C0_CHIP_ID:
                print(f"[bl] WARN: expected 0x{STM32C0_CHIP_ID:03X} "
                      f"(STM32C0)", file=sys.stderr)
            if not args.no_erase:
                mass_erase(ser)
            flash_file(ser, payload)
        finally:
            ser.close()

    if args.no_run:
        print("[done] flashed; MCU left in bootloader", file=sys.stderr)
    else:
        reset_to_app()
        print("[done] flashed; firmware started", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
