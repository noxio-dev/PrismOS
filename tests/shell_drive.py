#!/usr/bin/env python3
import os
import select
import subprocess
import sys
import time

def run(image, cmds, timeout=60, extra=None):
    base = ["qemu-system-x86_64", "-m", "256M", "-display", "none", "-no-reboot",
            "-chardev", "stdio,id=ser,signal=off", "-serial", "chardev:ser",
            "-drive", f"file={image},format=raw"]
    if extra:
        base += extra
    proc = subprocess.Popen(base, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    out = b""
    pending = list(cmds)
    deadline = time.time() + timeout
    seen_prompt = 0  # count of prompts seen
    sent = 0
    last_data = time.time()
    try:
        while time.time() < deadline:
            r, _, _ = select.select([proc.stdout], [], [], 0.2)
            if r:
                chunk = os.read(proc.stdout.fileno(), 65536)
                if not chunk:
                    break
                out += chunk
                last_data = time.time()
            if b"prism# " in out and sent < len(pending):
                # shell is alive: send the next command after a short pause
                time.sleep(0.4)
                cmd = pending[sent]
                sent += 1
                proc.stdin.write((cmd + "\r").encode())
                proc.stdin.flush()
            elif sent >= len(pending) and time.time() - last_data > 1.5:
                break
    finally:
        try:
            proc.kill()
            proc.wait()
        except Exception:
            pass
    return out.decode("latin-1")

if __name__ == "__main__":
    image = sys.argv[1]
    cmds = sys.argv[2:]
    text = run(image, cmds)
    # Only show from the kernel banner on
    idx = text.find("PrismOS kernel")
    print(text[idx:] if idx >= 0 else text)
