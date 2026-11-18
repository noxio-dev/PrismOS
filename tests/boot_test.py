#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""
Automated boot tests: run the disk image under QEMU (legacy BIOS, then UEFI
with OVMF) and drive the loader / kernel shell / userland session through
the serial console.

    tests/boot_test.py build/prismos.img [qemu] [ovmf_code] [ovmf_vars]

Each scenario starts a fresh VM, optionally sends keystrokes when a given
text shows up, and succeeds when every expected string has been seen before
the timeout.  Expectations are substrings of the whole serial output, or
("count", string, n) for "at least n occurrences", or ("not", string) for
"must never appear".  A step's wait_for can also be satisfied by the n-th
occurrence: (wait_for, send, delay, n).  The boot disk always sits on AHCI
port 0 so the kernel can find its ext4 root partition (P3) on its AHCI
driver.

What the OS boots like these days (the ground truth these scenarios pin):
the default boot hands the machine to BusyBox init (pid 1), which runs
/etc/init.d/rcS (mounts, hostname, keymap), opens the keyboard-setup prompt
(idle-timed out headless), prints the banner and puts a `-/bin/login` on
each tty of /etc/inittab -- the serial session logs in with the empty
root password and lands in busybox ash at the prism$ prompt.  The kernel
shell remains the rescue mode (loader.conf kernel_options="init=kshell",
built into build/prismos-rescue.img below).  The full kernel boot report
(pmm/vmm/heap/drm/clock/mouse/...) is behind `-v` now: only the CSPRNG
line and the two vfs mount lines print unqualified, plus -- on the userland
session -- nothing else, so the autoboot scenarios pin init's own output.

