#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
root.stamp for phase 3.3's second half (init / tty / procfs): the root
tree gains /proc (the procfs mount point) and the /etc files a session
expects (passwd + group for the identity lookups, motd for the hello).

Tab-safe by construction (the Makefile's tabs are literal \\t in the
patch text below, never through an editor's spaces conversion).
"""

import sys

MK = "Makefile"

# (anchor, must_contain, replacement_lines) -- every replacement is
# asserted to land exactly once.
EDITS = [
    # 1. the directory list: /proc between /tmp and /usr/bin (the
    # continuation lines carry 17 spaces, like the file does)
    (
        "\trm -rf $B/root\n"
        "\tmkdir -p $B/root/boot/lua $B/root/boot/defaults $B/root/boot/kernel \\\n"
        "                 $B/root/dev $B/root/etc $B/root/tmp \\\n"
        "                 $B/root/usr/bin $B/root/usr/lib\n"
        "\tchmod 1777 $B/root/tmp\n",
        "$B/root/proc",
        "\trm -rf $B/root\n"
        "\tmkdir -p $B/root/boot/lua $B/root/boot/defaults $B/root/boot/kernel \\\n"
        "                 $B/root/dev $B/root/etc $B/root/proc $B/root/tmp \\\n"
        "                 $B/root/usr/bin $B/root/usr/lib\n"
        "\tchmod 1777 $B/root/tmp\n",
    ),
    # 2. the /etc files, right after the /tmp mode (before the boot tree
    # copies: the passwd lines are long, they get their own block)
    (
        "\tcp boot/lua/*.lua $B/root/boot/lua/\n",
        "etc/passwd",
        "\t# /etc for the session (phase 3.3): passwd + group -- the\n"
        "\t# identity lookups a libc's getpwnam serves, root only -- and\n"
        "\t# the motd init's shell greets with.  /proc is here as well:\n"
        "\t# the procfs the kernel mounts on it needs the directory.\n"
        "\tprintf 'root:x:0:0:root:/:/usr/bin/sh\\n' > $B/root/etc/passwd\n"
        "\tprintf 'root:x:0:\\n' > $B/root/etc/group\n"
        "\tprintf 'Welcome to PrismOS (init + tty + procfs, phase 3.3).\\n' \\\n"
        "\t\t> $B/root/etc/motd\n"
        "\tcp boot/lua/*.lua $B/root/boot/lua/\n",
    ),
    # 3. the comment above the rule: /proc + /etc in the inventory
    (
        "# creates one (1777); mkdir_p keeps older images working.\n",
        "/proc is here",
        "# creates one (1777); mkdir_p keeps older images working.  /proc is\n"
        "# here the same way (0555) for the generated procfs the kernel\n"
        "# mounts on it, and /etc carries the session's passwd / group /\n"
        "# motd (phase 3.3's second half).\n",
    ),
]


def main():
    try:
        with open(MK) as f:
            text = f.read()
    except OSError as e:
        sys.exit("cannot read %s: %s" % (MK, e))

    for old, must, new in EDITS:
        if must in text:
            print("already applied: %s" % must)
            continue
        n = text.count(old)
        assert n == 1, "anchor not found (or not unique: %d): %r" % (n, old[:70])
        text = text.replace(old, new)

    with open(MK, "w") as f:
        f.write(text)
    print("Makefile: root.stamp updated (/proc, /etc/passwd+group+motd)")


if __name__ == "__main__":
    main()
