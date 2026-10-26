#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
mkimage.py - build the PrismOS bootable disk image.

Disk layout (GPT with a protective MBR carrying boot0):

    LBA 0            protective MBR: boot0 code, disk signature, 0xEE entry
    LBA 1            GPT header
    LBA 2-33         GPT partition entries
    LBA 2048-...     P1 "PrismOS ESP": EFI System Partition, FAT.  Minimal:
                     only EFI/BOOT/BOOTX64.EFI, the UEFI stub the firmware
                     launches (the loader reads its files from P3).
    next             P2 "PrismOS boot": BIOS boot partition, raw sectors:
                         boot1 (BOOT1_SECTORS sectors) followed by the
                         64-bit loader image
    next             P3 "PrismOS root": ext4, read by the loader (read-only
                     boot tree) and by the kernel (root filesystem):
                         /boot/kernel/kernel, /boot/lua/*.lua,
                         /boot/loader.conf, and the usr-merged tree
                         (phase 3.3): /usr/{bin,lib} with the root
                         symlinks bin|sbin -> usr/bin, lib|lib64 -> usr/lib,
                         /dev, /etc and /tmp (the tmpfs mount point)
    last 33 LBAs     backup GPT

Only the Python standard library plus mkfs.fat, mtools and mke2fs are
required.
"""

import argparse
import os
import re
import struct
import subprocess
import sys
import tempfile
import uuid
import zlib

SECTOR = 512
ESP_START = 2048
BOOT_PART_SECTORS = 4096          # 2 MiB, P2

TYPE_BIOS_BOOT = uuid.UUID("21686148-6449-6E6F-744E-656564454649")
TYPE_ESP = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
TYPE_LINUX = uuid.UUID("0FC63DAF-8483-4772-8E79-3D69D8477DE4")
NS = uuid.uuid5(uuid.NAMESPACE_DNS, "prismos.noxio.dev")


def read_constants(path):
    consts = {}
    pat = re.compile(r"^#define\s+(\w+)\s+(0x[0-9A-Fa-f]+|\d+)\b")
    with open(path) as f:
        for line in f:
            m = pat.match(line)
            if m:
                consts[m.group(1)] = int(m.group(2), 0)
    return consts


def crc32(data):
    return zlib.crc32(data) & 0xFFFFFFFF


def part_entry(type_guid, name, first, last):
    e = bytearray(128)
    e[0:16] = type_guid.bytes_le
    e[16:32] = uuid.uuid5(NS, name).bytes_le
    struct.pack_into("<QQQ", e, 32, first, last, 0)
    e[56:56 + 72] = name.encode("utf-16-le").ljust(72, b"\0")[:72]
    return bytes(e)


def gpt_header(my_lba, alt_lba, first_usable, last_usable, disk_guid,
               entries_lba, entries_crc):
    h = bytearray(92)
    h[0:8] = b"EFI PART"
    struct.pack_into("<IIII", h, 8, 0x00010000, 92, 0, 0)
    struct.pack_into("<QQQQ", h, 24, my_lba, alt_lba, first_usable, last_usable)
    h[56:72] = disk_guid.bytes_le
    struct.pack_into("<QIII", h, 72, entries_lba, 128, 128, entries_crc)
    struct.pack_into("<I", h, 16, crc32(bytes(h)))
    return bytes(h).ljust(SECTOR, b"\0")


def mib_sectors(mib):
    return mib * 1024 * 1024 // SECTOR


def build_fat(esp_dir, size_mib, workdir):
    """FAT filesystem image populated from esp_dir (FAT32, FAT16 fallback).

    A spec-valid FAT32 needs at least 65525 clusters, which with 512-byte
    clusters means roughly 33 MiB of data area: smaller ESPs would produce
    a volume some firmwares (EDK2 classifies by cluster count) refuse to
    mount.  Refuse instead of building something unbootable."""
    if size_mib < 34:
        sys.exit("ESP too small: a valid FAT32 needs at least 34 MiB "
                 "(65525 clusters x 512 B); use --esp-size 36 or more")
    img = os.path.join(workdir, "esp.img")
    with open(img, "wb") as f:
        f.truncate(size_mib * 1024 * 1024)
    subprocess.run(["mkfs.fat", "-F", "32", "-n", "PRISMESP", img], check=True)
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1", TZ="UTC")
    for root, dirs, files in sorted(os.walk(esp_dir)):
        dirs.sort()
        rel = os.path.relpath(root, esp_dir)
        if rel != ".":
            target = "::/" + rel.replace(os.sep, "/")
            subprocess.run(["mmd", "-i", img, target], check=True, env=env)
        for name in sorted(files):
            src = os.path.join(root, name)
            dst = "::/" + (name if rel == "." else
                           rel.replace(os.sep, "/") + "/" + name)
            subprocess.run(["mcopy", "-i", img, "-m", src, dst],
                          check=True, env=env)
    return img


def validate_root(root_dir):
    """The usr-merge layout contract (phase 3.3), checked before mke2fs so
    a broken staging dies with a message instead of an unbootable image:
    binaries in /usr/bin, the shared half in /usr/lib, the four /usr-merge
    symlinks at the root, a /tmp directory for the boot-time tmpfs mount,
    and the kernel where the loader reads it."""
    if not os.path.isdir(root_dir):
        sys.exit("root tree: %s is not a directory" % root_dir)
    if not os.path.isfile(os.path.join(root_dir, "boot", "kernel", "kernel")):
        sys.exit("root tree: boot/kernel/kernel missing (the loader reads it)")
    for d in ("usr/bin", "usr/lib", "dev", "etc", "tmp"):
        if not os.path.isdir(os.path.join(root_dir, d)):
            sys.exit("root tree: /%s missing" % d)
    bins = [f for f in os.listdir(os.path.join(root_dir, "usr", "bin"))
            if not f.startswith(".")]
    if not bins:
        sys.exit("root tree: /usr/bin is empty (the packages did not install)")
    for f in ("ld.so", "libc.so"):
        if not os.path.isfile(os.path.join(root_dir, "usr", "lib", f)):
            sys.exit("root tree: /usr/lib/%s missing (the dynamic half)" % f)
    # The four /usr-merge symlinks: bin, sbin -> usr/bin; lib, lib64 -> usr/lib.
    for name, target in (("bin", "usr/bin"), ("sbin", "usr/bin"),
                         ("lib", "usr/lib"), ("lib64", "usr/lib")):
        p = os.path.join(root_dir, name)
        if not os.path.islink(p):
            sys.exit("root tree: /%s is not a symlink (the usr-merge)" % name)
        if os.readlink(p) != target:
            sys.exit("root tree: /%s -> %r, wanted %r"
                     % (name, os.readlink(p), target))
    print("root tree: usr-merge layout ok (%d binaries in /usr/bin)" % len(bins))


def build_ext4(root_dir, size_mib, workdir):
    """ext4 filesystem image populated from root_dir (P3)."""
    img = os.path.join(workdir, "root.img")
    with open(img, "wb") as f:
        f.truncate(size_mib * 1024 * 1024)
    subprocess.run(["mke2fs", "-q", "-F", "-t", "ext4", "-L", "PRISMROOT",
                    "-d", root_dir, img], check=True)
    return img


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--inc", default="include/prism.inc")
    ap.add_argument("--boot0", required=True)
    ap.add_argument("--boot1", required=True)
    ap.add_argument("--loader", required=True, help="64-bit BIOS loader image")
    ap.add_argument("--esp-dir", required=True,
                    help="directory tree copied into the ESP (P1)")
    ap.add_argument("--esp-size", type=int, default=36,
                    help="ESP size in MiB (FAT32 needs at least 34)")
    ap.add_argument("--root-dir", required=True,
                    help="directory tree copied into the ext4 root (P3)")
    ap.add_argument("--root-size", type=int, default=64,
                    help="ext4 root size in MiB")
    ap.add_argument("-o", "--output", required=True)
    args = ap.parse_args()

    validate_root(args.root_dir)

    c = read_constants(args.inc)
    boot1_sectors = c["BOOT1_SECTORS"]
    lba_off = c["BOOT0_LBA_OFF"]
    magic = c["BOOT1_MAGIC"]
    loader_max = c["LOADER_MAX_SIZE"]

    boot0 = bytearray(open(args.boot0, "rb").read())
    boot1 = bytearray(open(args.boot1, "rb").read())
    loader = open(args.loader, "rb").read()

    if len(boot0) != SECTOR:
        sys.exit("boot0 must be exactly 512 bytes")
    if len(boot1) != boot1_sectors * SECTOR:
        sys.exit("boot1 must be exactly %d bytes" % (boot1_sectors * SECTOR))
    if struct.unpack_from("<I", boot1, 0)[0] != magic:
        sys.exit("boot1 magic mismatch")
    if len(loader) > loader_max:
        sys.exit("loader (%d bytes) exceeds LOADER_MAX_SIZE" % len(loader))
    loader_sectors = (len(loader) + SECTOR - 1) // SECTOR
    if (boot1_sectors + loader_sectors) > BOOT_PART_SECTORS:
        sys.exit("boot partition too small for boot1 + loader")

    esp_sectors = mib_sectors(args.esp_size)
    boot_start = ESP_START + esp_sectors
    root_start = boot_start + BOOT_PART_SECTORS
    root_sectors = mib_sectors(args.root_size)

    total = root_start + root_sectors + 33 + 1    # backup entries + header
    total = (total + 2047) // 2048 * 2048
    last = total - 1
    first_usable, last_usable = 34, last - 33

    # --- build the two filesystem images ------------------------------------
    with tempfile.TemporaryDirectory() as tmp:
        esp = open(build_fat(args.esp_dir, args.esp_size, tmp), "rb").read()
        root = open(build_ext4(args.root_dir, args.root_size, tmp), "rb").read()

    # --- patch boot0 / boot1 ------------------------------------------------
    struct.pack_into("<Q", boot0, lba_off, boot_start)
    struct.pack_into("<Q", boot1, 4, boot_start + boot1_sectors)
    struct.pack_into("<I", boot1, 12, loader_sectors)

    # --- protective MBR -----------------------------------------------------
    mbr = bytearray(SECTOR)
    mbr[0:len(boot0)] = boot0
    struct.pack_into("<I", mbr, 0x1B8, 0x5052534D)         # disk signature
    pmbr_sectors = min(total - 1, 0xFFFFFFFF)
    mbr[0x1BE:0x1CE] = struct.pack("<B3sB3sII", 0x80, b"\x00\x02\x00", 0xEE,
                                   b"\xFF\xFF\xFF", 1, pmbr_sectors)
    mbr[0x1FE:0x200] = b"\x55\xAA"

    # --- GPT -----------------------------------------------------------------
    entries = bytearray(128 * 128)
    entries[0:128] = part_entry(TYPE_ESP, "PrismOS ESP",
                                ESP_START, ESP_START + esp_sectors - 1)
    entries[128:256] = part_entry(TYPE_BIOS_BOOT, "PrismOS boot",
                                  boot_start, boot_start + BOOT_PART_SECTORS - 1)
    entries[256:384] = part_entry(TYPE_LINUX, "PrismOS root",
                                  root_start, root_start + root_sectors - 1)
    entries_crc = crc32(bytes(entries))
    disk_guid = uuid.uuid5(NS, "disk")
    primary = gpt_header(1, last, first_usable, last_usable, disk_guid, 2,
                         entries_crc)
    backup = gpt_header(last, 1, first_usable, last_usable, disk_guid,
                        last - 32, entries_crc)

    # --- assemble -------------------------------------------------------------
    with open(args.output, "wb") as out:
        out.truncate(total * SECTOR)
        out.seek(0)
        out.write(mbr)
        out.write(primary)
        out.write(entries)
        out.seek(ESP_START * SECTOR)
        out.write(esp)
        out.seek(boot_start * SECTOR)
        out.write(boot1)
        out.write(loader)
        out.seek(root_start * SECTOR)
        out.write(root)
        out.seek((last - 32) * SECTOR)
        out.write(entries)
        out.write(backup)

    print("%s: %d MiB, ESP @ LBA %d, boot @ LBA %d, root @ LBA %d" % (
        args.output, total * SECTOR // (1024 * 1024), ESP_START, boot_start,
        root_start))


if __name__ == "__main__":
    main()
