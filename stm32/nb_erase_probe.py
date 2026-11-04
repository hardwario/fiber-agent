#!/usr/bin/env python3
"""In the ROM bootloader: erase ONE page (default 119, empty on a bench NB) and report."""
import sys
sys.path.insert(0, __file__.rsplit("/", 1)[0])
from nb_uart_flash import Port, rom
p = Port("/dev/ttyAMA3"); r = rom(p)
page = int(sys.argv[1]) if len(sys.argv) > 1 else 119
r.erase_pages([page]); print(f"page {page} erased OK")
print("page 119 first word:", r.read_memory(0x0803B800, 8).hex())
