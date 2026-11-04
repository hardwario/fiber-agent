#!/usr/bin/env python3
"""First flash of the Northbridge (STM32WL55) over its UART alone, no SWD.

The Hub has no BOOT0 or reset line to the Northbridge, and a factory or bench
Northbridge has no `nbboot` stub, so the upstream path (ENTER_BOOTLOADER over
HDLC, nbboot jumps to the ROM bootloader) is not available yet. This tool gets
there once, in steps you run one at a time:

  status          what runs on the port: Zephyr shell, ROM bootloader, or silence
  enter-rom       from a Zephyr shell with `devmem`: program FLASH_OPTR so the chip boots
                  the ROM bootloader (nSWBOOT0=0, nBOOT0=0); takes effect at the next power-on
  ob-read         (ROM bootloader) dump the option-byte area to a file
  flash HEX       (ROM bootloader) erase pages 0..119, write HEX, read every byte back
  ob-restore FILE (ROM bootloader) write the saved option bytes back (boot from flash)
  go ADDR         (ROM bootloader) jump to ADDR (0x08000000 = nbboot)

Only the standard library: the Hub has no pyserial. Stop proximos-control first,
it owns the port.
"""
import argparse, os, select, struct, sys, termios, time, re

FLASH_R = 0x58004000
KEYR, OPTKEYR, SR, CR, OPTR = FLASH_R + 0x08, FLASH_R + 0x0C, FLASH_R + 0x10, FLASH_R + 0x14, FLASH_R + 0x20
KEY1, KEY2, OPTKEY1, OPTKEY2 = 0x45670123, 0xCDEF89AB, 0x08192A3B, 0x4C5D6E7F
CR_OPTSTRT, CR_OBL_LAUNCH = 1 << 17, 1 << 27
SR_BSY = 1 << 16
SR_ERRORS = 0x0000C3FA            # OPTVERR RDERR FASTERR MISSERR PGSERR SIZERR PGAERR WRPERR PROGERR OPERR
NSWBOOT0, NBOOT0 = 1 << 26, 1 << 27
OB_AREA, OB_LEN = 0x1FFF7800, 0x80
PID_STM32WL5X = 0x497
ACK, NACK = 0x79, 0x1F
PAGE = 2048
LAST_ERASED_PAGE = 119             # pages 120.. are `storage`, never touched


class Port:
    def __init__(self, path):
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY)
        self.mode("N")

    def mode(self, parity):
        a = termios.tcgetattr(self.fd)
        a[0] = a[1] = a[3] = 0
        a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL | (termios.PARENB if parity == "E" else 0)
        a[4] = a[5] = termios.B115200
        a[6][termios.VMIN] = 0
        a[6][termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW, a)
        termios.tcflush(self.fd, termios.TCIOFLUSH)

    def write(self, b):
        os.write(self.fd, b)

    def read(self, n, timeout):
        out, end = b"", time.time() + timeout
        while len(out) < n:
            left = end - time.time()
            if left <= 0:
                break
            r, _, _ = select.select([self.fd], [], [], left)
            if r:
                out += os.read(self.fd, n - len(out))
        return out

    def drain(self, timeout):
        out, end = b"", time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if r:
                out += os.read(self.fd, 4096)
        return out


# ---------------------------------------------------------------- Zephyr shell
def shell(p, line, timeout=0.8):
    p.mode("N")
    p.write(b"\r"); p.drain(0.15)
    p.write(line.encode() + b"\r")
    txt = p.drain(timeout).decode("utf-8", "replace")
    return re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", txt)


def devmem_read(p, addr):
    txt = shell(p, f"devmem 0x{addr:08x} 32")
    m = re.search(r"Read value 0x([0-9a-fA-F]+)", txt)
    if not m:
        raise SystemExit(f"devmem read 0x{addr:08x}: no value in reply: {txt!r}")
    return int(m.group(1), 16)


