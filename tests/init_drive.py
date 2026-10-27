#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Boot-time proof of phase 3.3's whole second half -- init, the tty line
disciplines, /proc, sessions -- driven through the *serial* session of a
freshly booted image (needs qemu).

    tests/init_drive.py build/prismos.img

The session this drives is /dev/ttyS0's (phase 3.3.5): the kernel found
a UART, init put a getty on it, and every keystroke below crosses the
serial line's own queue and line discipline.  Checks (roadmap 3.3.8):
the kernel starts /sbin/init as pid 1 and init mounts /proc and /tmp
(pid 1's mounts, phase 3.3.7); /proc answers (version, the per-pid
files, the cwd and fd symlinks, the magic self through stat); the line
edits (VERASE through the serial line) and ^C interrupts a blocked
foreground read of the fd program, reported by the shell as the kill;
/dev full is the ENOSPC it must be; ^D ends the shell and init brings
the *serial* session back; and the console session it also started is
visible in ps the whole time (sleeping at its VGA prompt).
"""
import os
import select
import subprocess
import sys
import time


def run_steps(image, steps, timeout=240):
    """steps: list of (wait_for, send) -- expect-style, control characters
    included.  Returns the whole serial output."""
    # The disk on AHCI port 0 like the Makefile's run target: the kernel
    # only speaks AHCI, a bare -drive lands on IDE and boots nothing.
    base = ["qemu-system-x86_64", "-m", "256M", "-display", "none", "-no-reboot",
            "-chardev", "stdio,id=ser,signal=off", "-serial", "chardev:ser",
            "-device", "ahci,id=ahci0",
            "-drive", "id=boot,file=%s,format=raw,if=none" % image,
            "-device", "ide-hd,drive=boot,bus=ahci0.0"]
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
                # Drain the tail: init paces the respawn with a sleep, so keep
                # reading for a few seconds instead of sleeping blind -- the
                # pipe would swallow the respawn banner otherwise.
                end = time.time() + 4.0
                while time.time() < end:
                    r, _, _ = select.select([proc.stdout], [], [], 0.2)
                    if r:
                        chunk = os.read(proc.stdout.fileno(), 65536)
                        if not chunk:
                            break
                        out += chunk
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
        ("(sh) R 1", "ls /proc/1\r"),
        ("fd", "ls /proc/self\r"),
        # The cwd and fd symlinks (phase 3.3.6): ls -l of the directory
        # shows the arrows through lstat + readlink.
        ("cwd", "ls -l /proc/self\r"),
        ("-> /usr/bin/sh", "ls /dev\r"),
        # /dev: ttyS0 is there, a queue of its own (phase 3.3.5).
        ("tty0", "cat /etc/passwd\r"),
        ("root:x:0:0:root:/:/usr/bin/sh", "ps\r"),
        # The tty's line discipline: VERASE (0x7f through the serial
        # line) fixes a typoed command, exactly like on a real terminal.
        ("    2     1 S", "pwdd\x7f\r"),           # "pwdd" + backspace -> pwd
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
        "init: /proc mounted (procfs, read-only)",  # pid 1's mount (3.3.7)
        "init: /tmp mounted (tmpfs)",
        "PrismOS init: starting the console shell",
        "PrismOS init: starting the serial shell (ttyS0)",
        "PrismOS version",                    # /proc/version
        "/usr/bin/sh",                        # /proc/self/cmdline
        "(sh) R 1",                           # /proc/self/stat (comm, state, ppid)
        "cmdline", "cwd", "exe", "fd", "stat", "status",  # /proc/N's listing
        "-> /usr/bin/sh", "-> /",            # ls -l's readlink of the pair
        "ttyS0",                              # /dev's serial node (3.3.5)
        "root:x:0:0:root:/:/usr/bin/sh",      # /etc/passwd
        # ps rows: init sleeps in waitpid, the console shell sleeps at its
        # VGA prompt (the session this test never touches), this shell runs.
        "    1     0 S", "    2     1 S", "    3     1 R",
        "/",                                  # pwd (the edited line)
        "test",                               # the erased-and-retyped echo
        "sh: write: /dev/full: No space left on device",
        "sh: fd: killed by signal 2",
    ]
    missing = [w for w in want if w not in out]

    # The respawn: ^D ended the shell, init logged it and brought it back.
    if out.count("init: serial shell exited (status 0), respawning") < 1:
        missing.append("init: serial shell exited (status 0), respawning")
    # the serial banner must appear twice (boot + respawn)
    if out.count("PrismOS init: starting the serial shell (ttyS0)") < 2:
        missing.append("PrismOS init: starting the serial shell (twice)")
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
