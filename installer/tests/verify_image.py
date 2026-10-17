#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
INDEPENDENT verifier for the image installed by prism-installer.

Shares no code with the C++ installer: it re-interprets everything
from the raw disk (MBR, GPT, BIOS boot partition, the ESP's FAT32
volume, ext4 superblock) and compares against the source artifacts
and the manifest.

Usage:
    verify_image.py IMAGE SOURCE_DIR [--esp-size MIB] [--reserve MIB]
                    [--stub-log LOG] [--label-ext4 L]
"""

import argparse
import binascii
import json
import os
import struct
import sys
import zlib

SECTOR = 512
MIB = 1024 * 1024

ESP_START = 2048                     # P1: the ESP is the FIRST
                                     # partition (OEM firmware quirk)
BOOT_PART_SECTORS = 4096             # 2 MiB
BOOT1_SECTORS = 16
BOOT0_LBA_OFF = 0x1B0
BOOT1_MAGIC = 0x31425250
MBR_DISK_SIG = 0x5052534D

GUID_BIOS_BOOT = "21686148-6449-6e6f-744e-656564454649"
GUID_ESP = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
GUID_LINUX_FS = "0fc63daf-8483-4772-8e79-3d69d8477de4"

problems = []
notes = []


def problem(msg):
    problems.append(msg)


def note(msg):
    notes.append(msg)


def crc32(data):
    return zlib.crc32(data) & 0xFFFFFFFF


def lfn_checksum(sn):
    """LFN checksum of an 8.3 name (Microsoft algorithm, implemented
    independently of the C++ writer)."""
    s = 0
    for c in sn:
        s = (((s & 1) << 7) + (s >> 1) + c) & 0xFF
    return s


def check_pe_efi(data):
    """Validates a BOOTX64.EFI: PE32+ / x86-64 / EFI application.
    Returns a list of problems (empty = compliant)."""
    errs = []
    if len(data) < 0x40 or data[:2] != b"MZ":
        return ["DOS \"MZ\" header missing - not a PE/COFF executable"]
    off = struct.unpack_from("<I", data, 0x3C)[0]
    if off < 0x40 or off + 24 > len(data):
        return [f"e_lfanew out of bounds ({off})"]
    if data[off:off + 4] != b"PE\x00\x00":
        return ["PE signature missing"]
    machine, nsec, _, _, _, szopt, _ = struct.unpack_from("<HHIIIHH", data, off + 4)
    if off + 24 + szopt > len(data) or szopt < 70:
        return ["optional header truncated"]
    magic = struct.unpack_from("<H", data, off + 24)[0]
    entry_rva = struct.unpack_from("<I", data, off + 24 + 16)[0]
    subsystem = struct.unpack_from("<H", data, off + 24 + 68)[0]
    if machine != 0x8664:
        errs.append(f"machine 0x{machine:04X} != x86-64 (0x8664)")
    if magic != 0x20B:
        errs.append(f"PE32 header (0x{magic:04X}) instead of PE32+ (0x20B)")
    if subsystem != 10:
        errs.append(f"subsystem {subsystem} != 10 (EFI application)")
    if entry_rva == 0:
        errs.append("null entry point")
    if not 0 < nsec <= 96 or off + 24 + szopt + nsec * 40 > len(data):
        errs.append("inconsistent section table")
    return errs


def guid_str(raw16):
    """RFC 4122 representation from the GPT little-endian form."""
    d1, d2, d3 = struct.unpack_from("<IHH", raw16, 0)
    d4 = raw16[8:16]
    return (f"{d1:08x}-{d2:04x}-{d3:04x}-"
            + d4[:2].hex() + "-" + d4[2:].hex())


class Image:
    def __init__(self, path):
        self.f = open(path, "rb")

    def read(self, offset, size):
        self.f.seek(offset)
        return self.f.read(size)

    def size(self):
        self.f.seek(0, 2)
        return self.f.tell()


def check_mbr(img, source, boot_start):
    mbr = img.read(0, 512)
    boot0 = open(os.path.join(source, "boot0.bin"), "rb").read()

    if mbr[510:512] != b"\x55\xAA":
        problem("MBR: 0x55AA signature missing")
    if mbr[0x1C2] != 0xEE:
        problem("MBR: protective 0xEE entry missing")
    if struct.unpack_from("<I", mbr, 0x1B8)[0] != MBR_DISK_SIG:
        problem("MBR: incorrect disk signature")

    lba = struct.unpack_from("<Q", mbr, BOOT0_LBA_OFF)[0]
    if lba != boot_start:
        problem(f"boot0: patched LBA = {lba}, expected {boot_start}")
    if mbr[:BOOT0_LBA_OFF] != boot0[:BOOT0_LBA_OFF]:
        problem("boot0: code differs from boot0.bin")
    note("protective MBR: boot0 code identical, boot1 LBA patched, 0xEE")


def check_gpt(img, esp_mib, reserve_mib):
    total_sectors = img.size() // SECTOR
    hdr = img.read(SECTOR, 92)
    if hdr[:8] != b"EFI PART":
        problem("GPT: signature missing")
        return None

    revision, hdr_size, crc, _ = struct.unpack_from("<IIII", hdr, 8)
    my, alt, first, last = struct.unpack_from("<QQQQ", hdr, 24)
    entries_lba, nentries, esize, ecrc = struct.unpack_from("<QIII", hdr, 72)

    tmp = bytearray(hdr)
    struct.pack_into("<I", tmp, 16, 0)
    if crc32(bytes(tmp)) != crc:
        problem("GPT: primary header CRC incorrect")
    if my != 1 or alt != total_sectors - 1:
        problem(f"GPT: MyLBA/AltLBA = {my}/{alt}, expected 1/{total_sectors-1}")
    if (first, last) != (34, total_sectors - 34):
        problem(f"GPT: usable range {first}/{last}")

    entries_raw = img.read(entries_lba * SECTOR, nentries * esize)
    if crc32(entries_raw) != ecrc:
        problem("GPT: entry table CRC incorrect")

    parts = []
    for i in range(nentries):
        e = entries_raw[i * esize:(i + 1) * esize]
        if e[:16] == b"\x00" * 16:
            continue
        type_guid = guid_str(e[:16])
        part_guid = guid_str(e[16:32])
        pfirst, plast, attrs = struct.unpack_from("<QQQ", e, 32)
        name = e[56:128].decode("utf-16-le").rstrip("\x00")
        parts.append({
            "n": i + 1, "type": type_guid, "guid": part_guid,
            "first": pfirst, "last": plast, "attrs": attrs, "name": name,
        })

    if len(parts) != 3:
        problem(f"GPT: {len(parts)} partitions, expected 3")
        return parts

    p1, p2, p3 = parts
    esp_sectors = esp_mib * MIB // SECTOR
    reserve_sectors = reserve_mib * MIB // SECTOR
    boot_start = ESP_START + esp_sectors + reserve_sectors
    ext4_expected_start = boot_start + BOOT_PART_SECTORS

    # P1 = ESP, the FIRST partition: some OEM firmwares only try the
    # default boot path (EFI/BOOT/BOOTX64.EFI) on partition #1.
    if p1["type"] != GUID_ESP or p1["name"] != "PrismOS ESP":
        problem(f"P1: incorrect type/name ({p1['type']}, \"{p1['name']}\") "
                "- the ESP MUST be the first partition")
    if p1["first"] != ESP_START:
        problem(f"P1: start {p1['first']}, expected {ESP_START}")
    if p1["last"] != ESP_START + esp_sectors - 1:
        problem(f"P1: end {p1['last']}, expected {ESP_START + esp_sectors - 1}")
    if not (p1["attrs"] & 0x4):
        problem("P1: 'legacy BIOS bootable' attribute (bit 2) missing - "
                "some OEM firmwares do not list the drive in the boot "
                "menu without it")

    # P2 = BIOS boot partition, after the unallocated reserve.
    if p2["type"] != GUID_BIOS_BOOT or p2["name"] != "PrismOS boot":
        problem(f"P2: incorrect type/name ({p2['type']}, \"{p2['name']}\")")
    if (p2["first"], p2["last"]) != (boot_start,
                                    boot_start + BOOT_PART_SECTORS - 1):
        problem(f"P2: range {p2['first']}..{p2['last']}, "
                f"expected {boot_start}..{boot_start + BOOT_PART_SECTORS - 1}")

    # P3 = ext4 data.
    if p3["type"] != GUID_LINUX_FS or p3["name"] != "PrismOS data":
        problem(f"P3: incorrect type/name ({p3['type']}, \"{p3['name']}\")")
    if p3["first"] != ext4_expected_start:
        problem(f"P3: start {p3['first']}, expected {ext4_expected_start} "
                f"(ESP {esp_mib} MiB + reserve {reserve_mib} MiB + boot)")
    if p3["last"] != total_sectors - 34:
        problem(f"P3: end {p3['last']}, expected {total_sectors - 34}")

    if p3["first"] % 2048 != 0:
        problem("P3: start not MiB-aligned")

    # Backup GPT
    bk_entries = img.read((total_sectors - 33) * SECTOR, nentries * esize)
    if bk_entries != entries_raw:
        problem("GPT: backup entries differ")
    bk_hdr = img.read((total_sectors - 1) * SECTOR, 92)
    if bk_hdr[:8] != b"EFI PART":
        problem("GPT: backup header unreadable")
    bk_crc = struct.unpack_from("<I", bk_hdr, 16)[0]
    bk_tmp = bytearray(bk_hdr)
    struct.pack_into("<I", bk_tmp, 16, 0)
    if crc32(bytes(bk_tmp)) != bk_crc:
        problem("GPT: backup header CRC incorrect")

    # The reserve must not contain any partition
    if reserve_sectors:
        gap = img.read((ESP_START + esp_sectors) * SECTOR, 4096)
        if any(gap):
            problem("reserve: non-zero bytes detected at the head of the area")

    note(f"GPT: 3 correct partitions - ESP first "
         f"(ESP@{p1['first']} ({esp_mib} MiB), reserve {reserve_mib} MiB, "
         f"boot@{p2['first']}, "
         f"ext4@{p3['first']} ({(p3['last']-p3['first']+1)*SECTOR/MIB:.2f} MiB), "
         f"backup intact)")
    return parts


def check_boot_partition(img, source, boot_start):
    part = img.read(boot_start * SECTOR, BOOT_PART_SECTORS * SECTOR)
    boot1 = bytearray(open(os.path.join(source, "boot1.bin"), "rb").read())
    loader = open(os.path.join(source, "loader.bin"), "rb").read()

    if len(boot1) != BOOT1_SECTORS * SECTOR:
        problem("boot1: incorrect size on the source side")
        return

    if struct.unpack_from("<I", part, 0)[0] != BOOT1_MAGIC:
        problem("boot1: PRB1 magic missing on the disk")

    lba = struct.unpack_from("<Q", part, 4)[0]
    sectors = struct.unpack_from("<I", part, 12)[0]
    expected_lba = boot_start + BOOT1_SECTORS
    expected_sectors = (len(loader) + SECTOR - 1) // SECTOR
    if lba != expected_lba:
        problem(f"boot1: loader LBA = {lba}, expected {expected_lba}")
    if sectors != expected_sectors:
        problem(f"boot1: loader sectors = {sectors}, expected "
                f"{expected_sectors}")

    # Expected boot1: PATCHED source (loader LBA, sector count).
    expected_boot1 = bytearray(boot1)
    struct.pack_into("<Q", expected_boot1, 4, expected_lba)
    struct.pack_into("<I", expected_boot1, 12, expected_sectors)
    if part[:len(boot1)] != bytes(expected_boot1):
        problem("boot1: bytes differ from the patched boot1.bin")
    on_disk = part[BOOT1_SECTORS * SECTOR:
                   BOOT1_SECTORS * SECTOR + len(loader)]
    if on_disk != loader:
        problem("loader: bytes differ from loader.bin")

    # The rest of the partition must be clean (zeros).
    tail = part[BOOT1_SECTORS * SECTOR + len(loader):]
    if any(tail):
        problem("boot partition: stray bytes after the loader")

    note(f"boot partition: PRB1 magic, loader LBA {lba}, "
         f"{sectors} sectors, bytes compliant")


class Fat32:
    """Minimal FAT32 reader (verification)."""

    def __init__(self, img, base):
        self.img = img
        self.base = base
        bs = img.read(base, 512)

        if bs[510:512] != b"\x55\xAA":
            problem("FAT32: 55AA signature missing from the boot sector")
        if bs[0x52:0x5A] != b"FAT32   ":
            problem("FAT32: FAT32 type missing from the boot sector")

        self.bps = struct.unpack_from("<H", bs, 0x0B)[0]
        self.spc = bs[0x0D]
        self.reserved = struct.unpack_from("<H", bs, 0x0E)[0]
        self.nfats = bs[0x10]
        self.totsec = struct.unpack_from("<I", bs, 0x20)[0]
        self.fatsz = struct.unpack_from("<I", bs, 0x24)[0]
        self.rootclus = struct.unpack_from("<I", bs, 0x2C)[0]
        self.label = bs[0x47:0x52].decode("ascii").rstrip()

        # Hidden sectors = partition start LBA (like mkfs.fat formatting
        # a real partition; 0 on a whole-disk image).
        hidden = struct.unpack_from("<I", bs, 0x1C)[0]
        if hidden != ESP_START:
            problem(f"FAT32: HiddSec = {hidden}, expected {ESP_START} "
                    f"(ESP LBA)")

        # FAT32 backup sectors: 6 = boot sector, 7 = FSInfo - must
        # replicate the primaries.
        if img.read(base + 6 * 512, 512) != bs:
            problem("FAT32: backup boot sector (6) "
                    "differs from the primary")
        info1 = img.read(base + 1 * 512, 512)
        if img.read(base + 7 * 512, 512) != info1:
            problem("FAT32: backup FSInfo (7) differs from the primary")

        if self.bps != 512:
            problem(f"FAT32: bytes/sector = {self.bps}")
        if self.rootclus != 2:
            problem(f"FAT32: RootClus = {self.rootclus}")

        self.fat_off = self.base + self.reserved * self.bps
        self.fat = img.read(self.fat_off, self.fatsz * self.bps)
        self.fat2 = img.read(self.fat_off + self.fatsz * self.bps,
                             self.fatsz * self.bps)
        if self.fat != self.fat2:
            problem("FAT32: the two FAT copies differ")

        self.data_off = self.base + (self.reserved
                                     + self.nfats * self.fatsz) * self.bps
        self.nclusters = (self.totsec - self.reserved
                          - self.nfats * self.fatsz) // self.spc
        if self.nclusters < 65525:
            problem(f"FAT32: {self.nclusters} clusters (< 65525)")

        # FAT[0..2]
        if struct.unpack_from("<I", self.fat, 0)[0] != 0x0FFFFFF8:
            problem("FAT32: FAT[0] != media descriptor")
        if struct.unpack_from("<I", self.fat, 4)[0] != 0x0FFFFFFF:
            problem("FAT32: FAT[1] incorrect")

        # FSInfo
        info = img.read(self.base + 512, 512)
        if struct.unpack_from("<I", info, 0)[0] != 0x41615252:
            problem("FSInfo: RRaA signature missing")
        free = struct.unpack_from("<I", info, 0x1E8)[0]
        real_free = sum(1 for c in range(2, 2 + self.nclusters)
                        if self.entry(c) == 0)
        if free != 0xFFFFFFFF and free != real_free:
            problem(f"FSInfo: {free} free declared, {real_free} real")
        else:
            note(f"FSInfo consistent: {real_free}/{self.nclusters} clusters "
                 f"free, clusters of {self.spc * self.bps} B")

    def entry(self, c):
        return struct.unpack_from("<I", self.fat, c * 4)[0] & 0x0FFFFFFF

    def chain(self, c):
        out = []
        seen = set()
        cur = c
        while 2 <= cur < 2 + self.nclusters:
            if cur in seen:
                problem(f"FAT32: loop at cluster {cur}")
                break
            seen.add(cur)
            out.append(cur)
            nxt = self.entry(cur)
            if nxt >= 0x0FFFFFF8:
                return out
            cur = nxt
        problem(f"FAT32: unterminated chain from cluster {c}")
        return out

    def read_chain(self, c, size):
        cs = self.spc * self.bps
        data = b""
        for cl in self.chain(c):
            if len(data) >= size:
                break
            data += self.img.read(self.data_off + (cl - 2) * cs, cs)
        return data[:size]

    def list_dir(self, cluster):
        cs = self.spc * self.bps
        data = b""
        for cl in self.chain(cluster):
            data += self.img.read(self.data_off + (cl - 2) * cs, cs)

        entries = []
        pending = ""
        pending_csum = None
        for off in range(0, len(data) - 31, 32):
            e = data[off:off + 32]
            if e[0] == 0x00:
                break
            if e[0] == 0xE5:
                pending = ""
                pending_csum = None
                continue
            attr = e[0x0B]
            if attr == 0x0F:
                # LFN
                seq = e[0]
                csum = e[0x0D]
                utf16 = (e[1:0x0B] + e[0x0E:0x1A] + e[0x1C:0x20])
                name = ""
                for i in range(0, len(utf16), 2):
                    ch = struct.unpack_from("<H", utf16, i)[0]
                    if ch == 0x0000:
                        break
                    if ch == 0xFFFF:
                        continue
                    name += chr(ch)
                if seq & 0x40:
                    pending = name
                    pending_csum = csum
                else:
                    if pending_csum is not None and pending_csum != csum:
                        pending = ""   # inconsistent LFN set
                        pending_csum = None
                    else:
                        pending = name + pending
                continue
            if attr & 0x08:
                pending = ""
                pending_csum = None
                continue
            # The LFN set must match its short entry (an EDK2 firmware
            # checks this checksum and ignores the long name otherwise)
            # - implemented independently of the writer.
            if pending and pending_csum is not None \
                    and lfn_checksum(e[0:11]) != pending_csum:
                problem(f"LFN: incorrect checksum for \"{pending}\"")
            name = pending if pending else self.short_name(e)
            pending = ""
            pending_csum = None
            if name in (".", ".."):
                continue
            first = struct.unpack_from("<H", e, 0x1A)[0] \
                | (struct.unpack_from("<H", e, 0x14)[0] << 16)
            size = struct.unpack_from("<I", e, 0x1C)[0]
            entries.append((name, attr & 0x10 != 0, first, size))
        return entries

    @staticmethod
    def short_name(e):
        base = e[0:8].decode("ascii").rstrip()
        ext = e[8:11].decode("ascii").rstrip()
        return base + ("." + ext if ext else "")


def find_path(fat, path):
    """Resolves a case-insensitive path from the FAT32 root.
    Returns (name, isdir, first cluster, size) or None."""
    cur = fat.rootclus
    parts = [p for p in path.split("/") if p]
    for i, part in enumerate(parts):
        entries = fat.list_dir(cur)
        hit = next((e for e in entries if e[0].lower() == part.lower()),
                   None)
        if hit is None:
            return None
        _, isdir, first, size = hit
        if i == len(parts) - 1:
            return hit
        if not isdir:
            return None
        cur = first
    return None


def check_esp(img, source, manifest):
    fat = Fat32(img, ESP_START * SECTOR)

    # Label
    root_entries = fat.list_dir(fat.rootclus)
    note(f"FAT32: label \"{fat.label}\", "
         f"{len(root_entries)} entries at the root")

    def walk(cluster, prefix):
        for name, isdir, first, size in fat.list_dir(cluster):
            rel = prefix + name
            if isdir:
                walk(first, rel + "/")
                continue
            info = manifest.get("esp/" + rel)
            if info is None:
                problem(f"ESP: unknown file {rel}")
                continue
            if info["size"] != size:
                problem(f"ESP: size of {rel} = {size}, expected "
                        f"{info['size']}")
                continue
            data = fat.read_chain(first, size)
            if crc32(data) != info["crc32"]:
                problem(f"ESP: CRC of {rel} incorrect")
        # Converse: every manifest file under this prefix must exist on
        # the ESP (handled globally below).

    walk(fat.rootclus, "")

    # Global converse.
    on_disk = set()

    def collect(cluster, prefix):
        for name, isdir, first, size in fat.list_dir(cluster):
            rel = prefix + name
            if isdir:
                collect(first, rel + "/")
            else:
                on_disk.add(rel)

    collect(fat.rootclus, "")
    for rel in manifest:
        if rel.startswith("esp/"):
            rel2 = rel[4:]
            if rel2 not in on_disk:
                problem(f"ESP: missing file {rel2}")
    if len(on_disk) != len(manifest) - 3:   # - boot0/boot1/loader.bin
        problem(f"ESP: {len(on_disk)} files on the ESP, "
                f"{len(manifest) - 3} expected")

    # UEFI boot: EFI/BOOT/BOOTX64.EFI must exist and be an x86-64 EFI
    # application (PE32+, subsystem 10) - Python mirror of the C++
    # check (Pe.cpp), implemented independently.
    hit = find_path(fat, "EFI/BOOT/BOOTX64.EFI")
    if hit is None:
        problem("UEFI: EFI/BOOT/BOOTX64.EFI missing from the ESP")
    else:
        _, isdir, first, size = hit
        if isdir:
            problem("UEFI: EFI/BOOT/BOOTX64.EFI is a directory")
        else:
            pe_data = fat.read_chain(first, size)
            errs = check_pe_efi(pe_data)
            if errs:
                for err in errs:
                    problem(f"UEFI: BOOTX64.EFI {err}")
            else:
                note(f"UEFI: BOOTX64.EFI valid x86-64 EFI application "
                     f"({size} bytes)")


def check_ext4(img, parts, label, stub_log):
    if not parts or len(parts) < 3:
        return
    p3 = parts[2]
    sb_off = p3["first"] * SECTOR + 1024
    sb = img.read(sb_off, 1024)
    magic = struct.unpack_from("<H", sb, 0x38)[0]
    if magic != 0xEF53:
        problem(f"ext4: magic 0x{magic:04X} (expected 0xEF53)")
    else:
        vol = sb[0x78:0x88].decode("ascii", "replace").rstrip("\x00")
        state = struct.unpack_from("<H", sb, 0x38 - 0x20)[0] \
            if False else struct.unpack_from("<I", sb, 0x18)[0]
        note(f"ext4: magic OK, state {state}, label \"{vol}\"")
        if vol != label:
            problem(f"ext4: label \"{vol}\" != \"{label}\"")

    if stub_log and os.path.exists(stub_log):
        with open(stub_log) as f:
            for line in f:
                call = json.loads(line)
                parsed = call["parsed"]
                if parsed.get("offset") != p3["first"] * SECTOR:
                    problem(f"mke2fs: offset {parsed.get('offset')} != "
                            f"{p3['first'] * SECTOR}")
                expected_blocks = ((p3["last"] - p3["first"] + 1) * SECTOR
                                   - 33 * SECTOR + 33 * SECTOR) // 4096
                # exact partition size in 4096-byte blocks
                expected_blocks = (p3["last"] - p3["first"] + 1) * SECTOR // 4096
                if parsed.get("size_blocks") != expected_blocks:
                    problem(f"mke2fs: {parsed.get('size_blocks')} blocks, "
                            f"expected {expected_blocks}")
                if parsed.get("label") != label:
                    problem("mke2fs: incorrect label")
                if "-t" in call["argv"]:
                    t = call["argv"][call["argv"].index("-t") + 1]
                    if t != "ext4":
                        problem(f"mke2fs: type {t}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("source")
    ap.add_argument("--esp-size", type=int, default=64)
    ap.add_argument("--reserve", type=int, default=128)
    ap.add_argument("--stub-log", default=None)
    ap.add_argument("--label-ext4", default="PRISMDATA")
    args = ap.parse_args()

    img = Image(args.image)
    manifest_path = os.path.join(args.source, "manifest.json")
    if not os.path.exists(manifest_path):
        problem("source manifest not found: " + manifest_path)
        manifest = {}
    else:
        with open(manifest_path) as f:
            manifest = json.load(f)

    print(f"image: {args.image} ({img.size() / MIB:.2f} MiB)")
    print(f"source: {args.source}")
    print()

    # Expected BIOS boot partition start: after the ESP and the reserve.
    esp_sectors = args.esp_size * MIB // SECTOR
    reserve_sectors = args.reserve * MIB // SECTOR
    boot_start = ESP_START + esp_sectors + reserve_sectors

    check_mbr(img, args.source, boot_start)
    parts = check_gpt(img, args.esp_size, args.reserve)
    check_boot_partition(img, args.source, boot_start)
    check_esp(img, args.source, manifest)
    check_ext4(img, parts, args.label_ext4, args.stub_log)

    for n in notes:
        print(f"  [note] {n}")
    print()
    if problems:
        for p in problems:
            print(f"  [FAIL] {p}")
        print(f"\n{len(problems)} problem(s).")
        return 1
    print("  ===== IMAGE COMPLIANT =====")
    return 0


if __name__ == "__main__":
    sys.exit(main())