def devmem_write(p, addr, value):
    txt = shell(p, f"devmem 0x{addr:08x} 32 0x{value:08x}")
    if "uart:~$" not in txt:
        raise SystemExit(f"devmem write 0x{addr:08x}: unexpected reply: {txt!r}")


def is_shell(p):
    return "uart:~$" in shell(p, "")


# ------------------------------------------------------------ ROM bootloader
def xor(data):
    c = 0
    for b in data:
        c ^= b
    return c


class Rom:
    def __init__(self, p):
        self.p = p

    def ack(self, what, timeout=1.0):
        r = self.p.read(1, timeout)
        if r != bytes([ACK]):
            raise SystemExit(f"ROM bootloader: {what}: expected ACK, got {r.hex() or 'nothing'}")

    def cmd(self, c, what):
        self.p.write(bytes([c, c ^ 0xFF]))
        self.ack(what)

    def sync(self):
        self.p.mode("E")
        for _ in range(3):
            self.p.write(b"\x7f")
            r = self.p.read(1, 0.5)
            if r in (bytes([ACK]), bytes([NACK])):   # NACK: already synced earlier
                return True
        return False

    def get_id(self):
        self.cmd(0x02, "GET_ID")
        n = self.p.read(1, 1)[0]
        pid = self.p.read(n + 1, 1)
        self.ack("GET_ID end")
        return int.from_bytes(pid, "big")

    def addr(self, a):
        b = struct.pack(">I", a)
        self.p.write(b + bytes([xor(b)]))

    def read_memory(self, a, n):
        out = b""
        while n:
            k = min(n, 256)
            self.cmd(0x11, "READ_MEMORY")
            self.addr(a); self.ack(f"READ_MEMORY address 0x{a:08x}")
            self.p.write(bytes([k - 1, (k - 1) ^ 0xFF])); self.ack("READ_MEMORY length")
            chunk = self.p.read(k, 2)
            if len(chunk) != k:
                raise SystemExit(f"READ_MEMORY 0x{a:08x}: {len(chunk)}/{k} bytes")
            out += chunk; a += k; n -= k
        return out

    def write_memory(self, a, data, timeout=1.0):
        self.cmd(0x31, "WRITE_MEMORY")
        self.addr(a); self.ack(f"WRITE_MEMORY address 0x{a:08x}")
        frame = bytes([len(data) - 1]) + data
        self.p.write(frame + bytes([xor(frame)]))
        self.ack(f"WRITE_MEMORY data 0x{a:08x}", timeout)

    def erase_pages(self, pages):
        self.cmd(0x44, "EXTENDED_ERASE")
        body = struct.pack(">H", len(pages) - 1) + b"".join(struct.pack(">H", x) for x in pages)
        self.p.write(body + bytes([xor(body)]))
        self.ack("EXTENDED_ERASE done", 2 + 0.1 * len(pages))

    def go(self, a):
        self.cmd(0x21, "GO")
        self.addr(a); self.ack(f"GO 0x{a:08x}")


def rom(p):
    r = Rom(p)
    if not r.sync():
        raise SystemExit("no ROM bootloader on the port (no ACK to 0x7F at 8E1)")
    pid = r.get_id()
    if pid != PID_STM32WL5X:
        raise SystemExit(f"GET_ID 0x{pid:03x}, expected 0x{PID_STM32WL5X:03x} (STM32WL5x): wrong chip, stopping")
    return r


# ----------------------------------------------------------------- Intel hex
def load_hex(path):
    mem, base = {}, 0
    for ln in open(path):
        ln = ln.strip()
        if not ln.startswith(":"):
            continue
        raw = bytes.fromhex(ln[1:])
        if sum(raw) & 0xFF:
            raise SystemExit(f"{path}: bad checksum in {ln}")
        n, off, typ = raw[0], (raw[1] << 8) | raw[2], raw[3]
        data = raw[4:4 + n]
        if typ == 0:
            for i, b in enumerate(data):
                mem[base + off + i] = b
        elif typ == 4:
            base = int.from_bytes(data, "big") << 16
        elif typ == 2:
            base = int.from_bytes(data, "big") << 4
    return mem


