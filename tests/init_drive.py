#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Boot-time proof of phase 3.3's second half: init, the tty line
discipline and /proc, driven through the userland session of a freshly
booted image (needs qemu).

    tests/init_drive.py build/prismos.img

Checks (roadmap 3.3.8): the kernel starts /sbin/init as pid 1 and the
shell answers at prism$; /proc answers (version, the per-pid files, the
magic self symlink through cat /proc/self/cmdline); the tty edits
(VERASE through the serial line) and interrupts (^C on a blocked
foreground read, reported by the shell as the kill); ^D ends the shell
and init respawns it; and a write to /dev/full is the ENOSPC it must be.
"""
import os
import select
import subprocess
import sys
import time


def run_steps(image, steps, timeout=240):
    """steps: list of (wait_for, send) -- expect-style, control characters
    included.  Returns the whole serial output."""
    base = ["qemu-system-x86_64", "-m", "256M", "-display", "none", "-no-reboot",
            "-chardev", "stdio,id=ser,signal=off", "-serial", "chardev:ser",
            "-drive", "file=%s,format=raw" % image]
    proc = subprocess.Popen(base, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    out = b""
    steps = list(steps)
    deadline = time.time() + timeout
    try:
        while time.time() < deadline:
            r, _, _ = select.select([proc.stdout], [], [], 0.2)
            if r:
                chunk = os.read(proc.stdout.fileno(), 65536)
                if not chunk:
                    break
                out += chunk
            while steps and steps[0][0].encode() in out:
                _, send = steps.pop(0)
                time.sleep(0.4)
                proc.stdin.write(send.encode())
                proc.stdin.flush()
            if not steps:
                time.sleep(1.5)  # let the tail of the output land
                break
    finally:
        try:
            proc.kill()
            proc.wait()
        except Exception:
            pass
    return out.decode("latin-1")


def main():
    if len(sys.argv) < 2:
        print("usage: tests/init_drive.py <image>")
        return 2

    out = run_steps(sys.argv[1], [
        # /proc: the static files, the magic self symlink, the per-pid
        # directory (pid 1 is init itself).
        ("prism$ ", "cat /proc/version\r"),
        ("PrismOS version", "cat /proc/self/cmdline\r"),
        ("/usr/bin/sh", "cat /proc/self/stat\r"),
        ("(sh) S 1", "ls /proc/1\r"),
        ("exe", "cat /etc/passwd\r"),
        ("root:x:0:0:root:/:/usr/bin/sh", "ps\r"),
        # The tty's line discipline: VERASE (0x7f through the serial
        # line) fixes a typoed command, exactly like on a real terminal.
        ("    1     0 S", "pwdd\x7f\r"),           # "pwdd" + backspace -> pwd
        ("/", "echo testt\x7f\r"),           # "testt" + backspace -> test
        # /dev/full: the write no disk could make.
        ("test", "write /dev/full probe\r"),
        # ^C on a blocked foreground read (the fd program's stdin check).
        ("No space left on device", "fd\r"),
        ("type 4 bytes (EXACTLY wxyz):", "\x03"),
        # ^D at the prompt: the shell exits, init reports and respawns.
        ("sh: fd: killed by signal 2", "\x04"),
    ])

    print(out)
    want = [
        "user     : /sbin/init as pid 1 -- the userland session",
        "vfs      : procfs on /proc (generated, read-only)",
        "PrismOS init: starting the console shell",
        "PrismOS version",                    # /proc/version
        "/usr/bin/sh",                        # /proc/self/cmdline
        "(sh) S 1",                           # /proc/self/stat (comm, state, ppid)
        "cmdline",                            # /proc/1's listing
        "status", "exe",
        "root:x:0:0:root:/:/usr/bin/sh",      # /etc/passwd
        "    1     0 S", "    2     1 S",            # ps rows (init, sh)
        "/",                                  # pwd (the edited line)
        "test",                               # the erased-and-retyped echo
        "sh: write: /dev/full: No space left on device",
        "sh: fd: killed by signal 2",
    ]
    missing = [w for w in want if w not in out]

    # The respawn: ^D ended the shell, init logged it and brought it back.
    for w in ["init: shell exited (status 0), respawning",
              "PrismOS init: starting the console shell"]:
        # the banner must appear twice (boot + respawn)
        if out.count(w) < 2:
            missing.append("%s (twice)" % w)
    # And the session came back: a second-generation prompt.
    if out.count("prism$ ") < 3:
        missing.append("the respawned shell's prompt")

    if missing:
        print("init_drive: FAIL -- missing: %s" % ", ".join(missing))
        return 1
    print("init_drive: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
