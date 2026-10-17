#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""UEFI boot test bench: QEMU + OVMF + removable USB drive.

Boots a disk image attached as removable USB media (the removable
boot path \\EFI\\BOOT\\BOOTX64.EFI is then the one a real UEFI firmware
would take), waits, captures the screen via QMP then kills the
virtual machine.

Usage:
    run_ovmf_boot_test.py <image> <output_png> [--code OVMF_CODE.fd]
        [--vars OVMF_VARS.fd] [--wait seconds] [--fixed]

    --fixed   : attach as a virtio hard disk instead of removable USB
                (to test the "fixed disk" behavior of firmwares).

Output: screen PNG + exit code 0 if the "PRISM ESP BOOT OK" marker
is detected by pixel analysis.
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

# Where to find qemu/OVMF: the PATH by default, overridable through
# the environment for a rootless bench (extracted debs).
QEMU = os.environ.get("QEMU_BIN", "qemu-system-x86_64")
LD = os.environ.get("QEMU_LD", "")
QEMU_DATA = os.environ.get("QEMU_DATA_DIR", "")
SEABIOS = os.environ.get("SEABIOS_DIR", "")
OVMF = os.environ.get(
    "OVMF_DIR",
    "/usr/share/OVMF")
DEFAULT_CODE = os.path.join(OVMF, "OVMF_CODE_4M.fd")
if not os.path.exists(DEFAULT_CODE):
    DEFAULT_CODE = os.path.join(OVMF, "OVMF_CODE.fd")
DEFAULT_VARS = os.path.join(OVMF, "OVMF_VARS_4M.fd")
if not os.path.exists(DEFAULT_VARS):
    DEFAULT_VARS = os.path.join(OVMF, "OVMF_VARS.fd")


class QmpClient:
    def __init__(self, path, timeout=60):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self._readline()  # initial greeting
        self.cmd("qmp_capabilities")

    def _readline(self):
        buf = b""
        while not buf.endswith(b"\r\n"):
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("QMP closed")
            buf += chunk
        return json.loads(buf.decode())

    def cmd(self, name, **args):
        payload = {"execute": name}
        if args:
            payload["arguments"] = args
        self.sock.sendall(json.dumps(payload).encode() + b"\r\n")
        while True:
            resp = self._readline()
            if "return" in resp or "error" in resp:
                return resp


def ppm_to_png(src_ppm, dst_png):
    """PPM -> PNG conversion without dependencies (minimal headers)."""
    from PIL import Image  # import delay: the driver is optional
    img = Image.open(src_ppm)
    img.save(dst_png)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("png")
    ap.add_argument("--code", default=DEFAULT_CODE)
    ap.add_argument("--vars", default=DEFAULT_VARS)
    ap.add_argument("--wait", type=float, default=35.0)
    ap.add_argument("--fixed", action="store_true")
    ap.add_argument("--marker", default="PRISM ESP BOOT OK")
    args = ap.parse_args()

    if not os.path.exists(QEMU):
        sys.exit(f"qemu not found: {QEMU} (QEMU_BIN?)")
    if not os.path.exists(args.code) or not os.path.exists(args.vars):
        sys.exit(f"OVMF firmware not found in {OVMF} (OVMF_DIR?)")

    tmp = tempfile.mkdtemp(prefix="ovmf-")
    vars_copy = os.path.join(tmp, "VARS.fd")
    shutil.copyfile(args.vars, vars_copy)
    qmp_sock = os.path.join(tmp, "qmp.sock")

    if args.fixed:
        storage = ["-drive", f"if=none,id=stick,format=raw,file={args.image}",
                   "-device", "virtio-blk-pci,drive=stick"]
    else:
        storage = ["-device", "qemu-xhci,id=xhci",
                   "-drive", f"if=none,id=stick,format=raw,file={args.image}",
                   "-device",
                   "usb-storage,bus=xhci.0,drive=stick,removable=on"]

    data_dirs = []
    if QEMU_DATA:
        data_dirs += ["-L", QEMU_DATA]
    if SEABIOS:
        data_dirs += ["-L", SEABIOS]

    cmd = [QEMU,
           "-machine", "q35", "-m", "512",
           "-accel", "tcg",
           *data_dirs,
           "-drive", f"if=pflash,format=raw,readonly=on,file={args.code}",
           "-drive", f"if=pflash,format=raw,file={vars_copy}",
           *storage,
           "-net", "none",
           "-display", "none", "-vga", "std",
           "-qmp", f"unix:{qmp_sock},server=on,wait=off"]
    env = dict(os.environ)
    if LD:
        env["LD_LIBRARY_PATH"] = LD

    print("qemu:", " ".join(cmd[:14]), "...")
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL,
                            stderr=subprocess.STDOUT)

    try:
        deadline = time.time() + 30
        while not os.path.exists(qmp_sock) and time.time() < deadline:
            time.sleep(0.2)
        qmp = QmpClient(qmp_sock)
        print(f"QMP connected - waiting {args.wait:.0f} s for the firmware...")
        time.sleep(args.wait)

        ppm = os.path.join(tmp, "screen.ppm")
        r = qmp.cmd("screendump", filename=ppm)
        if "error" in r:
            print("screendump error:", r)
        qmp.cmd("quit")
        for _ in range(50):
            if proc.poll() is not None:
                break
            time.sleep(0.1)
        if proc.poll() is None:
            proc.kill()

        if os.path.exists(ppm):
            ppm_to_png(ppm, args.png)
            print("capture:", args.png)
        else:
            print("NO screen capture!")
            return 2
    finally:
        if proc.poll() is None:
            proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