def blocks(mem, size=256):
    """(address, bytes) blocks, 8-byte aligned and padded with 0xFF (double-word programming)."""
    out, addrs = [], sorted(mem)
    i = 0
    while i < len(addrs):
        start = addrs[i] & ~7
        buf = bytearray(b"\xff" * size)
        end = start + size
        while i < len(addrs) and addrs[i] < end:
            buf[addrs[i] - start] = mem[addrs[i]]
            i += 1
        last = max(a for a in range(start, end) if mem.get(a) is not None) - start + 1
        last = (last + 7) & ~7
        out.append((start, bytes(buf[:last])))
    return out


# ------------------------------------------------------------------- commands
def c_status(p, a):
    if is_shell(p):
        print("status: Zephyr shell (application firmware, no ROM bootloader)")
        try:
            print(f"  OPTR = 0x{devmem_read(p, OPTR):08x}   reset vector = 0x{devmem_read(p, 0x08000004):08x}")
        except SystemExit as e:
            print(f"  {e}")
        return
    r = Rom(p)
    if r.sync():
        print(f"status: ROM bootloader, GET_ID 0x{r.get_id():03x}")
    else:
        p.mode("N")
        print("status: no shell and no ROM bootloader (HDLC app, or nothing)")


def c_enter_rom(p, a):
    if not is_shell(p):
        raise SystemExit("enter-rom needs the Zephyr shell on the port; run `status`")
    optr = devmem_read(p, OPTR)
    print(f"OPTR now 0x{optr:08x}")
    if (optr & 0xFF) != 0xAA:
        raise SystemExit("RDP is not level 0 (0xAA): stopping, the ROM bootloader would refuse to write")
    if optr != a.expect_optr:
        raise SystemExit(f"OPTR is not the expected 0x{a.expect_optr:08x}: stopping (pass --expect-optr after checking)")
    new = optr & ~(NSWBOOT0 | NBOOT0)
    print(f"OPTR new 0x{new:08x} (nSWBOOT0=0, nBOOT0=0: boot the ROM bootloader)")
    if a.dry_run:
        print("dry run: nothing written"); return
    sr = devmem_read(p, SR)
    if sr & SR_BSY:
        raise SystemExit(f"FLASH_SR busy (0x{sr:08x})")
    devmem_write(p, SR, sr & SR_ERRORS)               # clear stale error flags (write 1 to clear)
    devmem_write(p, KEYR, KEY1); devmem_write(p, KEYR, KEY2)
    devmem_write(p, OPTKEYR, OPTKEY1); devmem_write(p, OPTKEYR, OPTKEY2)
    cr = devmem_read(p, CR)
    if cr & (1 << 30 | 1 << 31):
        raise SystemExit(f"FLASH_CR still locked after the keys (0x{cr:08x}): stopping, nothing changed")
    devmem_write(p, OPTR, new)
    # On the STM32WL the register keeps reading the loaded value until the next
    # power-on reloads the options (bench 2026-11-04): no read-back check here.
    devmem_write(p, CR, CR_OPTSTRT)
    for _ in range(50):
        sr = devmem_read(p, SR)
        if not sr & SR_BSY:
            break
    if sr & SR_ERRORS:
        raise SystemExit(f"option programming error, FLASH_SR 0x{sr:08x}: not launching")
    print(f"options programmed (SR 0x{sr:08x}).")
    print("Now POWER-CYCLE the board (12 V and battery off). It comes back in the ROM")
    print("bootloader; keep proximos-control stopped, then run `flash` and `ob-restore`.")


