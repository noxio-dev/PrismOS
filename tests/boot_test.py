#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Automated boot tests: run the disk image under QEMU (legacy BIOS, then UEFI
with OVMF) and drive the loader through the serial console.

    tests/boot_test.py build/prismos.img [qemu] [ovmf_code] [ovmf_vars]

Each scenario starts a fresh VM, optionally sends keystrokes when a given
text shows up, and succeeds when every expected string has been seen before
the timeout.  The boot disk always sits on AHCI port 0 so the kernel can
find its ext4 root partition (P3) on its AHCI driver.
"""

import os
import select
import shutil
import struct
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
    fswork = os.path.join(tmp, "fsdisk.img")

    def ahci_boot(disk):
        """the bootable disk on AHCI port 0 (where the kernel looks for P3)"""
        return ["-device", "ahci,id=ahci0",
                "-drive", "id=boot,file=%s,format=raw,if=none" % disk,
                "-device", "ide-hd,drive=boot,bus=ahci0.0"]

    def bios_cmd():
        shutil.copyfile(image, work)
        return base + ahci_boot(work)

    # an extra bare ext4 disk (no boot tree): the loader must skip it and the
    # kernel must still pick the boot disk's P3 as its root
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

    def data_drive():
        return ["-drive", "id=data,file=%s,format=raw,if=none" % data,
                "-device", "ide-hd,drive=data,bus=ahci0.1"]

    def uefi_cmd():
        shutil.copyfile(image, work)
        v = os.path.join(tmp, "vars.fd")
        shutil.copyfile(vars_, v)
        cmd = base + ["-drive", "if=pflash,format=raw,readonly=on,file=" + code,
                      "-drive", "if=pflash,format=raw,file=" + v] + ahci_boot(work)
        if have_fs:
            cmd += data_drive()
        return cmd

    hello = ["Hello, world!", "PrismOS kernel", "gdt/tss", "idt", "pic",
             "pit      : channel 0 at 100 Hz", "pmm      :", "vmm      :",
             "heap     :", "kbd      :", "interrupts enabled", "prism# "]
    scenarios = [
        # BIOS gets a VBE linear framebuffer now: the loader switches to it at the
        # point of no return and the kernel double-buffers its console.
        ("BIOS autoboot", bios_cmd, [], hello + ["firmware : BIOS", " framebuffer", "back buffer active"], 40),
        ("BIOS prompt: Esc, then boot", bios_cmd,
         [("Autoboot in", "\x1b"), ("OK ", "version\rboot -v\r")],
         ["OK ", "PrismOS loader (bios, Lua 5.4.8)", "Hello, world!", 'cmdline  : "-v"'], 40),
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

    # The loader reads its boot tree from the ext4 root partition (P3) and the
    # kernel mounts the same partition as / -- the whole disk chain (boot0 ->
    # boot1 -> loader -> kernel) runs off one ext4 volume.
    def fs_cmd():
        shutil.copyfile(image, fswork)
        return base + ahci_boot(fswork)

    scenarios.append(
        ("BIOS shell: ext4 root partition (P3, read-write)", fs_cmd,
         [("prism# ", "cat /boot/loader.conf\r"),
          ("prism# ", "mkdir /d\r"),
          ("prism# ", "write /d/t.txt persisted by PrismOS\r"), ("prism# ", "cat /d/t.txt\r"),
          ("prism# ", "ls /d\r"), ("prism# ", "df\r"), ("prism# ", "sync\r")],
         ["vfs      : disk 0 partition 3 on / (ext4",
          "PrismOS loader configuration",
          "persisted by PrismOS", "t.txt", "mounted on", "synced"], 90))

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

    # ---- image layout (no VM needed) -------------------------------------
    # GPT: P1 ESP carrying only the UEFI stub, P2 BIOS boot partition with
    # boot1 at its start, P3 Linux ext4 root.
    why = layout_problem(image, tmp)
    if why:
        failed += 1
        print("FAIL  image layout: %s" % why)
    else:
        print("ok    image layout: P1 ESP (stub only) / P2 BIOS boot / P3 ext4 root")

    # What the guest wrote must be readable (and consistent) on the host.
    if os.path.exists(fswork) and shutil.which("e2fsck"):
        rootimg = extract_part(fswork, 3, os.path.join(tmp, "root.img"))
        r = subprocess.run(["e2fsck", "-fn", rootimg], capture_output=True, text=True)
        d = subprocess.run(["debugfs", "-R", "cat /d/t.txt", rootimg], capture_output=True, text=True)
        if r.returncode != 0 or "persisted by PrismOS" not in d.stdout:
            failed += 1
            print("FAIL  ext4 root after the guest ran\n" + r.stdout + d.stdout)
        else:
            print("ok    ext4 root clean and /d/t.txt readable on the host")
    shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(1 if failed else 0)


# ---------------------------------------------------------------- helpers

TYPE_ESP = bytes.fromhex("28732ac11ff8d211ba4b00a0c93ec93b")        # C12A7328-...
TYPE_BIOS_BOOT = bytes.fromhex("4861682149646f6e744e656564454649")  # 21686148-...
TYPE_LINUX = bytes.fromhex("af3dc60f838472478e793d69d8477de4")      # 0FC63DAF-...


def gpt_entries(image):
    """[(type_guid bytes, first_lba, last_lba)] of the first four entries."""
    with open(image, "rb") as f:
        f.seek(512)
        hdr = f.read(512)
        if hdr[:8] != b"EFI PART":
            return None
        out = []
        for i in range(4):
            e = f.read(128)
            out.append((e[0:16], struct.unpack("<QQ", e[32:48])))
    return out


def extract_part(image, n, out_path):
    """Dump GPT partition n (1-based) to a plain file; returns the path."""
    ents = gpt_entries(image)
    (t, (first, last)) = ents[n - 1]
    with open(image, "rb") as f:
        f.seek(first * 512)
        blob = f.read((last - first + 1) * 512)
    with open(out_path, "wb") as f:
        f.write(blob)
    return out_path


def layout_problem(image, tmp):
    ents = gpt_entries(image)
    if not ents:
        return "no GPT header"
    if ents[0][0] != TYPE_ESP:
        return "P1 is not an EFI System Partition"
    if ents[1][0] != TYPE_BIOS_BOOT:
        return "P2 is not the BIOS boot partition"
    if ents[2][0] != TYPE_LINUX:
        return "P3 is not a Linux filesystem partition"
    # boot1 must sit at the start of P2 (its magic and patched header)
    with open(image, "rb") as f:
        f.seek(ents[1][1][0] * 512)
        b1 = f.read(16)
    if b1[:4] != b"PRB1":
        return "no boot1 at the start of P2"
    if struct.unpack("<Q", b1[4:12])[0] < ents[1][1][0]:
        return "boot1 loader LBA outside P2"
    # the ESP must carry only the UEFI stub (no boot tree)
    if shutil.which("mdir"):
        esp = extract_part(image, 1, os.path.join(tmp, "esp.img"))
        root = subprocess.run(["mdir", "-i", esp, "::/"], capture_output=True, text=True)
        if "EFI" not in root.stdout or "boot" in root.stdout.lower():
            return "ESP is not minimal (boot tree on P1?)"
        stub = subprocess.run(["mdir", "-i", esp, "::/EFI/BOOT"], capture_output=True, text=True)
        if "BOOTX64" not in stub.stdout:
            return "no EFI/BOOT/BOOTX64.EFI on the ESP"
    return None


if __name__ == "__main__":
    main()
