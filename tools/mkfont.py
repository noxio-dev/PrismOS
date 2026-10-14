#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
mkfont.py - pack a GNU Unifont .hex file into the kernel's console font.

    tools/mkfont.py unifont_all.hex plane00-combining.txt sys/kern/data/unifont.bin

Input: "XXXX:HEX" lines, 8x16 (32 hex digits) or 16x16 (64 hex digits)
glyphs, one row per byte (two for wide glyphs), leftmost pixel = MSB.

Output layout (little endian), looked up in O(1) by sys/kern/src/unifont.rs:

    "UNIF"                       magic
    u32 nblocks                  0x1100 blocks of 256 code points
    u32 comb_off, comb_count     position and size of the combining table
    u32 index[nblocks]           file offset of the block record, 0xFFFFFFFF = empty
    block record:
        u8 present[32]           bit n set: code point base+n has a glyph
        u8 wide[32]              bit n set: that glyph is 16 pixels wide
        glyphs in code point order, 16 bytes (narrow) or 32 bytes (wide)

Combining table: sorted u32 entries, code point in bits 0-23 and the pen
shift in pixels (0, 8, 12 or 16, applied leftwards) in bits 24-31.  These
characters have zero advance: they are drawn over the previous cell.  The
zero-width format characters (ZWSP, variation selectors...) are listed too.

GNU Unifont is (c) Roman Czyborra, Paul Hardy et al., dual licensed under
GPL-2.0-or-later with the font embedding exception and SIL OFL 1.1.
"""
import struct
import sys

NBLOCKS = 0x1100


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    glyphs = {}
    with open(sys.argv[1]) as f:
        for line in f:
            line = line.strip()
            if not line or ":" not in line:
                continue
            cp, data = line.split(":")
            cp = int(cp, 16)
            raw = bytes.fromhex(data)
            if len(raw) not in (16, 32) or cp >= NBLOCKS * 256:
                continue
            if 0xE000 <= cp <= 0xF8FF or cp >= 0xF0000:
                continue        # private use areas are not Unicode characters
            glyphs[cp] = raw
    blocks = {}
    for cp in sorted(glyphs):
        blocks.setdefault(cp >> 8, []).append(cp)

    comb = {}
    with open(sys.argv[2]) as f:
        for line in f:
            if ":" in line:
                cp, off = line.strip().split(":")
                comb[int(cp, 16)] = -int(off)
    # zero-width format characters: no advance, nothing to overlay
    for lo, hi in ((0x200B, 0x200F), (0x2028, 0x202E), (0x2060, 0x2064),
                   (0xFE00, 0xFE0F), (0xFEFF, 0xFEFF), (0xE0001, 0xE01EF)):
        for cp in range(lo, hi + 1):
            comb.setdefault(cp, 0)
    comb_tab = struct.pack("<%dI" % len(comb),
                           *[cp | (off << 24) for cp, off in sorted(comb.items())])

    header = 4 + 4 + 8 + NBLOCKS * 4
    index = [0xFFFFFFFF] * NBLOCKS
    body = bytearray()
    for b in sorted(blocks):
        index[b] = header + len(body)
        present = bytearray(32)
        wide = bytearray(32)
        data = bytearray()
        for cp in blocks[b]:
            n = cp & 255
            present[n >> 3] |= 1 << (n & 7)
            if len(glyphs[cp]) == 32:
                wide[n >> 3] |= 1 << (n & 7)
            data += glyphs[cp]
        body += present + wide + data
    comb_off = header + len(body)
    out = (b"UNIF" + struct.pack("<III", NBLOCKS, comb_off, len(comb))
           + struct.pack("<%dI" % NBLOCKS, *index) + bytes(body) + comb_tab)
    with open(sys.argv[3], "wb") as f:
        f.write(out)
    print("%s: %d glyphs, %d blocks, %d combining, %d KiB" % (
        sys.argv[3], len(glyphs), len(blocks), len(comb), len(out) // 1024))


if __name__ == "__main__":
    main()