def c_ob_read(p, a):
    r = rom(p)
    data = r.read_memory(OB_AREA, OB_LEN)
    open(a.out, "wb").write(data)
    for off in range(0, OB_LEN, 8):
        lo, hi = struct.unpack_from("<II", data, off)
        print(f"0x{OB_AREA + off:08x}: {lo:08x} {hi:08x}" + ("  (hi = ~lo)" if hi == (~lo & 0xFFFFFFFF) else ""))
    print(f"saved {a.out}")


def c_flash(p, a):
    mem = load_hex(a.hex)
    lo, hi = min(mem), max(mem)
    if lo < 0x08000000 or hi >= 0x08000000 + (LAST_ERASED_PAGE + 1) * PAGE:
        raise SystemExit(f"{a.hex} spans 0x{lo:08x}..0x{hi:08x}, outside pages 0..{LAST_ERASED_PAGE}")
    r = rom(p)
    print(f"image 0x{lo:08x}..0x{hi:08x}, {len(mem)} bytes")
    pages = list(range(0, LAST_ERASED_PAGE + 1))
    t = time.time(); r.erase_pages(pages); print(f"erased pages 0..{LAST_ERASED_PAGE} in {time.time() - t:.1f}s")
    bl = blocks(mem)
    t = time.time()
    for addr, data in bl:
        r.write_memory(addr, data)
    print(f"wrote {len(bl)} blocks in {time.time() - t:.1f}s")
    t = time.time(); bad = 0
    for addr, data in bl:
        back = r.read_memory(addr, len(data))
        if back != data:
            bad += 1
            print(f"MISMATCH at 0x{addr:08x}")
    print(f"read back in {time.time() - t:.1f}s: {'OK' if not bad else f'{bad} blocks differ'}")
    if bad:
        raise SystemExit(1)


def c_ob_restore(p, a):
    saved = open(a.file, "rb").read()
    if len(saved) != OB_LEN:
        raise SystemExit(f"{a.file}: {len(saved)} bytes, expected {OB_LEN}")
    lo, hi = struct.unpack_from("<II", saved, 0)
    new = lo | NSWBOOT0 | NBOOT0
    if a.optr is not None:
        new = a.optr
    if (new & 0xFF) != 0xAA:
        raise SystemExit("refusing an OPTR whose RDP is not 0xAA")
    image = bytearray(saved)
    struct.pack_into("<I", image, 0, new)
    if hi == (~lo & 0xFFFFFFFF):
        struct.pack_into("<I", image, 4, ~new & 0xFFFFFFFF)
    print(f"OPTR 0x{lo:08x} -> 0x{new:08x}")
    if a.dry_run:
        print("dry run: nothing written"); return
    r = rom(p)
    r.write_memory(OB_AREA, bytes(image[: a.length]), timeout=3)
    print("option bytes written; the chip reloads them and resets")


def c_go(p, a):
    rom(p).go(int(a.addr, 0)); print(f"jumped to {a.addr}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", default="/dev/ttyAMA3")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("status")
    s = sub.add_parser("enter-rom"); s.add_argument("--expect-optr", type=lambda x: int(x, 0), default=0x3FFFF0AA); s.add_argument("--dry-run", action="store_true")
    s = sub.add_parser("ob-read"); s.add_argument("--out", default="nb-option-bytes.bin")
    s = sub.add_parser("flash"); s.add_argument("hex")
    s = sub.add_parser("ob-restore"); s.add_argument("file"); s.add_argument("--optr", type=lambda x: int(x, 0)); s.add_argument("--length", type=int, default=8); s.add_argument("--dry-run", action="store_true")
    s = sub.add_parser("go"); s.add_argument("addr")
    a = ap.parse_args()
    p = Port(a.port)
    {"status": c_status, "enter-rom": c_enter_rom, "ob-read": c_ob_read, "flash": c_flash,
     "ob-restore": c_ob_restore, "go": c_go}[a.cmd](p, a)


if __name__ == "__main__":
    main()
