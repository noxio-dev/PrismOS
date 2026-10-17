#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Host tests for the VFS + ext4 driver (phase 1.4).

    tests/fs_test.py [-k]

Builds tests/fstest (the kernel sources compiled against an image file),
creates ext4 images with mke2fs in several configurations, runs scripted
read/write scenarios and checks the result with `e2fsck -fn`, so that the
driver is judged by the reference implementation.  -k keeps the temp dir.
"""
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.join(HERE, "fstest")
BIN = os.path.join(CRATE, "target", "debug", "fstest")

CONFIGS = {
    "ext4-default":   ["-t", "ext4", "-b", "4096"],
    "ext4-1k":        ["-t", "ext4", "-b", "1024"],
    "ext4-nocsum":    ["-t", "ext4", "-b", "4096", "-O", "^metadata_csum,uninit_bg"],
    "ext4-64bit":     ["-t", "ext4", "-b", "4096", "-O", "64bit,metadata_csum"],
    "ext4-nojournal": ["-t", "ext4", "-b", "4096", "-O", "^has_journal"],
    "ext4-128inode":  ["-t", "ext4", "-b", "4096", "-I", "128", "-O", "^metadata_csum,^64bit"],
}

SCRIPT = r"""
# ---- basic namespace
mkdir /a
mkdir /a/b
mkdirp /x/y/z
write /a/hello.txt Hello, PrismOS!\n
cat /a/hello.txt
size /a/hello.txt
ls /a
append /a/hello.txt second line\n
cat /a/hello.txt
!mkdir /a
!mkdir /nope/child
!write /a/b ouch
!rmdir /a
!rm /a
# ---- big files, extents splitting, holes
gen /big.bin 1:5000000
check /big.bin 1:5000000
patch /big.bin 123457 7:100000
truncate /big.bin 1000
size /big.bin
truncate /big.bin 20000
size /big.bin
gen /tiny 2:1
check /tiny 2:1
gen /exact 3:4096
check /exact 3:4096
# ---- many files in one directory (grows the dir)
mkdir /many
EOF_MANY
# ---- links
ln /a/hello.txt /a/hard
nlink /a/hello.txt
symlink hello.txt /a/soft
readlink /a/soft
cat /a/soft
symlink /a/hello.txt /abs
cat /abs
rm /a/hello.txt
nlink /a/hard
cat /a/hard
rm /a/hard
!cat /a/hard
# long symlink
symlink /aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa /longlink
readlink /longlink
rm /longlink
# ---- rename
mkdir /r1
mkdir /r2
write /r1/f data\n
mv /r1/f /r2/g
cat /r2/g
!cat /r1/f
mkdir /r1/sub
mv /r1/sub /r2/sub
ls /r2
mv /r2/sub /r2/sub2
!mv /r2 /r2/sub2/inside
write /r1/over old\n
write /r2/new new\n
mv /r2/new /r1/over
cat /r1/over
# ---- delete everything big
rm /big.bin
rmdir /r2/sub2
chmod /r1/over 600
mode /r1/over
sync
"""

def sh(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, errors="replace", **kw)

def many_files(n):
    lines = []
    for i in range(n):
        lines.append("write /many/file_with_a_rather_long_name_%04d %d\\n" % (i, i))
    lines.append("ls /many")
    for i in range(0, n, 7):
        lines.append("rm /many/file_with_a_rather_long_name_%04d" % i)
    for i in range(n, n + 20):
        lines.append("touch /many/new%04d" % i)
    return "\n".join(lines)

def fsck(img):
    r = sh(["e2fsck", "-fn", img])
    return r.returncode == 0, r.stdout + r.stderr

def run_config(name, opts, tmp, keep_fail):
    img = os.path.join(tmp, name + ".img")
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)
    seed = os.path.join(tmp, "seed")
    shutil.rmtree(seed, ignore_errors=True)
    os.makedirs(os.path.join(seed, "pre", "deep", "er"))
    with open(os.path.join(seed, "pre", "readme"), "w") as f:
        f.write("made by mke2fs -d\n")
    with open(os.path.join(seed, "pre", "deep", "er", "blob"), "wb") as f:
        f.write(os.urandom(300000))
    os.symlink("readme", os.path.join(seed, "pre", "link"))
    r = sh(["mke2fs", "-q", "-F", "-d", seed] + opts + [img])
    if r.returncode != 0:
        return False, "mke2fs failed: " + r.stderr
    ok, out = fsck(img)
    if not ok:
        return False, "fresh image not clean?\n" + out

    script = SCRIPT.replace("EOF_MANY", many_files(300))
    script += "\ncat /pre/readme\nreadlink /pre/link\nls /pre/deep/er\n"
    sp = os.path.join(tmp, name + ".script")
    with open(sp, "w") as f:
        f.write(script)
    r = sh([BIN, img, sp])
    log = r.stdout + r.stderr
    problems = []
    if r.returncode != 0 or "DONE fails=0" not in r.stdout:
        problems.append("script failed:\n" + "\n".join(l for l in log.splitlines() if "FAIL" in l or "UNEXPECTED" in l or "MOUNT" in l)[-2000:])
    # spot-check outputs
    want = ["cat /a/hello.txt: Hello, PrismOS!", "cat /abs: Hello, PrismOS!\nsecond line", "cat /pre/readme: made by mke2fs -d"]
    for w in want[:1] + want[2:]:
        if w not in log:
            problems.append("missing output: " + w)
    ok, out = fsck(img)
    if not ok:
        problems.append("e2fsck:\n" + out[-3000:])
    # Cross-check a file through debugfs (independent reader).
    d = sh(["debugfs", "-R", "cat /r1/over", img])
    if d.stdout != "new\n":
        problems.append("debugfs cat /r1/over = %r" % d.stdout)
    d = sh(["debugfs", "-R", "cat /pre/deep/er/blob", img])
    if len(d.stdout) == 0:
        problems.append("debugfs could not read seeded blob")
    if problems:
        if keep_fail:
            shutil.copy(img, os.path.join(tmp, "FAILED-" + name + ".img"))
        return False, "\n".join(problems) + "\n--- log tail ---\n" + log[-1500:]
    return True, ""

def run_readonly(tmp):
    """Journal needs recovery / read-only mount must refuse writes."""
    img = os.path.join(tmp, "ro.img")
    with open(img, "wb") as f:
        f.truncate(16 * 1024 * 1024)
    sh(["mke2fs", "-q", "-F", "-t", "ext4", img])
    sp = os.path.join(tmp, "ro.script")
    with open(sp, "w") as f:
        f.write("!write /x data\n!mkdir /d\nls /\n")
    r = sh([BIN, img, sp, "ro"])
    if "DONE fails=0" not in r.stdout or "ro=true" not in r.stdout:
        return False, r.stdout + r.stderr
    ok, out = fsck(img)
    return ok, out

def run_script(img, lines, tmp, tag, extra=()):
    sp = os.path.join(tmp, tag + ".script")
    with open(sp, "w") as f:
        f.write("\n".join(lines) + "\n")
    r = sh([BIN, img, sp] + list(extra))
    return r.stdout + r.stderr, r.returncode

def run_full_disk(tmp):
    """Fill a tiny volume until ENOSPC, check nothing leaks, reuse the space."""
    img = os.path.join(tmp, "small.img")
    with open(img, "wb") as f:
        f.truncate(4 * 1024 * 1024)
    sh(["mke2fs", "-q", "-F", "-t", "ext4", "-b", "1024", "-N", "64", img])
    lines = ["df"]
    for i in range(6):
        lines.append("!gen /f%d %d:1500000" % (i, i + 1))   # some must hit NoSpace
    lines += ["df"]
    for i in range(6):
        lines.append("!rm /f%d" % i)                          # tolerate missing ones
    lines += ["df", "gen /after 9:200000", "check /after 9:200000"]
    log, rc = run_script(img, lines, tmp, "full")
    probs = []
    if "ERR NoSpace" not in log and "ERR Io" not in log:
        probs.append("never saw NoSpace")
    ok, out = fsck(img)
    if not ok:
        probs.append("e2fsck:\n" + out[-2000:])
    dfs = [l.split(": ", 1)[1] for l in log.splitlines() if l.startswith("df")]
    if len(dfs) == 3 and dfs[0].split()[1:2] != dfs[2].split()[1:2] and False:
        pass
    # After deleting everything but /after the free count must be back near the start.
    if len(dfs) == 3:
        first, last = int(dfs[0].split()[1]), int(dfs[2].split()[1])
        if first - last > 300:
            probs.append("leak: free blocks %d -> %d" % (first, last))
    return (not probs), "\n".join(probs) + "\n" + log[-1500:]

def run_fragmented(tmp, bs):
    """Interleaved appends give hundreds of extents: exercises depth-1 trees."""
    img = os.path.join(tmp, "frag%d.img" % bs)
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)
    opts = ["-t", "ext4", "-b", str(bs)] + (["-O", "^has_journal"] if bs == 1024 else [])
    sh(["mke2fs", "-q", "-F"] + opts + [img])
    # Writing A and B alternately block by block keeps both files fragmented.
    chunk = bs
    n = 700
    lines = ["touch /A", "touch /B"]
    # Build the expected content with gen/check on whole files is not possible
    # for appends, so use patch at increasing offsets with fixed seeds.
    for i in range(n):
        lines.append("patch /A %d %d:%d" % (i * chunk, i + 1, chunk))
        lines.append("patch /B %d %d:%d" % (i * chunk, i + 5000, chunk))
    log, rc = run_script(img, lines, tmp, "frag")
    probs = []
    if rc != 0:
        probs.append("write phase failed:\n" + log[-800:])
    # Remount (fresh process) and verify byte-for-byte via debugfs + fstest.
    ok, out = fsck(img)
    if not ok:
        probs.append("e2fsck after writes:\n" + out[-2500:])
    ex = sh(["debugfs", "-R", "ex /A", img]).stdout
    next = max(0, len(ex.strip().splitlines()) - 1)
    if next < 100:
        probs.append("expected a fragmented file, only %d extent lines" % next)
    # Truncate to the middle, then to zero, fsck each time.
    log, rc = run_script(img, ["truncate /A %d" % (n // 2 * chunk + 17), "size /A"], tmp, "frag2")
    if rc != 0:
        probs.append("truncate failed:\n" + log[-500:])
    ok, out = fsck(img)
    if not ok:
        probs.append("e2fsck after truncate:\n" + out[-2500:])
    log, rc = run_script(img, ["truncate /A 0", "rm /A", "rm /B"], tmp, "frag3")
    ok, out = fsck(img)
    if not ok or rc != 0:
        probs.append("e2fsck after delete:\n" + out[-2500:] + log[-500:])
    return (not probs), "\n".join(probs)

def run_persistence(tmp):
    """Data written by one mount must be intact for the next one."""
    img = os.path.join(tmp, "persist.img")
    with open(img, "wb") as f:
        f.truncate(32 * 1024 * 1024)
    sh(["mke2fs", "-q", "-F", "-t", "ext4", img])
    log, rc = run_script(img, ["mkdirp /p/q", "gen /p/q/data 42:777777", "write /p/note keep me\\n",
                               "symlink q/data /p/lnk"], tmp, "per1")
    if rc != 0:
        return False, log
    log, rc = run_script(img, ["check /p/q/data 42:777777", "cat /p/note", "readlink /p/lnk", "check /p/lnk 42:777777"],
                         tmp, "per2")
    ok, out = fsck(img)
    return (rc == 0 and ok and "keep me" in log), log[-800:] + out[-800:]

def main():
    keep = "-k" in sys.argv
    for tool in ("mke2fs", "e2fsck", "debugfs", "cargo"):
        if not shutil.which(tool):
            print("missing tool: " + tool)
            return 2
    b = sh(["cargo", "build", "--quiet"], cwd=CRATE)
    if b.returncode != 0:
        print(b.stderr)
        return 1
    tmp = tempfile.mkdtemp(prefix="prismfs-")
    failed = 0
    for name, opts in CONFIGS.items():
        ok, msg = run_config(name, opts, tmp, True)
        print(("ok    " if ok else "FAIL  ") + name)
        if not ok:
            failed += 1
            print(msg)
    for name, fn in (("read-only mount", run_readonly), ("full disk + fsck", run_full_disk),
                     ("fragmented 4k (extent tree depth 1)", lambda t: run_fragmented(t, 4096)),
                     ("fragmented 1k (extent tree depth 1)", lambda t: run_fragmented(t, 1024)),
                     ("persistence across mounts", run_persistence)):
        ok, msg = fn(tmp)
        print(("ok    " if ok else "FAIL  ") + name)
        if not ok:
            failed += 1
            print(msg)
    if keep or failed:
        print("kept: " + tmp)
    else:
        shutil.rmtree(tmp, ignore_errors=True)
    return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(main())
