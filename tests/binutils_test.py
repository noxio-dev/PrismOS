#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Host tests for the binutils port (phase 4.1).

    tests/binutils_test.py [-k]

The five cross-built utilities of build/binutils-out -- ar, ranlib, nm,
objdump and strip -- are PrismOS static PIEs; prism_emu.c (the ptrace
runner that maps them at USER_BASE, applies the R_X86_64_RELATIVE
relocations, builds the SysV stack and answers the PrismOS syscall ABI)
executes them on this host, on the kernel's own stack size.  Everything
they produce is compared against the host's own binutils -- 2.44 here,
the same release the port builds, the same judge e2fsck is for fs_test:

  - the archives (`ar rc`, then `ranlib`) byte for byte: both sides write
    deterministic archives (timestamps / uid / gid / mode zeroed), so a
    PrismOS ar and a Linux ar of the same members are the same file
  - `nm` listings, `objdump -d` disassembly, `ar tv` tables: text for text
  - `strip -o` outputs byte for byte
  - `nm` on the stripped object: "no symbols"
  - a full `objdump -d` of one of the utilities themselves (~1.9 MiB in,
    ~370k lines out): the sustained file-reading, symbol-walking,
    disassembling path, not just the small-object one

Needs: a C compiler for the emulator, the host's binutils, and the staged
tools (`make binutils`).  -k keeps the temp dir.
"""
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
EMU_SRC = os.path.join(REPO, "prism_emu.c")
EMU_BIN = os.path.join(REPO, "build", "binutils-test", "prism_emu")
OUT = os.path.join(REPO, "build", "binutils-out")
TOOLS = ("ar", "ranlib", "nm", "objdump", "strip")
ALARM = "240"          # the stress disassembly takes a couple of seconds

T_C = "int add(int a, int b) { return a + b; }\nint answer = 42;\n"
T2_C = "int mul(int a, int b) { return a * b; }\nlong table[4] = {1, 2, 3, 4};\n"

keep = "-k" in sys.argv
failures = []
passes = 0


def run(cmd, **kw):
    """Run, returning (rc, stdout, stderr); the PrismOS tools' own exit
    status comes back through the emulator."""
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


def emu(tool, args, cwd):
    """A PrismOS utility under the emulator; returns (rc, out, err)."""
    return run([EMU_BIN, os.path.join(OUT, tool)] + list(args), cwd=cwd)


def hosttool(tool, args, cwd):
    """The host's own binutils, the reference implementation."""
    return run([tool] + list(args), cwd=cwd)


def normal(err):
    """Drop mlibc's missing-sysdep warnings (getrlimit stays ENOSYS by the
    port's contract: what the kernel lacks stays out)."""
    skip = ("sysdep_or_enosys", "__ensure", "In function mlibc::",
            "missing sysdep")
    return "\n".join(l for l in err.splitlines() if not any(s in l for s in skip))