Known OS bugs the suite deliberately exposes (see the report that shipped
with this rewrite): the image files are owned by the build host's uid
(the ownership check below fails until tools/mkimage.py fixes it) and the
AF_UNIX nonblock check of /bin/unixsock fails on the kernel side.
"""

import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time


def kernel_version():
    """The binary's own version (sys/kern/Cargo.toml) so checks that pin it
    (the kernel's /proc/version, hello's env:) keep tracking the kernel."""
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with open(os.path.join(root, "sys/kern/Cargo.toml")) as f:
        for line in f:
            if line.startswith("version"):
                return line.split("=", 1)[1].strip().strip('"')
    return "unknown"


KVER = kernel_version()

LOGIN_PROMPT = "prismos login:"
SHELL_PROMPT = "prism$ "
KSHELL_PROMPT = "prism# "
MOTD = "Welcome to PrismOS (busybox + ash, phase 3.4)."


def expectation_met(text, want):
    if isinstance(want, tuple):
        kind = want[0]
        if kind == "count":
            return text.count(want[1]) >= want[2]
        if kind == "not":
            return want[1] not in text
        if kind == "line":
            # all the tokens on one line of the output (e.g. an ls -l row:
            # the mode + owner on the line that carries the file name)
            return any(all(t in l for t in want[1:]) for l in text.splitlines())
    return want in text


def expectation_missing(text, want):
    if isinstance(want, tuple):
        kind = want[0]
        if kind == "count":
            return "%s (x%d, want x%d)" % (want[1], text.count(want[1]), want[2])
        if kind == "not":
            return "not to appear: %s" % want[1]
        if kind == "line":
            return "one line with %s" % " and ".join(want[1:])
    return want


def run_vm(cmd, steps, expect, timeout, autologin=False):
    """steps: list of (wait_for, send[, delay[, nth]]); expect: strings that
    must appear (or ("count", s, n) / ("not", s) tuples)."""
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    out = b""
    steps = list(steps)
    deadline = time.time() + timeout
    login_sent = 0
    try:
        while time.time() < deadline:
            r, _, _ = select.select([proc.stdout], [], [], 0.2)
            if r:
                chunk = os.read(proc.stdout.fileno(), 65536)
                if not chunk:
                    break
                out += chunk
                sys.stdout.write(chunk.decode("latin-1", errors="replace"))
                sys.stdout.flush()
            if autologin:
                # Every re-printed login prompt gets a fast root: the login
                # process's blocking read dies with EAGAIN after 10 s (the
                # tty read's WAIT_MAX_MS, mlibc makes it EOF), so the reply
                # must come well inside that window -- and after a shell
                # exit (^D below) init reprints the prompt, which this also
                # answers.  Sending once per prompt occurrence keeps the
                # replies ordered with the (one-line, atomic) prints.
                seen = out.count(LOGIN_PROMPT.encode())
                if seen > login_sent:
                    login_sent = seen
                    time.sleep(0.2)
                    proc.stdin.write(b"root\r")
                    proc.stdin.flush()
            while steps:
                tok = steps[0][0].encode()
                nth = steps[0][3] if len(steps[0]) > 3 else 1
                if out.count(tok) < nth:
                    break
                _, send = steps[0][0], steps[0][1]
                delay = steps[0][2] if len(steps[0]) > 2 else 0.3
                steps.pop(0)
                time.sleep(delay)
                proc.stdin.write(send.encode())
                proc.stdin.flush()
            if all(expectation_met(out.decode("latin-1"), e) for e in expect):
                break
    finally:
        proc.kill()
        proc.wait()
    text = out.decode("latin-1")
    missing = [expectation_missing(text, e) for e in expect
               if not expectation_met(text, e)]
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

    # ---- boot-report expectations -----------------------------------------
    # The unqualified boot lines (no -v): the CSPRNG seeding and the two vfs
    # reports of the root and devfs.  Everything else the kernel prints --
    # Hello world!, pmm/vmm/heap, drm, clock, mouse, ahci/nvme/xhci -- is
    # behind -v (the boot -v scenario pins that report).
    kernel_brief = [
        "random   : ChaCha20 CSPRNG, seeded",
        "vfs      : disk 0 partition 3 on / (ext4), 96 MiB",
        "vfs      : devfs on /dev",
    ]
    # The BusyBox init hand-over: pid 1 reads /etc/inittab, runs rcS (the
    # SNN* script banner of /etc/rc), opens the keyboard-setup prompt that a
    # headless boot times out of, prints the banner twice (rc + session) and
    # puts the login on the serial tty.
    userland_start = kernel_brief + [
        "init started: BusyBox v1.37.0",
        "starting pid 2, tty '': '/etc/init.d/rcS'",
        "PrismOS: system startup",
        "S00mounts", "S05hostname", "S10keymap",
        "Keyboard layout successfully loaded!",
        "PrismOS: startup complete",
        "tty '/dev/ttyS0': '-/bin/login'",
        LOGIN_PROMPT,
    ]
    # The rescue boot (init=kshell): the kernel mounts /tmp and /proc (and
    # /sys) itself and says so -- the kernel-shell scenarios expect those
    # lines; the full report is still behind -v.
    kshell_start = kernel_brief + [
        "vfs      : tmpfs on /tmp (RAM, 16 MiB / 4096 files quota)",
        "vfs      : procfs on /proc (generated, read-only)",
        "vfs      : sysfs on /sys (read-only)",
        "PrismOS " + KVER + " kernel console -- type 'help' for commands.",
        KSHELL_PROMPT,
    ]
    # The -v boot report (loader prompt: boot -v), the kernel's full lines.
    verbose_report = [
        "Hello, world!",
        "PrismOS kernel " + KVER + " (Rust, x86_64)",
        'cmdline  : "-v"',
        "firmware : BIOS",
        "memory   : 248 MiB usable",
        "kernel   : 6144 KiB at phys 0x200000, virt 0xffffffff80200000",
        "display  : 1920x1080 framebuffer",
        "cpu      : QEMU Virtual CPU version 2.5+",
        "machine  : QEMU (TCG",
        "gdt/tss  : loaded (code 0x8, data 0x10, tss 0x28)",
        "idt      : 256 gates, exceptions 0..20 + irqs 0x20..0x2f",
        "pic      : 8259 remapped to 0x20..0x2f, all lines masked",
        "pit      : channel 0 at 100 Hz",
        "serial   : COM1 115200 8n1, rx on irq4",
        "pmm      : 254 MiB in",
        "vmm      : own pml4 active, direct map at 0xffff888000000000, kernel W^X",
        "display  : back buffer active, text and graphics are double-buffered",
        "heap     : 16384 KiB at 0xffffffff90000000 (buddy 4 KiB..4 MiB + slab 32..1024)",
        "drm      : prismfb, 1920x1080 -> /dev/dri/card0",
        "user     : syscall/sysret ready",
        "proc     : pcb + 16 KiB kernel stacks per process, preemptive priority scheduler",
        "acpi     : rev 0 (RSDT)",
        "random   : ChaCha20 CSPRNG, seeded",
        "apic     : lapic id 0 at 0xfee00000 (xAPIC)",
        "clock    : tsc",
        "measured on HPET, NOT invariant",
        "timer    : lapic one-shot deadlines + 100 Hz scheduler tick",
        "kbd      : i8042 self-test ok",
        "mouse    : PS/2 on irq12, evdev /dev/input/event1",
        "interrupts enabled",
        "smp      : single cpu",
        "pci      : 7 function(s) on the bus",
        "ahci     : controller 00:04.0 AHCI 1.0, 6 port(s), 32 slot(s), 1 disk(s)",
        "nvme     : 0 namespace(s)",
        "xhci     : 0 controller(s)",
        "disk  0  : sata 136 MiB",
        "user     : /sbin/init as pid 1 -- the userland session",
    ]

    # Phase 3.3+: the default boot is the userland session (BusyBox init as
    # pid 1, a login on ttyS0, and the shell at prism$); the kernel shell is
    # the rescue mode.  The kernel-shell scenarios below run against a
    # rescue image -- the same root tree with kernel_options="init=kshell"
    # in its loader.conf, rebuilt with the same tools -- and the userland
    # ones against the image as built (they log in first: the autologin
    # machinery in run_vm answers every fresh prismos login: with root,
    # fast, because the login's read dies after 10 s without input).
    build = os.path.dirname(os.path.abspath(image))

    def make_rescue():
        rescue = os.path.join(build, "prismos-rescue.img")
        rroot = os.path.join(build, "root-rescue")
        conf = os.path.join(rroot, "boot", "loader.conf")
        fresh = not (os.path.exists(rescue) and os.path.exists(conf))
        if not fresh:
            with open(conf) as f:
                fresh = "init=kshell" not in f.read()
        # The tree the rescue image embeds must follow the root it was
        # copied from: a newer kernel / app build (root.stamp) means the
        # copy -- and the image -- are stale.
        stamp = os.path.join(build, "root.stamp")
        if not fresh and os.path.exists(stamp):
            fresh = os.path.getmtime(stamp) > os.path.getmtime(rescue)
        if fresh:
            if os.path.exists(rroot):
                shutil.rmtree(rroot)
            # symlinks=True: the usr-merge /bin -> usr/bin must survive
            # the copy as the link it is (copytree's default FOLLOWS it
            # and pastes the target directory in its place, which the
            # image builder's usr-merge check then rightly refuses).
            shutil.copytree(os.path.join(build, "root"), rroot, symlinks=True)
            with open(conf, "a") as f:
                f.write('\n# rescue boot: the kernel shell instead of /sbin/init\n'
                        'kernel_options="init=kshell"\n')
            mkimage = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                   "..", "tools", "mkimage.py")
            subprocess.run([sys.executable, mkimage,
                            "--boot0", os.path.join(build, "boot0.bin"),
                            "--boot1", os.path.join(build, "boot1.bin"),
                            "--loader", os.path.join(build, "loader.bin"),
                            "--esp-dir", os.path.join(build, "esp"),
                            "--root-dir", rroot, "-o", rescue], check=True)
        return rescue

    rescue = make_rescue()

    def ksh_cmd():
        """the rescue boot (init=kshell): the kernel shell owns the console"""
        shutil.copyfile(rescue, work)
        return base + ahci_boot(work)

    # The NVMe + xHCI scenario needs a second (blank, 32 MiB) ext4 image --
    # never written, only probed by the kernel's nvme driver.
    nvme = None
    if have_fs:
        nvme = os.path.join(tmp, "nvme.img")
        with open(nvme, "wb") as f:
            f.truncate(32 * 1024 * 1024)
        subprocess.run(["mke2fs", "-q", "-F", "-t", "ext4", nvme],
                       capture_output=True, check=True)

    def hw_cmd():
        shutil.copyfile(rescue, work)
        return base + ahci_boot(work) + [
            "-drive", "id=nvme0,file=%s,format=raw,if=none" % nvme,
            "-device", "nvme,drive=nvme0,serial=PRISMNVME",
            "-device", "qemu-xhci,id=xhci0",
            "-device", "usb-kbd,bus=xhci0.0"]

    scenarios = [
        # BIOS gets a VBE linear framebuffer: the loader switches to it at
        # the point of no return and the kernel double-buffers its console.
        # The framebuffer lines are -v-only now: the autoboot expectations
        # are the brief kernel lines plus init's whole hand-over.
        ("BIOS autoboot: loader, init hand-over, login", bios_cmd, [], userland_start, 60),
        ("BIOS prompt: Esc, then boot -v (kernel report)", bios_cmd,
         [("Autoboot in", "\x1b"), ("OK ", "boot -v\r")],
         ["OK ", "PrismOS loader (bios)", "Loading kernel /boot/kernel/kernel"]
         + verbose_report + [LOGIN_PROMPT], 60),
        ("BIOS shell: exception recovery", ksh_cmd,
         [(KSHELL_PROMPT, "crash int\r"), (KSHELL_PROMPT, "crash page\r"),
          (KSHELL_PROMPT, "crash div\r"), (KSHELL_PROMPT, "uptime\r")],
         ["handled, continuing", "page mapped, continuing", "divide by zero",
          "survived", "ticks at 100 Hz"], 60),
        ("BIOS shell: memory + keyboard layout", ksh_cmd,
         [(KSHELL_PROMPT, "meminfo\r"), (KSHELL_PROMPT, "layout fr\r"),
          (KSHELL_PROMPT, "kbd\r")],
         ["pages total", "self-test: ok", "switched to fr", "i8042"], 60),
        # Phase 2: ring 3 processes, context switch, preemptive scheduler
        # (time slices, priorities, sleep/wakeup), watchdog.
        ("BIOS shell: ring 3 processes + scheduler (phase 2.3)", ksh_cmd,
         [(KSHELL_PROMPT, "user ping pong\r"), (KSHELL_PROMPT, "user spin:3 sleep:0\r"),
          (KSHELL_PROMPT, "usertest\r"), (KSHELL_PROMPT, "ps\r")],
         # (the syscall/sysret and pcb lines of the boot are -v-only now:
         #  the boot -v scenario pins them; these steps pin the scheduler)
         ["[ring 3] ping", "[ring 3] pong",
          "pid 1 ping", "pid 2 pong",
          "pid 3 spin", "pid 4 sleep",
          "preempt  ok", "prio     ok", "wakeup   ok",
          "usertest: all 21 passed",
          "process table empty"], 240),
        # Phase 2.4: ELF programs from the ext4 root (/bin/hello is a static PIE
        # built by `make`): loader, argv/envp/auxv, execve, per-process FPU
        # state, fault containment, exit status.  hello errors stops at its
        # third check these days: /bin/notelf is the rootfs' own shebang demo
        # now (a #!/bin/sh script), so its execve succeeds and replaces hello
        # with the script -- ash's own fork of `echo` takes the next pid, so
        # the waits below key on the occurrences of the exit report and the
        # notelf report instead of pinning process numbers (see the report).
        ("BIOS shell: ELF programs + execve (phase 2.4)", ksh_cmd,
         [(KSHELL_PROMPT, "exec hello info alpha\r"),
          ("hello -- exited with status 0", "exec hello chain x y\r", 0.3, 1),
          ("hello -- exited with status 0", "exec hello errors\r", 0.3, 2),
          ("notelf -- exited with status 0", "exec hello fpu 1\r", 0.3, 1),
          ("hello -- exited with status 0", "exec hello fpu 2\r", 0.3, 3),
          ("hello -- exited with status 0", "exec hello fault\r", 0.3, 4),
          ("killed by exception 14", "exec hello exit 42\r"),
          ("exited with status 42", "exec notelf\r")],
         ['argv[2] = "alpha"', "env: PRISMOS=" + KVER, "main() runs at 0x", "AT_PAGESZ = 4096",
          "float check: ok",
          "hello chain: pid 2 calls execve()", 'argv[2] = "(after execve)"', 'argv[4] = "y"',
          "execve(/nonexistent)         -> -2", "execve(/boot/loader.conf)    -> -13",
          ("count", "not an ELF file", 2),            # errors' third check + the step below
          ("count", "notelf -- exited with status 0", 2),
          "fpu 1: 40 rounds ok", "fpu 2: 40 rounds ok",
          "hello fault: about to dereference NULL", "killed by exception 14",
          "exited with status 42"], 240),
        # Phase 2.5: fork with copy-on-write, wait, signals (/bin/fork):
        # COW isolation, pid / status round trips, blocking and WNOHANG
        # waits, SIGCHLD handlers, SIGTERM kills, register restoration
        # through sigreturn, execve from a forked child, orphan adoption.
        ("BIOS shell: fork, wait, signals (phase 2.5)", ksh_cmd,
         [(KSHELL_PROMPT, "exec fork\r"),
          ("fork: all 10 passed", "ps\r")],
         ["fork (pid 1): fork, wait, signals and copy-on-write",
          "cow       ok -- 3 pages rewritten by the child, parent copy intact",
          "pids      ok", "waiterr   ok", "waitpid   ok",
          "sigchld   ok", "sigterm   ok", "sigself   ok",
          "exec      ok", "orphan    ok", "sigkill   ok",
          "fork: all 10 passed",
          "pid 1 fork -- exited with status 0",
          "process table empty"], 240),
        # Phase 2.6: per-process descriptors on the VFS, pipes, /dev.
        # /bin/fd exercises them end to end: stdio through the table, file
        # offsets with lseek, the /dev devices, pipes (round trip, FIFO
        # order, blocking until a forked writer writes, EOF / EPIPE),
        # descriptor inheritance across fork, dup, a real read of fd 0 from
        # the serial line (its `wxyz` step below -- the answer must be typed
        # exactly at the fd prompt, in the same VM, or the following bytes
        # are what fd reads), and fds surviving execve.  This scenario keeps
        # fd alone in its own VM: the named-socket files /bin/unixsock and
        # /bin/posix leave behind in /tmp trip its EEXIST check.
        ("BIOS shell: descriptors, pipes, /dev (phase 2.6)", ksh_cmd,
         [(KSHELL_PROMPT, "exec fd\r"),
          ("type 4 bytes (EXACTLY wxyz):", "wxyz\r"),
          (KSHELL_PROMPT, "ps\r"),
          (KSHELL_PROMPT, "ls /dev\r"),
          (KSHELL_PROMPT, "mount\r"),
          (KSHELL_PROMPT, "df\r")],
         ["fd (pid 1): descriptors, pipes and /dev",
          "stdio     ok", "file      ok", "dev       ok", "pipe      ok",
          "pblock    ok", "epipe     ok", "forkfd    ok", "dups      ok",
          "stdin     ok", "execfd    ok",
          "fd: all 10 passed",
          "read blocked", "parent read \"0123\", child (shared offset) \"4567\"",
          "(written to /dev/console)",
          "(written to a dup of stdout)",
          "postexec: stdout survived execve",
          "postexec: read 4 bytes after execve",
          'read 4 byte(s): "wxyz"',
          "pid 1 fd -- exited with status 0",
          "process table empty",
          "devfs on /dev (devfs, ro)"], 240),
        # Phase 3.1: the newlib syscall set.  /bin/posix drives all seventeen
        # new numbers end to end (stat/fstat, the program break, mmap, cwd,
        # dup2/fcntl, O_NONBLOCK pipes, SIGPIPE, getdents, the name-space
        # syscalls, uname / the isatty ioctl pair).  The shell steps also
        # exercise the kernel shell's own cd + pwd.
        ("BIOS shell: newlib syscall set (phase 3.1)", ksh_cmd,
         [(KSHELL_PROMPT, "exec posix\r"),
          ("posix: all 11 passed", "ps\r"),
          (KSHELL_PROMPT, "cd /boot\r"),
          (KSHELL_PROMPT, "pwd\r")],
         ["posix (pid 1): the C library's syscall set",
          "stat      ok", "brk       ok", "mmap      ok", "cwd       ok",
          "dup2      ok", "nonblk    ok", "sigpipe   ok", "getdents  ok",
          "fsops     ok", "uname     ok",
          "posix: all 11 passed",
          "postexec: O_CLOEXEC fd closed, dupfd alive, stdout ok",
          "pid 1 posix -- exited with status 0",
          "process table empty",
          "/boot"], 240),
        # AF_UNIX stream sockets (the X11 transport; syscalls 90..102,
        # sys/kern/src/user/fd/unix.rs).  /bin/unixsock drives them end to
        # end.  The state machine itself is host-tested by `make unixtest`.
        # KNOWN KERNEL BUG (deliberately exposed): the nonblock check
        # fails -- an EAGAIN read of a filled socket is not the EAGAIN the
        # check asked for -- so the run reports 1 failure.  The scenario
        # pins today's honest output; flip it to "all 10 passed" when the
        # socket layer is fixed.
        ("BIOS shell: AF_UNIX stream sockets (nonblock bug exposed)", ksh_cmd,
         [(KSHELL_PROMPT, "exec unixsock\r"),
          ("unixsock: 1 failure(s)", "ps\r")],
         ["unixsock (pid 1): AF_UNIX stream sockets",
          "pair      ok", "named     ok", "abstract  ok", "forkecho  ok",
          "rights    ok", "nonblock  FAIL", "shutdown  ok", "poll      ok",
          "opts      ok", "errors    ok",
          "unixsock: 1 failure(s), FAILURES",
          "pid 1 unixsock -- exited with status 1",
          "process table empty"], 240),
        # The kernel's own self-tests and clocks, driven from the shell:
        # ktest (the 20 kernel scenarios), the tsc clock measured against
        # HPET, the rtc, the dmesg ring (level filters, timestamps) and the
        # buddy+slab allocator under churn.
        ("BIOS shell: ktest, clocks, dmesg ring, heap (self-tests)", ksh_cmd,
         [(KSHELL_PROMPT, "ktest\r"),
          ("ktest: all 20 passed", "clock\r"),
          (KSHELL_PROMPT, "date\r"),
          (KSHELL_PROMPT, "dmesg -l warn\r"),
          (KSHELL_PROMPT, "dmesg -l debug\r"),
          (KSHELL_PROMPT, "heaptest 5000\r")],
         ["ktest: all 20 passed",
          "source   : tsc",
          "measured on HPET, NOT invariant",
          "hpet     : 100000000 Hz, 64-bit counter",
          "timer    : one-shot deadlines + 100 Hz tick",
          "rtc      : 2026-",
          "-- 0 record(s)",                       # dmesg -l warn: a clean boot
          "[    0.0] info  random   : ChaCha20 CSPRNG, seeded",
          "retired by wrapping",                   # the ring's own footer
          "heaptest : 5000 objects allocated/freed, 0 alloc failures, ok, no corruption"], 240),
    ]
    if nvme:
        # The device tree beyond the boot disk: an NVMe controller (drive +
        # -device nvme) and a QEMU xHCI with a USB keyboard.  disks must
        # list both disks, usb the controller and the keyboard behind a
        # port-change notice, lspci both controllers by their QEMU vendor.
        scenarios.append(
            ("BIOS shell: NVMe + xHCI/USB keyboard (device probe)", hw_cmd,
             [(KSHELL_PROMPT, "disks\r"),
              (KSHELL_PROMPT, "usb\r"),
              (KSHELL_PROMPT, "lspci\r")],
             ["disk 0   : sata 278528 sectors (136 MiB), block 512",
              'disk 1   : nvme 65536 sectors (32 MiB), block 512, "QEMU NVMe Ctrl" serial PRISMNVME',
              "usb      : 1 xHCI controller(s)",
              "0627:0001 keyboard",
              "(port change pending: `usb rescan`)",
              "1b36:0010  NVMe controller",
              "1b36:000d  USB controller",
              "SATA controller"], 120))
    if code and vars_ and os.path.exists(code):
        scenarios += [
            ("UEFI autoboot: loader, init hand-over, login", uefi_cmd, [],
             userland_start + ["PrismOS loader (uefi)", "prismos login:"], 120),
            ("UEFI prompt: Esc, lsmem, boot -v", uefi_cmd,
             [("Autoboot in", "\x1b"), ("OK ", "lsmem\rboot -v\r")],
             ["OK ", "usable", "Hello, world!", 'cmdline  : "-v"',
              "firmware : UEFI", LOGIN_PROMPT], 120),
        ]
    else:
        print("note: OVMF not found, skipping UEFI scenarios")

    # The loader reads its loader.conf overrides from the ext4 root
    # partition (P3) and the kernel mounts the same partition as / -- the
    # whole disk chain (boot0 -> boot1 -> loader -> kernel) runs off one
    # ext4 volume.  (Rescue image: the writes belong to the kernel shell's
    # own /d commands.)
    def fs_cmd():
        shutil.copyfile(rescue, fswork)
        return base + ahci_boot(fswork)

    scenarios.append(
        ("BIOS shell: ext4 root partition (P3, read-write)", fs_cmd,
         [(KSHELL_PROMPT, "cat /boot/loader.conf\r"),
          (KSHELL_PROMPT, "mkdir /d\r"),
          (KSHELL_PROMPT, "write /d/t.txt persisted by PrismOS\r"), (KSHELL_PROMPT, "cat /d/t.txt\r"),
          (KSHELL_PROMPT, "ls /d\r"), (KSHELL_PROMPT, "df\r"), (KSHELL_PROMPT, "sync\r")],
         ["vfs      : disk 0 partition 3 on / (ext4",
          "PrismOS loader configuration",
          "persisted by PrismOS", "t.txt", "mounted on", "synced"], 90))

    # The extra bare ext4 disk on AHCI port 1 (phase 1.5: make test with the
    # ext4 data disk): the kernel still boots from disk 0's P3, and the shell
    # mounts the second disk read-write, writes to it, and unmounts cleanly.
    if have_fs:
        def data_cmd():
            shutil.copyfile(rescue, work)
            return base + ahci_boot(work) + data_drive()

        scenarios.append(
            ("BIOS shell: ext4 data disk (AHCI port 1, read-write)", data_cmd,
             [(KSHELL_PROMPT, "mount -l\r"),
              (KSHELL_PROMPT, "mkdir /mnt\r"),
              (KSHELL_PROMPT, "mount disk1 /mnt\r"),
              (KSHELL_PROMPT, "cat /mnt/hello.txt\r"),
              (KSHELL_PROMPT, "write /mnt/from-guest.txt persisted on the data disk\r"),
              (KSHELL_PROMPT, "cat /mnt/from-guest.txt\r"),
              (KSHELL_PROMPT, "df\r"),
              (KSHELL_PROMPT, "umount /mnt\r")],
             ["disk1        whole disk",
              "mounted disk1 on /mnt",
              "Hello from the PrismOS ext4 data disk.",
              "persisted on the data disk",
              "unmounted /mnt"], 90))

    # The boot tree with INDEXED directories: after a kernel dx_convert or an
    # e2fsck -D pass (or years of package updates) /boot/lua and /boot/kernel
    # are htree-indexed -- both the loader (linear scan through the dx
    # blocks) and the kernel (dx_probe lookup) must still find their files.
    idximg = None
    if have_fs and shutil.which("debugfs"):
        idximg = index_boot_tree(rescue, os.path.join(tmp, "idx.img"))
    if idximg:
        def idx_cmd():
            return base + ahci_boot(idximg)

        scenarios.append(
            ("BIOS: boot tree with indexed (htree) directories", idx_cmd,
             [(KSHELL_PROMPT, "cat /boot/loader.conf\r"),
              (KSHELL_PROMPT, "ls /boot/lua\r"),
              (KSHELL_PROMPT, "cat /boot/lua/loader.lua\r")],
             # the boot itself is the proof: the loader walked the indexed
             # /boot tree (linear scan through the dx blocks) and the
             # kernel dx_probe found loader.conf, both in /boot/lua and
             # /boot/kernel (the kshell banner means the rescue option
             # arrived -- the two vfs reports, the same brief lines).
             kshell_start
             + ["PrismOS loader configuration",
                "loader.lua"], 90))

    # SMP (phase 1.5): three cpus, the APs come up through INIT-SIPI-SIPI,
    # run their own LAPIC timer and answer IPIs (the scheduler stays on the
    # BSP until phase 2 grows per-CPU scheduling).
    def smp_cmd():
        shutil.copyfile(rescue, work)
        return base + ["-smp", "3"] + ahci_boot(work)

    scenarios.append(
        ("BIOS SMP: 3 cpus, AP bring-up + IPI round-trip", smp_cmd,
         [(KSHELL_PROMPT, "smp\r"),
          (KSHELL_PROMPT, "uptime\r")],
         ["cpu  lapic  state   ticks      ipis   idle-halts",
          "online", "online", "online",
          "ipi      : 2/2 AP(s) answered the round-trip",
          "ipi      : ok", "uptime   :", "ticks at 100 Hz"], 120))

    # ---- the userland session (BIOS + login) ------------------------------
    # BusyBox init put a login on ttyS0 (root's password is empty); the
    # autologin machinery above answers it, and the session is busybox ash:
    # /proc generated on read, the usr-merge tree, the applets.  Each
    # scenario below is its own fresh boot + login.

    # The mounts pid 1's scripts made, /proc answering, the usr-merge
    # links, the identity files -- and the image ownership check that is
    # DELIBERATELY red today: tools/mkimage.py's mke2fs -d copies the
    # build host's uid over the whole rootfs (everything is uid 1000,
    # `user user`, instead of root's 0).  When mkimage is fixed (or the
    # files pre-chowned), flip the two owner expectations back to root.
    scenarios.append(
        ("BIOS userland: login, /proc, mounts, usr-merge, ownership", bios_cmd,
         [(SHELL_PROMPT, "cat /proc/mounts\r"),
          ("disk0p3 / ext4 rw 0 0", "ls /proc\r"),
          ("uptime", "cat /proc/version\r"),
          ("PrismOS version", "cat /proc/meminfo\r"),
          ("MemTotal:", "ls -l /\r"),
          ("-> usr/bin", "df\r"),
          ("Filesystem", "cat /etc/passwd\r"),
          ("root::0:0:root:/:/usr/bin/sh", "id\r"),
          ("uid=0(root)", "hello info\r"),
          ("hello from an ELF program", "ls -l /etc/passwd /etc/motd\r"),
          ("/etc/passwd", "head -n 3 /proc/cpuinfo\r")],
         kernel_brief
         + ["tmpfs /tmp tmpfs rw 0 0",       # S00mounts' four mounts
            "devfs /dev devfs ro 0 0",
            "procfs /proc procfs ro 0 0",
            "sysfs /sys sysfs ro 0 0",
            "cmdline", "meminfo", "cpuinfo", "mounts", "self", "uptime", "version",
            "MemTotal:", "MemFree:", "MemAvailable:",
            "processor\t: 0",                 # /proc/cpuinfo's first line
            # the usr-merge (ls -l /): the four links plus lost+found
            "bin -> usr/bin", "lib -> usr/lib", "sbin -> usr/sbin",
            "lost+found",
            "Filesystem",                     # busybox df's header
            "root::0:0:root:/:/usr/bin/sh",   # /etc/passwd (empty passwords)
            "uid=0(root) gid=0(root) groups=0(root)",
            "hello from an ELF program", "env: PRISMOS=" + KVER,
            "main() runs at 0x", "float check: ok",
            # ownership: the image files must belong to root.  RED until
            # tools/mkimage.py stops copying the host uid (see the module
            # docstring) -- today they print as `user     user`.  Pinned as
            # one ls -l row: the mode, the root owner and the file name on
            # the same line (other root-owned files must not satisfy it).
            ("line", "-rw-r--r--", "root     root", "/etc/passwd")], 300))

    # The busybox applets the session leans on, in ash: the tmpfs package
    # (twice -- a good /tmp citizen leaves the tree clean), fork, the
    # dynamically linked hello (AT_PHNUM = 8, the interpreter's own), the
    # infofetch banner, and ash's own pipes / arithmetic / subshell.
    scenarios.append(
        ("BIOS userland: tmpfs x2, fork, hello-dyn, infofetch, ash", bios_cmd,
         [(SHELL_PROMPT, "tmpfs\r"),
          ("tmpfs: 37/37 checks passed", "tmpfs\r"),
          ("tmpfs: 37/37 checks passed", "fork\r"),
          ("fork: all 10 passed", "hello-dyn\r"),
          ("AT_PHNUM = 8", "infofetch\r"),
          ("QEMU Virtual CPU version", "echo one two three | wc -w | sed 's/^/wc=/'\r"),
          ("wc=3", "echo answer=$((6 * 7))\r"),
          ("answer=42", "for i in 1 2 3; do echo line $i; done | wc -l | sed 's/^/lines=/'\r"),
          ("lines=3", "busybox uname -s\r"),
          ("PrismOS", "ash -c 'echo subshell $((40 + 2))'\r")],
         [("count", "tmpfs: 37/37 checks passed", 2),
          "tmpfs: 37/37 checks passed",
          "fork: all 10 passed",
          "AT_PHNUM = 8",                      # hello-dyn: the loader's auxv
          "float check: ok",
          "prismos",                           # infofetch's host line
          "QEMU Virtual CPU version",          # infofetch's CPU line
          "wc=3", "answer=42", "lines=3",
          "PrismOS",                           # busybox uname -s (the utsname)
          "subshell 42"], 300))

    # ash as a shell around real software: find over the tree, a tar round
    # trip, redirections into /tmp, umask + chmod + ls -l, grep, and a su
    # to the wheel member user (empty password like root) and back.
    scenarios.append(
        ("BIOS userland: find, tar, /tmp writes, umask, su user", bios_cmd,
         [(SHELL_PROMPT, "find /etc -name \"rc*\"\r"),
          ("/etc/init.d/rcS", "tar cf /tmp/a.tar /etc/passwd && tar tf /tmp/a.tar\r"),
          ("removing leading '/'", "echo shell-write > /tmp/s.txt && cat /tmp/s.txt\r"),
          ("shell-write", "umask\r"),
          ("0022", "touch /tmp/u && chmod 644 /tmp/u && ls -l /tmp/u\r"),
          ("-rw-r--r--", "grep root /etc/passwd\r"),
          ("root::0:0:root:/:/usr/bin/sh", "su user\r"),
          (SHELL_PROMPT, "id\r", 1.2),
          ("uid=1000(user) gid=1000(user) groups=1000(user),10(wheel)", "umask\r"),
          ("0022", "touch /tmp/w && chmod 644 /tmp/w && ls -l /tmp/w\r"),
          ("-rw-r--r--", "exit\r"),
          (SHELL_PROMPT, "echo back-to-root\r")],
         ["/etc/init.d/rcS", "/etc/rc",
          "tar: removing leading '/' from member names",
          "etc/passwd",
          "shell-write",
          "0022",
          "-rw-r--r--",
          "root::0:0:root:/:/usr/bin/sh",
          "uid=1000(user) gid=1000(user) groups=1000(user),10(wheel)",
          "back-to-root"], 300))

    # The session's own ttys: ^C reaches a blocked foreground read (fd's
    # stdin prompt here -- fd stays a kshell program for its stdio checks,
    # but the blocked read is real), the prompt comes back, and ^D ends the
    # shell -- busybox init respawns the login (the second prismos login: /
    # motd pair) and the autologin machinery brings a second session up.
    # The prompt occurrences: 1 = first login, 2 = ash's re-prompt after the
    # ^C, 3 = after the echo ran, 4 = the respawned session's prompt.
    scenarios.append(
        ("BIOS userland: ^C interrupts a blocked read; ^D respawns login", bios_cmd,
         [(SHELL_PROMPT, "fd\r"),
          ("type 4 bytes (EXACTLY wxyz):", "\x03"),   # ^C at fd's stdin prompt
          (SHELL_PROMPT, "echo survived-ctrlc\r", 0.3, 2),  # shell took it back
          ("survived-ctrlc", "\x04"),                 # ^D: ash exits, init respawns
          (SHELL_PROMPT, "echo second-session\r", 0.5, 4)],
         ["fd (pid ",
          "type 4 bytes",
          "survived-ctrlc",
          ("count", LOGIN_PROMPT, 2),          # boot + respawn
          ("count", MOTD, 2),                  # its motd, twice
          "second-session"], 240))

    # A substring filter for focused runs (comma-separated): keep the
    # whole-suite default for CI, let a developer ask for one scenario.
    wanted = [f.strip() for f in os.environ.get("PRISM_SCENARIOS", "").split(",") if f.strip()]
    if wanted:
        scenarios = [sc for sc in scenarios if any(w in sc[0] for w in wanted)]
        print("note: running %d scenario(s) (PRISM_SCENARIOS=%s)"
              % (len(scenarios), ",".join(wanted)))

    failed = 0
    ran = set()
    for name, cmd, steps, expect, timeout in scenarios:
        auto = "userland" in name   # the scenarios that must answer the login
        print(f"\n{'=' * 60}", flush=True)
        print(f"[TEST] {name}", flush=True)
        print(f"[TEST] Timeout: {timeout}s", flush=True)
        print(f"{'=' * 60}", flush=True)
        ran.add(name)

        start = time.time()
        try:
            full = cmd()
        except Exception as exc:  # e.g. the mke2fs of a scenario's device
            print("FAIL  %s  (scenario setup: %s)" % (name, exc))
            failed += 1
            continue
        text, missing = run_vm(full, steps, expect, timeout, autologin=auto)
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

    # What the guest wrote must be readable (and consistent) on the host --
    # only when the scenario that did the writing actually ran (a focused
    # PRISM_SCENARIOS run leaves the throwaway disks untouched).
    if "BIOS shell: ext4 root partition (P3, read-write)" in ran \
            and os.path.exists(fswork) and shutil.which("e2fsck"):
        rootimg = extract_part(fswork, 3, os.path.join(tmp, "root.img"))
        r = subprocess.run(["e2fsck", "-fn", rootimg], capture_output=True, text=True)
        d = subprocess.run(["debugfs", "-R", "cat /d/t.txt", rootimg], capture_output=True, text=True)
        if r.returncode != 0 or "persisted by PrismOS" not in d.stdout:
            failed += 1
            print("FAIL  ext4 root after the guest ran\n" + r.stdout + d.stdout)
        else:
            print("ok    ext4 root clean and /d/t.txt readable on the host")

    # Same for the data disk (whole-disk ext4, no partitions to extract).
    if "BIOS shell: ext4 data disk (AHCI port 1, read-write)" in ran \
            and have_fs and os.path.exists(data) and shutil.which("e2fsck"):
        r = subprocess.run(["e2fsck", "-fn", data], capture_output=True, text=True)
        d = subprocess.run(["debugfs", "-R", "cat /from-guest.txt", data], capture_output=True, text=True)
        if r.returncode != 0 or "persisted on the data disk" not in d.stdout:
            failed += 1
            print("FAIL  ext4 data disk after the guest ran\n" + r.stdout + d.stdout)
        else:
            print("ok    ext4 data disk clean and /from-guest.txt readable on the host")
    shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(1 if failed else 0)


# ---------------------------------------------------------------- helpers

def index_boot_tree(image, out_path):
    """Copy `image` and htree-index the boot directories of its P3 (fill the
    dirs past one block, then e2fsck -D).  Returns the new image path, or
    None when the tree could not be prepared."""
    try:
        shutil.copyfile(image, out_path)
        rootimg = out_path + ".p3"
        ents = gpt_entries(out_path)
        first, last = ents[2][1]
        with open(out_path, "rb") as f, open(rootimg, "wb") as o:
            f.seek(first * 512)
            o.write(f.read((last - first + 1) * 512))
        # Fillers: ~48-byte entries, ~80 per directory is past one 4 KiB block.
        filler = out_path + ".filler"
        with open(filler, "wb") as f:
            f.write(b"x" * 512)
        cmds = out_path + ".cmds"
        with open(cmds, "w") as f:
            for d in ("/boot/lua", "/boot/kernel"):
                f.write("cd %s\n" % d)
                for i in range(96):
                    f.write("write %s dx%s%03d\n" % (filler, "z" * 32, i))
        subprocess.run(["debugfs", "-w", "-f", cmds, rootimg],
                       capture_output=True, text=True, check=True)
        r = subprocess.run(["e2fsck", "-fyD", rootimg], capture_output=True, text=True)
        if r.returncode != 0:
            return None
        # Both directories must now carry INDEX_FL (bit 12 of the flags).
        def is_indexed(d):
            s = subprocess.run(["debugfs", "-R", "stat %s" % d, rootimg],
                               capture_output=True, text=True).stdout
            for l in s.splitlines():
                if "Flags:" in l:
                    try:
                        return int(l.split("Flags:")[1].split()[0], 16) & 0x1000 != 0
                    except (ValueError, IndexError):
                        return False
            return False
        if not is_indexed("/boot/lua") or not is_indexed("/boot/kernel"):
            return None
        with open(rootimg, "rb") as f, open(out_path, "r+b") as o:
            o.seek(first * 512)
            o.write(f.read())
        os.unlink(rootimg)
        os.unlink(filler)
        os.unlink(cmds)
        return out_path
    except (OSError, subprocess.CalledProcessError):
        return None


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
