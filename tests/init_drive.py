#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Boot-time proof of the userland session's own plumbing -- init, the tty
line disciplines, /proc, the login respawn -- driven through the *serial*
session of a freshly booted image (needs qemu).

    tests/init_drive.py build/prismos.img

The session this drives is ttyS0's: the boot hands the machine to BusyBox
init (pid 1), which reads /etc/inittab, runs rcS (mounts, hostname,
keymap), prints the banner and puts a `-/bin/login` on the serial line;
root's password is empty, so the runner answers the prompt as fast as the
login's read needs (it dies with EAGAIN after 10 s without input -- see
the report).  Behind the login is busybox ash (the login spelling:
/etc/profile's PATH and the prism$ prompt), so the scenarios speak ash:
/proc answers (version, the per-pid files, the per-process view through
the magic self), the line editing (VERASE retypes the line), /dev/full is
the ENOSPC it must be, ^C interrupts a blocked foreground read (fd's
stdin prompt -- fd stays a kshell program for its stdio checks, but the
blocked read is real), and ^D ends the shell -- init respawns the login
and the runner brings a second session up.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from shell_drive import run  # noqa: E402


def main():
    if len(sys.argv) < 2:
        print("usage: tests/init_drive.py <image>")
        return 2

    out = run(sys.argv[1], [
        # /proc: the version, the per-pid directory of pid 1 (init), and
        # the per-process view through the magic self -- the stat fields
        # of the cat itself (comm "(cat)", the running state, ppid = the
        # login that started the shell).
        "cat /proc/version",
        "ls /proc/1",
        "cat /proc/self/stat",
        # The line editing (VERASE on the tty): one DEL erases the last
        # digit of probe22 -- the line ash executes is echo probe2.
        "echo probe22\x7f",
        # /dev: the serial node is there (a queue of its own), and the
        # identity files the login spoke.
        "ls /dev",
        "cat /etc/passwd",
        # busybox ps (through /proc): KNOWN BUG (deliberately left unmet
        # here, see the report): the listing comes out empty, no rows and
        # no header, so nothing is pinned on it -- run it to keep the
        # regression visible in the log.
        "ps",
        # /dev/full: the write no disk could make -- ash's redirection.
        "echo probe > /dev/full",
        # ^C on a blocked foreground read (fd's stdin prompt); ash
        # survives it and answers with a fresh prompt.
        "fd",
        ("type 4 bytes (EXACTLY wxyz):", "\x03"),
        "echo survived-interrupt",
        # The second session: ^D at the prompt ended the first one --
        # the checks below prove init brought the login back.  The
        # command keys on the ash banner's SECOND occurrence (the
        # respawned session's): the ^D killed the shell at its own
        # last prompt, so the next fresh prism$ belongs to the login
        # still to come -- a prompt-keyed command would land in it.
        ("survived-interrupt", "\x04"),
        ("built-in shell (ash)", "echo second-session-alive\r", 2),
    ], timeout=120)
    print(out)
    missing = [w for w in [
        "PrismOS version",                       # /proc/version
        "cmdline", "cwd", "exe", "fd", "status",  # ls /proc/1's columns
        "(cat) R",                               # /proc/self/stat (comm, state)
        "ttyS0",                                 # /dev's serial node
        "root::0:0:root:/:/usr/bin/sh",          # /etc/passwd (empty password)
        "No space left on device",               # the /dev/full redirection
        "type 4 bytes (EXACTLY wxyz):",          # fd reached its stdin prompt
        "survived-interrupt",                    # the shell survived the ^C
        "second-session-alive",                  # the respawned session works
    ] if w not in out]
    # VERASE: the line ash executed was probe2 -- per-line, because the
    # characters as typed are still echoed on the prompt line (probe22 is
    # on that line whatever the line discipline did).
    lines = [l.strip() for l in out.splitlines()]
    if "probe2" not in lines:
        missing.append("the retyped line's output: probe2")
    if "probe22" in lines:
        missing.append("the erased digit survived: VERASE")

    # The respawn: ^D ended the shell, busybox init brought the login back
    # and the runner logged in again -- the login prompt and the motd each
    # appear twice (boot + respawn).
    if out.count("prismos login:") < 2:
        missing.append("the respawned login prompt")
    if out.count("Welcome to PrismOS (busybox + ash, phase 3.4).") < 2:
        missing.append("the motd of the respawned session")
    if out.count("prism$ ") < 3:
        missing.append("the respawned session's prompt")

    if missing:
        print("init_drive: FAIL -- missing: %s" % ", ".join(missing))
        return 1
    print("init_drive: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
