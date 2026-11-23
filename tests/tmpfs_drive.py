#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Boot-time proof of the /tmp tmpfs and the usr-merge, driven through the
*userland shell* of a freshly booted image (needs qemu, like
tests/shell_drive.py whose run() this reuses -- the boot goes through
BusyBox init since 3.4.5: /etc/inittab starts /etc/init.d/rcS, whose
S00mounts mounts tmpfs on /tmp plus devfs/procfs/sysfs, then a login on
ttyS0 hands over to busybox ash at the prism$ prompt).

    tests/tmpfs_drive.py build/prismos.img

Checks: pid 1 ran rcS (its S00mounts); /proc/mounts lists the four
mounts -- tmpfs on /tmp first among the interesting ones; /bin and
/lib are the usr-merge symlinks; the tmpfs package passes its own
scenarios from ring 3 -- twice, because a good /tmp citizen leaves the
tree clean -- and the shell (ash's redirections) can write /tmp itself,
which df then reports.
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
        "cat /proc/mounts",
        "ls -l /",
        "tmpfs",
        "tmpfs",
        "echo written by the shell > /tmp/from-shell",
        "cat /tmp/from-shell",
        "df",
    ], timeout=180, prompt=b"prism$ ")
    print(out)
    want = [
        "starting pid 2, tty '': '/etc/init.d/rcS'",
        "S00mounts",                       # the mount script ran
        "tmpfs /tmp tmpfs rw 0 0",         # the four mounts of S00mounts
        "devfs /dev devfs ro 0 0",
        "procfs /proc procfs ro 0 0",
        "sysfs /sys sysfs ro 0 0",
        "-> usr/bin",                      # bin, sbin symlinks (lstat)
        "-> usr/lib",                      # lib, lib64 symlinks (lstat)
        "tmpfs: 37/37 checks passed",      # the package, first run
        "written by the shell",            # the shell's own write
        "Filesystem",                      # busybox df's header
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
