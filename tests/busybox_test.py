#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Host tests for the busybox port (phase 3.4).

    tests/busybox_test.py [-k]

The cross-built busybox of build/busybox-out is a PrismOS static PIE;
prism_emu.c (the ptrace runner that maps it at USER_BASE, applies the
R_X86_64_RELATIVE relocations, builds the SysV stack and answers the
PrismOS syscall ABI) executes it on this host, on the kernel's own stack
size.  The judge is a *host* busybox built from the very same tarball and
the very same committed config (ports/busybox/busybox-1.37.0-prismos.config,
cached under build/busybox-test/host) -- the same source, the same applets,
the only difference the libc underneath: the arrangement binutils_test.py
has with the host's binutils 2.44, and fs_test.py with e2fsck.

What runs here is the applet surface a single process covers -- exactly the
half the emulator can honestly run (fork / execve / wait stay ENOSYS under
it, so the shell itself is the QEMU scenario's business, boot_test.py's
busybox step):

  - the text paths: echo, printf, cat, wc, head, tail, grep, sed, awk
    (math included -- mlibc's musl-derived libm inside libc.a), uname,
    id / whoami, basename / dirname / pwd -- text for text with the host
  - the file paths: mkdir / rmdir / rm, cp and mv (content byte for
    byte), ln -s + readlink, touch, chmod then ls -l (the whole line,
    dates and all -- the same files stat'd by both sides), ls itself
    (the sorted walk, getdents through mlibc's readdir)
  - tar: archives byte for byte (deterministic: same members, same
    order, same mtimes), and `tar tf` listings text for text
  - the mounts: `mount`'s listing (mlibc's mntent over /proc/mounts,
    text for text) and `df`'s row for the filesystem the test runs on
    (the numbers drift with the live disk -- the shape is the check)
  - the failure paths: cat on a missing file exits 1 with the errno
    text, ls on a missing path ditto

Needs: a C compiler for the emulator, make for the host busybox, and the
staged binary (`make busybox`).  -k keeps the temp dir.
"""
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
EMU_SRC = os.path.join(REPO, "prism_emu.c")
TESTDIR = os.path.join(REPO, "build", "busybox-test")
EMU_BIN = os.path.join(TESTDIR, "prism_emu")
OUT = os.path.join(REPO, "build", "busybox-out")
BB = os.path.join(OUT, "busybox")
HOSTDIR = os.path.join(TESTDIR, "host")
HOSTBB = os.path.join(HOSTDIR, "busybox")
TARBALL = os.path.join(REPO, "build", "toolchain-src", "busybox-1.37.0.tar.bz2")
CONFIG = os.path.join(REPO, "ports", "busybox", "busybox-1.37.0-prismos.config")
ALARM = "60"

T_TXT = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\n"

keep = "-k" in sys.argv
failures = []
passes = 0


def run(cmd, **kw):
    kw.setdefault("timeout", 300)
    env = dict(os.environ, PRISM_EMU_ALARM=ALARM)
    p = subprocess.run(cmd, capture_output=True, text=True, env=env, **kw)
    return p.returncode, p.stdout, p.stderr


def check(name, ok, detail=""):
    global passes
    if ok:
        passes += 1
        print("  ok   %s" % name)
    else:
        failures.append(name)
        print("  FAIL %s%s" % (name, (" -- " + detail) if detail else ""))


def emu(args, cwd):
    """The PrismOS busybox under the emulator; returns (rc, out, err)."""
    return run([EMU_BIN, BB] + list(args), cwd=cwd)


def host(args, cwd):
    """The host busybox -- same tarball, same config, glibc underneath."""
    return run([HOSTBB] + list(args), cwd=cwd)


def normal(err):
    """Drop the emulator's own chatter and mlibc's missing-sysdep warnings
    (the port's contract: what the kernel lacks stays ENOSYS)."""
    skip = ("emu: ", "sysdep_or_enosys", "__ensure", "In function mlibc::",
            "missing sysdep")
    return "\n".join(l for l in err.splitlines() if not any(s in l for s in skip))


def both(name, args, cwd, want_rc=True):
    """Run the applet on both sides, text for text."""
    r1, o1, e1 = emu(args, cwd)
    r2, o2, e2 = host(args, cwd)
    ok = r1 == r2 and o1 == o2 and (r1 == 0) == want_rc
    check(name, ok, "prismos rc=%d %r / host rc=%d %r%s"
          % (r1, o1[:80], r2, o2[:80],
             "" if want_rc else " (expected nonzero)"))
    return r1, o1


def build_host_busybox():
    """The judge: the same tarball, the same committed config, native."""
    if os.path.isfile(HOSTBB):
        return True
    if not os.path.isfile(TARBALL):
        print("busybox_test: %s missing -- run `make busybox` first" % TARBALL)
        return False
    if not os.path.isdir(HOSTDIR):
        os.makedirs(HOSTDIR)
        r, _, err = run(["tar", "xf", TARBALL, "-C", HOSTDIR,
                         "--strip-components=1"])
        if r:
            print("busybox_test: cannot extract the tarball: %s" % err[-200:])
            return False
        shutil.copy(CONFIG, os.path.join(HOSTDIR, ".config"))
        r, _, err = run(["sh", "-c", "yes '' | make oldconfig"], cwd=HOSTDIR)
        if r:
            print("busybox_test: host config failed: %s" % err[-200:])
            return False
    r, _, err = run(["make", "-j2"], cwd=HOSTDIR)
    if r or not os.path.isfile(HOSTBB):
        print("busybox_test: host busybox failed: %s" % err[-300:])
        return False
    return True


def main():
    if not os.path.isfile(BB):
        sys.exit("busybox_test: %s missing -- run `make busybox` first" % BB)
    if not os.path.isfile(EMU_SRC):
        sys.exit("busybox_test: %s missing" % EMU_SRC)

    # ---- the emulator itself ------------------------------------------------
    print("== prism_emu (the PrismOS static-PIE runner) ==")
    os.makedirs(TESTDIR, exist_ok=True)
    rc, _, err = run(["gcc", "-O2", "-Wall", "-o", EMU_BIN, EMU_SRC])
    check("compiles", rc == 0, err.strip()[-200:] if rc else "")

    # ---- the judge: a host busybox of the same source and config -----------
    print("== the host busybox (same tarball, same config) ==")
    check("builds", build_host_busybox())

    tmp = tempfile.mkdtemp(prefix="prismos-busybox-")
    try:
        os.chdir(tmp)
        with open("t.txt", "w") as f:
            f.write(T_TXT)

        print("== the text paths ==")
        both("echo", ["echo", "hello from busybox"], tmp)
        both("printf", ["printf", "%s-%d-%x\\n", "word", "42", "255"], tmp)
        both("cat", ["cat", "t.txt"], tmp)
        both("wc -l", ["wc", "-l", "t.txt"], tmp)
        both("wc -w", ["wc", "-w", "t.txt"], tmp)
        both("head -n 3", ["head", "-n", "3", "t.txt"], tmp)
        both("tail -n 2", ["tail", "-n", "2", "t.txt"], tmp)
        both("grep", ["grep", "bravo", "t.txt"], tmp)
        both("grep -v", ["grep", "-v", "bravo", "t.txt"], tmp)
        both("grep -n", ["grep", "-n", "[bc]", "t.txt"], tmp)
        both("sed", ["sed", "s/bravo/BRAVO/", "t.txt"], tmp)
        both("awk fields", ["awk", "{print NR \": \" $1}", "t.txt"], tmp)
        both("awk math", ["awk", "BEGIN{print 6*7; printf \"%.3f\\n\", 1/8}"],
             tmp)
        both("uname -s", ["uname", "-s"], tmp)
        both("uname -m", ["uname", "-m"], tmp)
        both("id", ["id"], tmp)
        both("whoami", ["whoami"], tmp)
        both("basename", ["basename", "/usr/bin/sh"], tmp)
        both("dirname", ["dirname", "/usr/bin/sh"], tmp)
        both("pwd", ["pwd"], tmp)
        both("true", ["true"], tmp)
        both("false", ["false"], tmp, want_rc=False)

        print("== the file paths (side effects compared) ==")
        r, _, _ = emu(["mkdir", "-p", "d1/d2"], tmp)
        check("mkdir -p", r == 0 and os.path.isdir("d1/d2"))
        r, _, _ = emu(["touch", "d1/d2/x"], tmp)
        check("touch", r == 0 and os.path.isfile("d1/d2/x"))
        r, _, _ = emu(["cp", "t.txt", "d1/d2/copy"], tmp)
        r2, _, _ = host(["cp", "t.txt", "d1/d2/copy-host"], tmp)
        same = r == 0 and r2 == 0 and \
            open("d1/d2/copy", "rb").read() == open("d1/d2/copy-host", "rb").read()
        check("cp (content byte for byte)", same)
        r, _, _ = emu(["mv", "d1/d2/copy", "d1/moved"], tmp)
        r2, _, _ = host(["mv", "d1/d2/copy-host", "d1/moved-host"], tmp)
        same = r == 0 and r2 == 0 and \
            open("d1/moved", "rb").read() == open("d1/moved-host", "rb").read()
        check("mv (content byte for byte)", same)
        r, _, _ = emu(["ln", "-s", "t.txt", "d1/link"], tmp)
        check("ln -s", r == 0 and os.path.islink("d1/link"))
        both("readlink", ["readlink", "d1/link"], tmp)
        both("ls (the sorted walk)", ["ls"], tmp)
        both("ls d1", ["ls", "d1"], tmp)
        # the whole line, dates and all: both sides stat the same files
        emu(["chmod", "640", "t.txt"], tmp)
        host(["chmod", "640", "t.txt"], tmp)   # the same mode, both worlds
        both("ls -l after chmod", ["ls", "-l", "t.txt"], tmp)
        r, _, _ = emu(["rm", "d1/moved"], tmp)
        check("rm", r == 0 and not os.path.exists("d1/moved"))
        r, _, _ = emu(["rm", "d1/d2/x"], tmp)
        check("rm (emptying the directory)", r == 0 and not os.path.exists("d1/d2/x"))
        r, _, _ = emu(["rmdir", "d1/d2"], tmp)
        check("rmdir", r == 0 and not os.path.exists("d1/d2"))
        # and the honest refusal: a non-empty directory stays
        r, _, e = emu(["rmdir", "d1"], tmp)
        check("rmdir on a non-empty directory refuses",
              r != 0 and "d1" in normal(e) and "empty" in normal(e),
              "rc=%d %r" % (r, normal(e)[:60]))

        print("== tar (deterministic archives, byte for byte) ==")
        r1, _, _ = emu(["tar", "cf", "pr.tar", "t.txt", "d1"], tmp)
        r2, _, _ = host(["tar", "cf", "host.tar", "t.txt", "d1"], tmp)
        check("tar cf exits 0", r1 == 0 and r2 == 0)
        check("archive identical to the host's",
              open("pr.tar", "rb").read() == open("host.tar", "rb").read())
        both("tar tf", ["tar", "tf", "pr.tar"], tmp)

        print("== the mounts (mntent + statvfs through the libc) ==")
        r1, o1, _ = emu(["mount"], tmp)
        r2, o2, _ = host(["mount"], tmp)
        check("mount listing text for text", r1 == 0 and r2 == 0 and o1 == o2,
              "prismos: %d lines, host: %d lines"
              % (len(o1.splitlines()), len(o2.splitlines())))
        r1, o1, _ = emu(["df", "."], tmp)
        r2, o2, _ = host(["df", "."], tmp)
        # the live disk drifts between the two runs: the shape is the check
        shape = lambda s: [l.split()[0] for l in s.splitlines()[1:]]
        check("df row for this filesystem", r1 == 0 and r2 == 0
              and shape(o1) == shape(o2) and "%" in o1,
              "prismos: %r host: %r" % (o1[:60], o2[:60]))

        print("== the failure paths (errno honestly reported) ==")
        r, o, e = emu(["cat", "no-such-file"], tmp)
        check("cat on a missing file exits 1",
              r == 1 and "No such file" in normal(e),
              "rc=%d %r" % (r, normal(e)[:80]))
        r, o, e = emu(["ls", "no-such-dir"], tmp)
        check("ls on a missing path exits nonzero (and says so)",
              r != 0 and "No such file" in (o + normal(e)),
              "rc=%d" % r)
    finally:
        os.chdir(REPO)
        if keep:
            print("tmp: %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    print("busybox_test: %d ok, %d failed" % (passes, len(failures)))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
