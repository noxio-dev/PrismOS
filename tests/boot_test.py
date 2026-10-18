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

    hello = ["Hello, world!", "PrismOS kernel", "gdt/tss", "idt", "pic",
             "pit      : channel 0 at 100 Hz", "pmm      :", "vmm      :",
             "heap     :", "kbd      :", "interrupts enabled", "prism# "]
    scenarios = [
        # BIOS gets a VBE linear framebuffer now: the loader switches to it at the
        # point of no return and the kernel double-buffers its console.
        ("BIOS autoboot", bios_cmd, [], hello + ["firmware : BIOS", " framebuffer", "back buffer active"], 40),
        ("BIOS prompt: Esc, then boot", bios_cmd,
         [("Autoboot in", "\x1b"), ("OK ", "version\rboot -v\r")],
         ["OK ", "PrismOS loader 0.1.0", "Hello, world!", 'cmdline  : "-v"'], 40),
        ("BIOS shell: exception recovery", bios_cmd,
         [("prism# ", "crash int\r"), ("prism# ", "crash page\r"),
          ("prism# ", "crash div\r"), ("prism# ", "uptime\r")],
         ["handled, continuing", "page mapped, continuing", "divide by zero",
          "survived", "ticks at 100 Hz"], 60),
        ("BIOS shell: memory + layout", bios_cmd,
         [("prism# ", "meminfo\r"), ("prism# ", "layout fr\r"), ("prism# ", "kbd\r")],
         ["pages total", "self-test: ok", "switched to fr", "i8042"], 60),
        # Phase 2: ring 3 processes, context switch, preemptive scheduler
        # (time slices, priorities, sleep/wakeup), watchdog.
        ("BIOS shell: ring 3 processes + scheduler (phase 2.3)", bios_cmd,
         [("prism# ", "user ping pong\r"), ("prism# ", "user spin:3 sleep:0\r"),
          ("prism# ", "usertest\r"), ("prism# ", "ps\r")],
         ["user     : syscall/sysret ready",
          "proc     : pcb + 16 KiB kernel stacks per process",
          "[ring 3] ping", "[ring 3] pong",
          "pid 1 ping", "pid 2 pong",
          "pid 3 spin", "pid 4 sleep",
          "preempt  ok", "prio     ok", "wakeup   ok",
          "usertest: all 21 passed",
          "process table empty"], 240),
    ]
    if code and vars_ and os.path.exists(code):
        scenarios += [
            ("UEFI autoboot", uefi_cmd, [], hello + ["firmware : UEFI", " framebuffer", "back buffer active"], 90),
            ("UEFI prompt: Esc, then boot", uefi_cmd,
             [("Autoboot in", "\x1b"), ("OK ", "lsmem\rboot -v\r")],
             ["OK ", "usable", "Hello, world!", 'cmdline  : "-v"'], 90),
        ]
    else:
        print("note: OVMF not found, skipping UEFI scenarios")

    # ext4 + VFS: a second disk on an AHCI controller, driven from the shell.
    data = os.path.join(tmp, "data.img")
    have_fs = shutil.which("mke2fs") is not None
    if have_fs:
        with open(data, "wb") as f:
            f.truncate(32 * 1024 * 1024)
        seed = os.path.join(tmp, "seed")
        os.makedirs(seed)
        with open(os.path.join(seed, "hello.txt"), "w") as f:
            f.write("Hello from the PrismOS ext4 data disk.\n")
        subprocess.run(["mke2fs", "-q", "-F", "-t", "ext4", "-d", seed, data], check=True)

        def fs_cmd():
            return base + ["-drive", "file=%s,format=raw" % work,
                           "-device", "ahci,id=ahci0",
                           "-drive", "id=data,file=%s,format=raw,if=none" % data,
                           "-device", "ide-hd,drive=data,bus=ahci0.0"]

        scenarios.append(
            ("BIOS shell: ext4 on AHCI (VFS)", fs_cmd,
             [("prism# ", "cat /hello.txt\r"), ("prism# ", "mkdir /d\r"),
              ("prism# ", "write /d/t.txt persisted by PrismOS\r"), ("prism# ", "cat /d/t.txt\r"),
              ("prism# ", "ls /d\r"), ("prism# ", "df\r"), ("prism# ", "sync\r")],
             ["vfs      : disk 0 on / (ext4", "Hello from the PrismOS ext4 data disk.",
              "persisted by PrismOS", "t.txt", "mounted on", "synced"], 60))
    else:
        print("note: mke2fs not found, skipping the ext4 scenario")

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
    if have_fs:
        # What the guest wrote must be readable (and consistent) on the host.
        r = subprocess.run(["e2fsck", "-fn", data], capture_output=True, text=True)
        d = subprocess.run(["debugfs", "-R", "cat /d/t.txt", data], capture_output=True, text=True)
        if r.returncode != 0 or "persisted by PrismOS" not in d.stdout:
            failed += 1
            print("FAIL  ext4 image after the guest ran\n" + r.stdout + d.stdout)
        else:
            print("ok    ext4 image clean and /d/t.txt readable on the host")
    shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
