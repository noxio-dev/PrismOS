#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Automated boot tests: run the disk image under QEMU (legacy BIOS, then UEFI
with OVMF) and drive the loader through the serial console.

    tests/boot_test.py build/prismos.img [qemu] [ovmf_code] [ovmf_vars]

Each scenario starts a fresh VM, optionally sends keystrokes when a given
text shows up, and succeeds when every expected string has been seen before
the timeout.
"""

import os
import select
import shutil
import subprocess
import sys
import tempfile
import time


def run_vm(cmd, steps, expect, timeout):
    """steps: list of (wait_for, send); expect: strings that must appear."""
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
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
                time.sleep(0.3)
                proc.stdin.write(send.encode())
                proc.stdin.flush()
            if all(e.encode() in out for e in expect):
                break
    finally:
        proc.kill()
        proc.wait()
    text = out.decode("latin-1")
    missing = [e for e in expect if e not in text]
    return text, missing


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    image = sys.argv[1]
    qemu = sys.argv[2] if len(sys.argv) > 2 else "qemu-system-x86_64"
    code = sys.argv[3] if len(sys.argv) > 3 else ""
    vars_ = sys.argv[4] if len(sys.argv) > 4 else ""

    base = [qemu, "-m", "256M", "-display", "none", "-no-reboot",
            "-chardev", "stdio,id=ser,signal=off", "-serial", "chardev:ser"]
    tmp = tempfile.mkdtemp()
    work = os.path.join(tmp, "disk.img")

    def bios_cmd():
        shutil.copyfile(image, work)
        return base + ["-drive", "file=%s,format=raw" % work]

    def uefi_cmd():
        shutil.copyfile(image, work)
        v = os.path.join(tmp, "vars.fd")
        shutil.copyfile(vars_, v)
        return base + ["-drive", "if=pflash,format=raw,readonly=on,file=" + code,
                       "-drive", "if=pflash,format=raw,file=" + v,
                       "-drive", "file=%s,format=raw" % work]

    hello = ["Hello, world!", "PrismOS kernel"]
    scenarios = [
        ("BIOS autoboot", bios_cmd, [], hello + ["firmware : BIOS"], 40),
        ("BIOS prompt: Esc, then boot", bios_cmd,
         [("Autoboot in", "\x1b"), ("OK ", "version\rboot -v\r")],
         ["OK ", "PrismOS loader 0.1.0", "Hello, world!", 'cmdline  : "-v"'], 40),
    ]
    if code and vars_ and os.path.exists(code):
        scenarios += [
            ("UEFI autoboot", uefi_cmd, [], hello + ["firmware : UEFI"], 90),
            ("UEFI prompt: Esc, then boot", uefi_cmd,
             [("Autoboot in", "\x1b"), ("OK ", "lsmem\rboot -v\r")],
             ["OK ", "usable", "Hello, world!", 'cmdline  : "-v"'], 90),
        ]
    else:
        print("note: OVMF not found, skipping UEFI scenarios")

    failed = 0
    for name, cmd, steps, expect, timeout in scenarios:
        start = time.time()
        text, missing = run_vm(cmd(), steps, expect, timeout)
        if missing:
            failed += 1
            print("FAIL  %s  (missing: %s)" % (name, ", ".join(missing)))
            print("---- serial output ----")
            print(text[-3000:])
            print("-----------------------")
        else:
            print("ok    %s  (%.1fs)" % (name, time.time() - start))
    shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
