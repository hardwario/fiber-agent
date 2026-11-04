#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Turn the golden TOWER link vector file into a C header (#470 C4).

Every non-comment, non-blank line becomes one string of tower_link_vectors[]; the test
parses the tokens at run time, so the file stays the single source for both sides.

    vectors_to_header.py IN.txt OUT.h [SYMBOL]

SYMBOL (default tower_link_vectors) names the array; SYMBOL_COUNT in upper case its length.
tests/tower_gateway reads the ACK CTRL KAT (p2p_tower_ack_ctrl_kat.txt) the same way.
"""
import os
import sys


def main() -> None:
    src, out = sys.argv[1], sys.argv[2]
    sym = sys.argv[3] if len(sys.argv) > 3 else "tower_link_vectors"
    up = sym.upper()
    with open(src, encoding="utf-8") as f:
        lines = [ln.strip() for ln in f]
    vecs = [ln for ln in lines if ln and not ln.startswith("#")]
    if not vecs:
        sys.exit(f"{src}: no vectors")
    for ln in vecs:
        if not ln.isascii() or '"' in ln or "\\" in ln:
            sys.exit(f"{src}: non-ASCII, quote or backslash in a vector: {ln[:40]}")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w", encoding="ascii") as f:
        name = os.path.basename(src)
        count = "TOWER_LINK_VECTOR_COUNT" if sym == "tower_link_vectors" else f"{up}_COUNT"
        f.write(f"/* Generated from {name}; do not edit. */\n")
        f.write(f"#ifndef {up}_H_\n#define {up}_H_\n\n")
        f.write(f"#define {count} {len(vecs)}\n\n")
        f.write(f"static const char *const {sym}[{count}] = {{\n")
        for ln in vecs:
            f.write(f'\t"{ln}",\n')
        f.write("};\n\n#endif\n")


if __name__ == "__main__":
    main()
