#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Generates VALID test artifacts for the PrismOS installer:
boot0.bin (512 B), boot1.bin (8 KiB, magic "PRB1"), loader.bin,
a MINIMAL ESP tree (esp/: only EFI/BOOT/BOOTX64.EFI) and the ext4 root
tree (root/: boot/kernel/kernel, boot/lua, boot/loader.conf, bin, etc,
usr) - the same two trees "make esp" and "make root" produce.

Everything is deterministic (seeded PRNG): the CRCs are stable from
one run to the next.  A manifest.json allows after-the-fact
verification.
"""

import argparse
import json
import os
import random
import struct
import zlib

BOOT1_MAGIC = 0x31425250          # "PRB1"
BOOT1_SECTORS = 16


def rng(seed):
    return random.Random(seed)


def make_boot0(seed):
    r = rng(seed)
    data = bytearray(r.randbytes(512))
    data[0x1FE:0x200] = b"\x55\xAA"
    return bytes(data)


def make_boot1(seed):
    r = rng(seed)
    data = bytearray(r.randbytes(BOOT1_SECTORS * 512))
    struct.pack_into("<I", data, 0, BOOT1_MAGIC)
    return bytes(data)


def make_loader(seed, size):
    r = rng(seed)
    return r.randbytes(size)


def make_efi_app(seed, text_kib=96):
    """Synthetic but structurally valid x86-64 EFI application
    (PE32+, subsystem 10) - same shape as build/loader.efi produced by
    lld-link in the upstream Makefile.  The .text content is random:
    never executed, only the header matters for the tests (the
    installer must accept THIS file and reject everything else)."""
    r = rng(seed)
    code = r.randbytes(text_kib * 1024)

    # DOS header: "MZ" + e_lfanew = 0x80.
    dos = bytearray(64)
    dos[0:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, 0x80)

    # COFF header: AMD64, 1 section, PE32+ optional header (240 B).
    coff = struct.pack("<HHIIIHH",
                       0x8664,     # Machine = IMAGE_FILE_MACHINE_AMD64
                       1,          # NumberOfSections
                       0, 0, 0,    # TimeDateStamp, PtrSymTab, NbSyms
                       240,        # SizeOfOptionalHeader
                       0x0002)     # EXECUTABLE_IMAGE

    # PE32+ optional header (relevant fields only).
    opt = bytearray(240)
    struct.pack_into("<H", opt, 0, 0x20B)        # Magic PE32+
    struct.pack_into("<I", opt, 4, len(code))    # SizeOfCode
    struct.pack_into("<I", opt, 16, 0x1000)      # AddressOfEntryPoint
    struct.pack_into("<I", opt, 20, 0x1000)      # BaseOfCode
    struct.pack_into("<Q", opt, 24, 0x00400000)  # ImageBase
    struct.pack_into("<I", opt, 32, 0x1000)      # SectionAlignment
    struct.pack_into("<I", opt, 36, 512)         # FileAlignment
    struct.pack_into("<H", opt, 48, 1)           # MajorSubsystemVersion
    struct.pack_into("<I", opt, 56, 0x2000)      # SizeOfImage
    struct.pack_into("<I", opt, 60, 512)         # SizeOfHeaders
    struct.pack_into("<H", opt, 68, 10)          # Subsystem = EFI app
    struct.pack_into("<Q", opt, 72, 0x10000)     # SizeOfStackReserve
    struct.pack_into("<Q", opt, 80, 0x10000)     # SizeOfStackCommit
    struct.pack_into("<Q", opt, 88, 0x10000)     # SizeOfHeapReserve
    struct.pack_into("<Q", opt, 96, 0x10000)     # SizeOfHeapCommit
    struct.pack_into("<I", opt, 108, 16)         # NumberOfRvaAndSizes

    # .text section: code | execute | read.
    sect = struct.pack("<8sIIIIIIHHI",
                       b".text\0\0\0",
                       len(code), 0x1000, len(code), 512, 0, 0, 0, 0,
                       0x60000020)

    headers = (bytes(dos) + b"\0" * (0x80 - 64)
               + b"PE\0\0" + coff + bytes(opt) + sect).ljust(512, b"\0")
    return headers + code


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-o", "--output", default="build/testsrc")
    ap.add_argument("--kernel-kib", type=int, default=3072,
                    help="size of the fake kernel in KiB")
    args = ap.parse_args()

    root = args.output
    boot0 = make_boot0(1001)
    boot1 = make_boot1(1002)
    loader = make_loader(1003, 700 * 1024)
    kernel = make_loader(1004, args.kernel_kib * 1024)
    efi_app = make_efi_app(1006)

    write(f"{root}/boot0.bin", boot0)
    write(f"{root}/boot1.bin", boot1)
    write(f"{root}/loader.bin", loader)

    # P1 tree identical to the one produced by "make esp": ONLY the UEFI
    # stub (BOOTX64.EFI is a REAL x86-64 EFI application: the installer
    # must validate it - random bytes there would be rejected by the
    # PE/COFF check).
    write(f"{root}/esp/EFI/BOOT/BOOTX64.EFI", efi_app)

    # P3 tree identical to the one produced by "make root":
    # boot/kernel/kernel, boot/lua/*.lua, boot/defaults/loader.conf,
    # boot/loader.conf, plus the (empty) bin, etc and usr directories.
    write(f"{root}/root/boot/kernel/kernel", kernel)

    r = rng(1005)
    lua_files = {
        "loader.lua": b"-- loader.lua (test)\nprint('prism loader')\n"
                      + r.randbytes(2048),
        "menu.lua": b"-- menu.lua (test)\n" + r.randbytes(1536),
        "cli.lua": b"-- cli.lua (test)\n" + r.randbytes(1024),
        "config.lua": b"-- config.lua (test)\n" + r.randbytes(512),
    }
    for name, content in lua_files.items():
        write(f"{root}/root/boot/lua/{name}", content)

    write(f"{root}/root/boot/defaults/loader.conf",
          b'autoboot_delay="3"\nloader_menu_title="PrismOS (test)"\n')
    write(f"{root}/root/boot/loader.conf",
          b'kernel="/boot/kernel/kernel"\nkernel_options=""\n')
    for d in ("bin", "etc", "usr"):
        os.makedirs(f"{root}/root/{d}", exist_ok=True)

    # Manifest: CRC and size of each file.
    manifest = {}
    for dirpath, _, files in os.walk(root):
        for name in sorted(files):
            if name == "manifest.json":
                continue
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root)
            with open(full, "rb") as f:
                data = f.read()
            manifest[rel] = {
                "size": len(data),
                "crc32": zlib.crc32(data) & 0xFFFFFFFF,
            }
    with open(f"{root}/manifest.json", "w") as f:
        json.dump(manifest, f, indent=1, sort_keys=True)

    print(f"artifacts generated in {root}/ "
          f"({len(manifest)} files, kernel {args.kernel_kib} KiB)")


if __name__ == "__main__":
    main()