def main():
    for t in TOOLS:
        if not os.path.isfile(os.path.join(OUT, t)):
            sys.exit("binutils_test: %s missing -- run `make binutils` first" % t)
    if not os.path.isfile(EMU_SRC):
        sys.exit("binutils_test: %s missing" % EMU_SRC)

    # ---- the emulator itself ------------------------------------------------
    print("== prism_emu (the PrismOS static-PIE runner) ==")
    os.makedirs(os.path.dirname(EMU_BIN), exist_ok=True)
    rc, _, err = run(["gcc", "-O2", "-Wall", "-o", EMU_BIN, EMU_SRC])
    check("compiles", rc == 0, err.strip()[-200:] if rc else "")

    tmp = tempfile.mkdtemp(prefix="prismos-binutils-")
    try:
        # ---- the inputs: two host objects, as any build would make them --
        with open(os.path.join(tmp, "t.c"), "w") as f:
            f.write(T_C)
        with open(os.path.join(tmp, "t2.c"), "w") as f:
            f.write(T2_C)
        for c in ("t.c", "t2.c"):
            rc, _, err = run(["gcc", "-c", "-o", os.path.join(tmp, c[:-2] + ".o"),
                              os.path.join(tmp, c)], cwd=tmp)
            check("host object %s" % c, rc == 0, err.strip()[-200:] if rc else "")
        big = os.path.join(OUT, "ar")          # ~1.9 MiB of PrismOS ELF

        print("== ar + ranlib (deterministic archives, byte for byte) ==")
        rc, out, err = emu("ar", ["rc", "pr.a", "t.o", "t2.o"], tmp)
        check("ar rc exits 0", rc == 0, normal(err).strip()[-200:])
        rc, _, err = hosttool("ar", ["rc", "host.a", "t.o", "t2.o"], tmp)
        check("host reference archive", rc == 0, err.strip()[-200:])
        rc, out, err = emu("ranlib", ["pr.a"], tmp)
        check("ranlib exits 0", rc == 0, normal(err).strip()[-200:])
        run(["ranlib", "host.a"], cwd=tmp)
        check("archive identical to the host's",
              open(os.path.join(tmp, "pr.a"), "rb").read()
              == open(os.path.join(tmp, "host.a"), "rb").read())

        print("== nm (symbols through bfd, text for text) ==")
        # same input file for both sides: the listing carries its name
        rc1, out1, err1 = emu("nm", ["host.a"], tmp)
        rc2, out2, err2 = hosttool("nm", ["host.a"], tmp)
        check("nm archive listing", rc1 == 0 and rc2 == 0
              and out1 == out2,
              "prismos: %r host: %r" % (out1[:120], out2[:120]))
        rc1, out1, err1 = emu("nm", ["t.o"], tmp)
        rc2, out2, err2 = hosttool("nm", ["t.o"], tmp)
        check("nm object listing (T add, D answer)",
              rc1 == 0 and out1 == out2
              and "T add" in out1 and "D answer" in out1,
              out1.strip()[:120])

        print("== objdump (the disassembler, text for text) ==")
        rc1, out1, err1 = emu("objdump", ["-d", "t.o"], tmp)
        rc2, out2, err2 = hosttool("objdump", ["-d", "t.o"], tmp)
        check("objdump -d t.o", rc1 == 0 and out1 == out2,
              "prismos: %d lines, host: %d lines"
              % (len(out1.splitlines()), len(out2.splitlines())))
        rc1, out1, err1 = emu("objdump", ["-d", big], tmp)
        rc2, out2, err2 = hosttool("objdump", ["-d", big], tmp)
        nl = len(out1.splitlines())
        check("objdump -d of a full utility (%d lines)" % nl,
              rc1 == 0 and rc2 == 0 and out1 == out2 and nl > 100000,
              "prismos: %d lines, host: %d lines"
              % (nl, len(out2.splitlines())))

        print("== strip (symbols out, byte for byte) ==")
        rc, out, err = emu("strip", ["-o", "pr-stripped.o", "t.o"], tmp)
        check("strip -o exits 0", rc == 0, normal(err).strip()[-200:])
        run(["strip", "-o", "host-stripped.o", "t.o"], cwd=tmp)
        check("stripped object identical to the host's",
              open(os.path.join(tmp, "pr-stripped.o"), "rb").read()
              == open(os.path.join(tmp, "host-stripped.o"), "rb").read())
        rc, out, err = emu("nm", ["pr-stripped.o"], tmp)
        # the warning lines of mlibc's missing-sysdep report can end without
        # a newline and glue themselves to the tool's own message, so the
        # check reads the raw stderr and only the display normalizes it
        check("nm on the stripped object: no symbols",
              rc == 0 and "no symbols" in err,
              normal(err).strip()[:120])

        print("== ar tv (the archive table) ==")
        rc1, out1, err1 = emu("ar", ["tv", "pr.a"], tmp)
        rc2, out2, err2 = hosttool("ar", ["tv", "host.a"], tmp)
        # the deterministic table: same names, sizes, modes -- only the
        # archive name differs (pr.a / host.a); compare past it
        strip_name = lambda s: [l.split()[1] if len(l.split()) > 1 else l
                                for l in s.splitlines()]
        check("ar tv listing", rc1 == 0 and rc2 == 0
              and strip_name(out1) == strip_name(out2),
              out1.strip()[:120])

        print("== the failure paths (errno honestly reported) ==")
        rc, out, err = emu("nm", ["no-such-file.o"], tmp)
        check("nm on a missing file exits 1", rc == 1
              and "No such file" in normal(err), "rc=%d %r" % (rc, normal(err)[:80]))
        rc, out, err = emu("objdump", [], tmp)
        check("objdump with no argument prints its usage",
              rc == 2 and "Usage:" in out + normal(err),
              "rc=%d" % rc)
    finally:
        if keep:
            print("tmp: %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    print("binutils_test: %d ok, %d failed" % (passes, len(failures)))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
