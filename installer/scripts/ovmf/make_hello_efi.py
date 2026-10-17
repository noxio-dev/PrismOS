#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""Generates a REAL, minimal, working x86-64 EFI application.

Unlike tests/make_test_artifacts.py::make_efi_app (random bytes in
.text, structurally valid but never executed), this generator
produces REAL MACHINE CODE:

    entry(ImageHandle RCX, SystemTable RDX):
        sub  rsp, 0x28                  ; shadow space + ABI alignment
        mov  rcx, [rdx+0x40]            ; rcx = SystemTable->ConOut
        lea  rdx, [rip+msg]             ; rdx = UTF-16 string
        call [rcx+8]                    ; ConOut->OutputString(ConOut, msg)
        jmp  $                          ; hangs (observable)

If a UEFI firmware executes this file, the marker is displayed on
screen and the machine hangs: end-to-end proof that GPT -> ESP ->
FAT32 -> EFI/BOOT/BOOTX64.EFI -> PE loader all worked.

Used by the QEMU+OVMF test bench (scripts/ovmf/).
"""

import struct
import sys

# offset 0 : 48 83 EC 28           sub  rsp, 0x28
# offset 4 : 48 8B 4A 40           mov  rcx, [rdx+0x40]   ; ConOut
# offset 8 : 48 8D 15 05 00 00 00  lea  rdx, [rip+5]      ; msg @20
# offset 15: FF 51 08              call [rcx+8]           ; OutputString
# offset 18: EB FE                 jmp  $
# offset 20: message UTF-16LE
def build(text):
    msg = text.encode("utf-16-le") + b"\x00\x00"
    code = bytearray()
    code += bytes.fromhex("48 83 EC 28".replace(" ", ""))
    code += bytes.fromhex("48 8B 4A 40".replace(" ", ""))
    code += bytes.fromhex("48 8D 15 05 00 00 00".replace(" ", ""))
    code += bytes.fromhex("FF 51 08".replace(" ", ""))
    code += bytes.fromhex("EB FE".replace(" ", ""))
    assert len(code) == 20
    code += msg
    return bytes(code)


def make_efi(text, image_base=0x00400000):
    code = build(text)
    raw_size = (len(code) + 511) // 512 * 512

    dos = bytearray(64)
    dos[0:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, 0x80)

    coff = struct.pack("<HHIIIHH",
                       0x8664,          # Machine = AMD64
                       1,               # NumberOfSections
                       0, 0, 0,         # TimeDateStamp, PtrSymTab, NbSyms
                       240,             # SizeOfOptionalHeader
                       0x0002)          # EXECUTABLE_IMAGE

    opt = bytearray(240)
    struct.pack_into("<H", opt, 0, 0x20B)           # Magic PE32+
    struct.pack_into("<I", opt, 4, raw_size)        # SizeOfCode
    struct.pack_into("<I", opt, 16, 0x1000)         # AddressOfEntryPoint
    struct.pack_into("<I", opt, 20, 0x1000)         # BaseOfCode
    struct.pack_into("<Q", opt, 24, image_base)     # ImageBase
    struct.pack_into("<I", opt, 32, 0x1000)         # SectionAlignment
    struct.pack_into("<I", opt, 36, 512)            # FileAlignment
    struct.pack_into("<H", opt, 48, 1)              # MajorSubsystemVersion
    struct.pack_into("<I", opt, 56, 0x2000)         # SizeOfImage
    struct.pack_into("<I", opt, 60, 512)            # SizeOfHeaders
    struct.pack_into("<H", opt, 68, 10)             # Subsystem = EFI app
    struct.pack_into("<Q", opt, 72, 0x10000)        # SizeOfStackReserve
    struct.pack_into("<Q", opt, 80, 0x10000)        # SizeOfStackCommit
    struct.pack_into("<Q", opt, 88, 0x10000)        # SizeOfHeapReserve
    struct.pack_into("<Q", opt, 96, 0x10000)        # SizeOfHeapCommit
    struct.pack_into("<I", opt, 108, 16)            # NumberOfRvaAndSizes

    sect = struct.pack("<8sIIIIIIHHI",
                       b".text\0\0\0",
                       len(code), 0x1000, raw_size, 512, 0, 0, 0, 0,
                       0x60000020)                  # CODE|EXECUTE|READ

    headers = (bytes(dos) + b"\0" * (0x80 - 64)
               + b"PE\0\0" + coff + bytes(opt) + sect).ljust(512, b"\0")
    return headers + code.ljust(raw_size, b"\x90")


if __name__ == "__main__":
    text = sys.argv[2] if len(sys.argv) > 2 else "PRISM ESP BOOT OK"
    out = sys.argv[1] if len(sys.argv) > 1 else "hello.efi"
    data = make_efi(text)
    with open(out, "wb") as f:
        f.write(data)
    print(f"{out}: {len(data)} bytes, message \"{text}\"")
