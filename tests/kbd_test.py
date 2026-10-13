#!/usr/bin/env python3
import os
import select
import socket
import subprocess
import sys
import time

MON = "/tmp/kbdtest-mon.sock"


def main():
    image = sys.argv[1]
    proc = subprocess.Popen(
        ["qemu-system-x86_64", "-m", "256M", "-display", "none", "-no-reboot",
         "-monitor", f"unix:{MON},server,nowait",
         "-chardev", "stdio,id=ser,signal=off", "-serial", "chardev:ser",
         "-drive", f"file={image},format=raw"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    out = b""
    # Connect to the monitor
    deadline = time.time() + 60
    mon = None
    while time.time() < deadline:
        if os.path.exists(MON):
            try:
                mon = socket.socket(socket.AF_UNIX)
                mon.connect(MON)
                mon.settimeout(0.3)
                break
            except OSError:
                pass
        r, _, _ = select.select([proc.stdout], [], [], 0.2)
        if r:
            out += os.read(proc.stdout.fileno(), 65536)
        if b"prism# " in out:
            pass
    if mon is None:
        print("FAIL: no monitor connection")
        proc.kill()
        return 1

    def drain_mon():
        try:
            while True:
                d = mon.recv(65536)
                if not d:
                    break
        except socket.timeout:
            pass

    def sendkey(keys):
        mon.sendall(f"sendkey {keys}\n".encode())
        time.sleep(0.35)
        drain_mon()

    def serial_read():
        got = b""
        end = time.time() + 1.0
        while time.time() < end:
            r, _, _ = select.select([proc.stdout], [], [], 0.1)
            if r:
                d = os.read(proc.stdout.fileno(), 65536)
                if not d:
                    break
                got += d
        return got

    # Wait for the shell prompt
    while time.time() < deadline and b"prism# " not in out:
        r, _, _ = select.select([proc.stdout], [], [], 0.3)
        if r:
            out += os.read(proc.stdout.fileno(), 65536)
    time.sleep(0.5)
    serial_read()

    results = []
    def check(name, keys, expect):
        before = serial_read()
        sendkey(keys)
        # newline so the echo lands on its own line
        sendkey("ret")
        after = serial_read()
        text = (before + after).decode("latin-1")
        lines = [l.strip() for l in text.splitlines()]
        ok = any(l == "prism# " + expect or l.endswith(expect) for l in lines)
        results.append((name, ok, keys, expect, text.strip()[-60:]))
        return ok

    # 1. US layout: the backslash key (right of ]) -> '\'
    check("us backslash", "backslash", "\\")
    # 2. US layout: shift+backslash -> '|'
    check("us pipe", "shift-backslash", "|")
    # 3. switch to FR layout via serial
    proc.stdin.write(b"layout fr\r")
    proc.stdin.flush()
    time.sleep(0.8)
    serial_read()
    # 4. FR layout: AltGr+8 -> backslash (right alt = AltGr)
    check("fr altgr+8 backslash", "alt_r-8", "\\")
    # 5. FR layout: shift+, -> '?' (the AZERTY question mark)
    check("fr question mark", "shift-comma", "?")
    # 6. FR layout: plain 'a' key position types 'q' (AZERTY)
    check("fr azerty a->q", "a", "q")

    print("\nkeyboard test results:")
    failed = 0
    for name, ok, keys, expect, seen in results:
        mark = "ok  " if ok else "FAIL"
        if not ok:
            failed += 1
        print(f"  {mark} {name:26s} sendkey={keys:20s} expect={expect!r:14s} tail={seen!r}")

    try:
        proc.kill()
        proc.wait()
    except Exception:
        pass
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
