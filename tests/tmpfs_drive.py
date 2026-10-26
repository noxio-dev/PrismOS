#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Boot-time proof of phase 3.3: the /tmp tmpfs and the usr-merge, driven
through the shell of a freshly booted image (needs qemu, like
tests/shell_drive.py whose run() this reuses).

    tests/tmpfs_drive.py build/prismos.img

Checks: the boot log mounts tmpfs on /tmp; /bin, /sbin, /lib and
/lib64 are the usr-merge symlinks; the tmpfs package passes its own
scenarios from ring 3 -- twice, because a good /tmp citizen leaves
the tree clean -- and the shell can write /tmp itself.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from shell_drive import run  # noqa: E402


def main():
    if len(sys.argv) < 2:
        print("usage: tests/tmpfs_drive.py <image>")
        return 2
    out = run(sys.argv[1], [
        "ls -l /",
        "exec hello",
        "exec tmpfs",
        "exec tmpfs",
        "write /tmp/from-shell written by the shell\\n",
        "cat /tmp/from-shell",
        "df",
    ], timeout=180)
    print(out)
    want = [
        "tmpfs on /tmp",                      # the boot-time mount
        "-> usr/bin",                         # bin, sbin symlinks
        "-> usr/lib",                         # lib, lib64 symlinks
        "tmpfs: 37/37 checks passed",         # the package, first run
        "written by the shell",               # the shell's own write
    ]
    missing = [w for w in want if w not in out]
    if out.count("37/37 checks passed") < 2:
        missing.append("tmpfs: 37/37 twice (the cleanup works)")
    if missing:
        print("tmpfs_drive: FAIL -- missing: %s" % ", ".join(missing))
        return 1
    print("tmpfs_drive: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
