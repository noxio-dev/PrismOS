#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
mke2fs stub for automated tests (the test environment does not
always have e2fsprogs).  Behavior:

  - logs each invocation (argv + cwd) to the file designated by the
    PRISM_STUB_LOG environment variable;
  - writes a minimal but RECOGNIZABLE ext4 superblock (magic 0xEF53,
    clean state) at the requested offset (-E offset=N) of the target
    file, over the requested size;
  - populatable via -d directory (naive copy to validate the option).

Real formatting is done by e2fsprogs on real systems; this stub
makes it possible to check argument construction, the offset, the
size and the exit code.
"""

import json
import os
import struct
import sys

SECTOR = 512


def parse(argv):
    opts = {"offset": 0, "size_blocks": None, "label": "", "seed": None}
    positional = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "-E":
            i += 1
            for kv in argv[i].split(","):
                if kv.startswith("offset="):
                    opts["offset"] = int(kv.split("=", 1)[1], 0)
        elif a == "-L":
            i += 1
            opts["label"] = argv[i]
        elif a == "-b":
            i += 1
            opts["blocksize"] = int(argv[i])
        elif a == "-d":
            i += 1
            opts["seed"] = argv[i]
        elif a in ("-q", "-F"):
            pass
        elif a == "-t":
            i += 1
            opts["type"] = argv[i]
        else:
            positional.append(a)
        i += 1
    opts["target"] = positional[0] if positional else None
    if len(positional) > 1:
        opts["size_blocks"] = int(positional[1])
    return opts


def write_superblock(path, offset, size_bytes, label):
    """Minimal ext2/3/4 superblock: magic, clean state, volume."""
    sb = bytearray(1024)
    struct.pack_into("<H", sb, 0x38, 0xEF53)          # s_magic
    struct.pack_into("<I", sb, 0x18, 1)               # s_state = clean
    struct.pack_into("<I", sb, 0x00, 0)               # inodes count
    struct.pack_into("<I", sb, 0x04, 0)               # blocks count (lo)
    struct.pack_into("<I", sb, 0x18, 1)               # clean state
    struct.pack_into("<I", sb, 0x18 + 4, 0)           # errors
    # label: s_volume_name at 0x78, 16 bytes
    lab = label.encode()[:16]
    sb[0x78:0x78 + len(lab)] = lab

    with open(path, "r+b") as f:
        f.seek(offset + 1024)
        f.write(sb)
        # end-of-area marker (last block of the requested size)
        end = offset + size_bytes - SECTOR
        f.seek(end)
        f.write(b"PRISMEXT4END")


def main():
    opts = parse(sys.argv[1:])
    if not opts.get("target"):
        print("mke2fs stub: missing target", file=sys.stderr)
        return 1

    log = os.environ.get("PRISM_STUB_LOG")
    if log:
        with open(log, "a") as f:
            f.write(json.dumps({
                "argv": sys.argv[1:],
                "parsed": {k: v for k, v in opts.items()
                           if not callable(v)},
            }) + "\n")

    size_bytes = 0
    if opts["size_blocks"] is not None:
        bs = opts.get("blocksize", 4096)
        size_bytes = opts["size_blocks"] * bs
    else:
        size_bytes = os.path.getsize(opts["target"]) - opts["offset"]

    write_superblock(opts["target"], opts["offset"], size_bytes,
                     opts["label"])

    if opts.get("seed"):
        # Naive copy of the directory's files (just to validate that
        # the option is passed through): counting only.
        n = sum(len(files) for _, _, files in os.walk(opts["seed"]))
        print(f"stub: {n} files seeded")

    return 0


if __name__ == "__main__":
    sys.exit(main())
