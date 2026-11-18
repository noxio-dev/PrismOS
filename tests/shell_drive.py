import os
import select
import subprocess
import sys
import time

LOGIN_PROMPT = "prismos login:"


def run(image, cmds, timeout=60, extra=None, prompt=b"prism$ "):
    """Drive the serial session of a freshly booted image: the login prompt
    is answered (root, no password) as fast as the login's read needs --
    its blocking read dies with EAGAIN after 10 s without input -- and each
    command goes out when a fresh occurrence of the shell prompt shows up,
    the echo the command's own proof of consumption.  A command may also be
    a (wait_for, raw_data[, nth]) tuple: the raw bytes (control characters
    included) go out as soon as wait_for's nth occurrence (default first)
    appears, prompt or not -- the nth matters when the wait token belongs
    to a session that a previous tuple's bytes just ended (e.g. the ash
    banner of the session a ^D respawned).  Returns the whole serial
    output as text."""
    # The disk on AHCI port 0 like the Makefile's run target: the kernel
    # only speaks AHCI, a bare -drive lands on IDE and boots nothing.
    base = ["qemu-system-x86_64", "-m", "256M", "-display", "none", "-no-reboot",
            "-chardev", "stdio,id=ser,signal=off", "-serial", "chardev:ser",
            "-device", "ahci,id=ahci0",
            "-drive", "id=boot,file=%s,format=raw,if=none" % image,
            "-device", "ide-hd,drive=boot,bus=ahci0.0"]
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
    login_sent = 0
    try:
        while time.time() < deadline:
            r, _, _ = select.select([proc.stdout], [], [], 0.2)
            if r:
                chunk = os.read(proc.stdout.fileno(), 65536)
                if not chunk:
                    break
                out += chunk
                last_data = time.time()
            # the login: every re-printed prompt gets one fast root (busybox
            # init respawns the login after the 10 s read death, and the
            # boot's own prompt is the first of them)
            seen_login = out.count(LOGIN_PROMPT.encode())
            if seen_login > login_sent:
                login_sent = seen_login
                time.sleep(0.2)
                proc.stdin.write(b"root\r")
                proc.stdin.flush()
            seen = out.count(prompt)
            if seen > seen_prompt and sent < len(pending) and \
                    isinstance(pending[sent], str):
                # a new prompt appeared: send the next command
                seen_prompt = seen
                time.sleep(0.4)
                cmd = pending[sent]
                sent += 1
                proc.stdin.write((cmd + "\r").encode())
                proc.stdin.flush()
            while sent < len(pending) and isinstance(pending[sent], tuple):
                # a raw step: wait for the nth occurrence of its token
                # (default first), then send the bytes as they are
                # (control characters included)
                tok = pending[sent][0].encode()
                nth = pending[sent][2] if len(pending[sent]) > 2 else 1
                if out.count(tok) < nth:
                    break
                step = pending[sent]
                sent += 1
                time.sleep(0.3)
                proc.stdin.write(step[1].encode())
                proc.stdin.flush()
            if sent >= len(pending) and time.time() - last_data > 1.5:
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
