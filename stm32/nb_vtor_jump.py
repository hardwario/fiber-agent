#!/usr/bin/env python3
"""Send a running Zephyr Northbridge into its ROM bootloader without touching
flash or option bytes: build a vector table in SRAM whose every entry is the
ROM bootloader's reset handler, point VTOR at it, and let the next interrupt
jump there. Needs the app's shell with `devmem`. Worst case: the chip hangs
until its next power cycle; nothing is written to flash."""
import sys, time
sys.path.insert(0, __file__.rsplit("/", 1)[0])
from nb_uart_flash import Port, devmem_read, shell, Rom, is_shell

TABLE = 0x2000FE00           # 512-byte aligned, top of SRAM, above the ROM bootloader's RAM
ENTRIES = 128                # 16 core + 62 IRQs, rounded up to the alignment
VTOR = 0xE000ED08

p = Port(sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyAMA3")
if not is_shell(p):
    raise SystemExit("no Zephyr shell on the port")
sp, reset = devmem_read(p, 0x1FFF0000), devmem_read(p, 0x1FFF0004)
if not (0x1FFF0000 <= (reset & ~1) < 0x1FFF8000) or not reset & 1:
    raise SystemExit(f"ROM reset vector 0x{reset:08x} does not look like system memory")
print(f"ROM bootloader: SP 0x{sp:08x}, reset 0x{reset:08x}")
for i in range(ENTRIES):
    shell(p, f"devmem 0x{TABLE + 4 * i:08x} 32 0x{(sp if i == 0 else reset):08x}", 0.15)
for i in (0, 1, 15, ENTRIES - 1):
    v = devmem_read(p, TABLE + 4 * i)
    want = sp if i == 0 else reset
    if v != want:
        raise SystemExit(f"table entry {i} reads 0x{v:08x}, want 0x{want:08x}: not switching VTOR")
print(f"table at 0x{TABLE:08x} verified")
quiet = "--quiet-irqs" in sys.argv
if quiet:
    # No interrupt may re-enter the table once the bootloader runs: switch VTOR
    # with every NVIC line disabled and SysTick off, then pend one PendSV, which
    # is the only exception left to take the jump.
    print("VTOR, then NVIC off, SysTick off, PendSV")
    shell(p, f"devmem 0x{VTOR:08x} 32 0x{TABLE:08x}", 0.15)   # harmless until an exception fires
    p.mode("N")
    cmds = [f"devmem 0x{0xE000E180 + 4 * i:08x} 32 0xffffffff" for i in range(3)]
    cmds += ["devmem 0xe000e010 32 0x0", "devmem 0xe000ed04 32 0x10000000"]
    p.write(("\r".join(cmds) + "\r").encode())
else:
    print("switching VTOR")
    p.mode("N"); p.write(f"devmem 0x{VTOR:08x} 32 0x{TABLE:08x}\r".encode())
time.sleep(0.5)
r = Rom(p)
if r.sync():
    print(f"ROM bootloader is up: GET_ID 0x{r.get_id():03x}")
else:
    raise SystemExit("no ROM bootloader answer after the VTOR switch")
