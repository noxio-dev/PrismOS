# Xorg roadmap for PrismOS

Status and plan for running an X server (Xorg) on PrismOS.
Basis: analysis of the repository as read (Rust kernel, mlibc port,
`docs/ROADMAP.md`, `docs/SYSCALLS.md`).

## 1. Already in place

- [x] **DRM/KMS** `/dev/dri/card0` (simpledrm-like: 1 CRTC, 1 primary plane,
      dumb buffers, page flip, atomic) -- `sys/kern/src/drm/`
- [x] **evdev** `/dev/input/event0` (keyboard) and `event1` (PS/2 mouse)
- [x] **VT** `/dev/tty1..6`, `VT_*` / `KD_*` ioctls
- [x] **Minimal sysfs**: `/sys/dev/char`, `/sys/class/{drm,input,tty}`,
      `prismfb` platform device
- [x] **AF_UNIX `SOCK_STREAM`**: named + abstract, `SCM_RIGHTS`, `SO_PEERCRED`
- [x] `poll` / `select` / `epoll` (109-111)
- [x] mlibc, `ld.so`, `libc.so`, threads (`clone`, futex, TLS)
- [x] `x86_64-prismos` cross toolchain (binutils + gcc)

## 2. Missing kernel / ABI pieces

### 2.1 Signals
- [x] `SA_SIGINFO` (siginfo + ucontext passed to the handler)
- [x] `sigaltstack` + `SA_ONSTACK` (syscall 112)
- [x] `SIGIO`: `F_SETOWN`, `F_SETSIG`, `O_ASYNC` (edge-triggered, probed in process context at syscall return and after a park; mlibc `Fcntl` already passes the commands through; host tests `make sigiotest`)

### 2.2 Shared memory
- [x] `MAP_SHARED` on tmpfs files (file frames mapped directly; other file systems answer `-ENODEV`, a real page cache stays optional)
- [x] `ftruncate` / `truncate` / `fallocate` (syscalls 113..115)
- [x] `/dev/shm` (tmpfs, 32 MiB) + `memfd_create` (116); `shm_open` = mlibc's open on `/dev/shm`, **to verify at mlibc build time**; unlinked-but-open files stay alive
- [x] SysV `shmget` / `shmat` / `shmdt` / `shmctl` (141..144, MIT-SHM: segments are anonymous `/dev/shm` files mapped `MAP_SHARED` like `mmap` does, so attachers and fork children share the same frames; `IPC_RMID` is deferred to the last detach, attachments follow `fork` / `munmap` / `exit` / `execve`; `IPC_STAT` / `IPC_SET` / `IPC_INFO` / `SHM_INFO` / `SHM_STAT`, `SHM_RDONLY` / `SHM_RND` / `SHM_REMAP`; rules in `user/shm_core.rs`, host tests `make shmsysvtest`; mlibc sysdeps `Shmget` / `Shmat` / `Shmdt` / `Shmctl` wired)

### 2.3 Event descriptors
- [x] `eventfd` (`eventfd2`, 124: counter, semaphore mode, O_NONBLOCK, poll / epoll readiness; host tests `make eventfdtest`; mlibc sysdep `EventfdCreate` wired)
- [x] `signalfd` (`signalfd4`, 129: mask update on an existing fd, 128-byte `signalfd_siginfo` records, poll / epoll readiness; blocked signals now stay pending whatever their disposition; host tests `make signalfdtest`; mlibc sysdep `SignalfdCreate` wired, mask shifted like every other)
- [x] `timerfd` (`timerfd_create` / `timerfd_settime` / `timerfd_gettime`, 126..128: CLOCK_REALTIME / MONOTONIC / BOOTTIME, one-shot and periodic, absolute and relative, poll / epoll readiness; host tests `make timerfdtest`; mlibc sysdeps `TimerfdCreate` / `TimerfdSettime` / `TimerfdGettime` wired)
- [x] `epoll_pwait` (125: mask swap, restored after the handler on `-EINTR` via `SignalState::saved_mask`; mlibc `EpollPwait` now sends a real mask to 125 (shifted to the kernel's bit order); a null mask stays the plain 111)

### 2.4 Miscellaneous syscalls
- [x] `getrlimit` / `setrlimit` / `prlimit64` (117..119, kernel side: stored and
      reported, **not enforced**; defaults are the real system sizes; mlibc `GetRlimit` / `SetRlimit` wired -- mlibc has no
      prlimit tag, so 119 stays reachable only through `syscall()`)
- [x] `mlock` / `munlock` (120, 121: a range check, there is no swap; mlibc `Mlock` / `Munlock`
      wired; no `mlockall` tag, no kernel call)
- [x] `sched_getaffinity` / `sched_setaffinity` (122, 123: one CPU, mask `0x1`);
      `sched_yield` is the existing syscall 4 (`Yield` sysdep already there);
      mlibc `GetAffinity` / `SetAffinity` wired (the rest of the caller's `cpu_set_t` is zeroed;
      mlibc v7.0.0's `sched_setaffinity` aborted with "Not implemented", the patch now routes
      it to the sysdep); the `*_thread*` affinity tags stay absent (the kernel takes a pid)
- [x] mlibc `Truncate` / `Fallocate` / `MemfdCreate` (114..116) wired next to `Ftruncate`
- [x] `O_DIRECTORY` (0x2000, kernel `open`; mlibc `OpenDir` uses it, no stat round trip)
- [x] `*at()` calls (130..140: `openat`, `fstatat`, `mkdirat`, `unlinkat`, `readlinkat`, `renameat`, `symlinkat`, `linkat`, `faccessat`, `fchmodat`, `fchownat`; the dirfd base is the path the directory was opened by; mlibc `Openat` / `Mkdirat` / `Readlinkat` / `Renameat` / `Symlinkat` / `Linkat` / `Fchmodat` added, `Stat` fd_path / `Faccessat` / `Fchownat` / `Unlinkat` now send any dirfd to the kernel; `utimensat` stays `AT_FDCWD` only)

### 2.4b Networking / hardware discovery
- [ ] `SOCK_DGRAM` / `SOCK_SEQPACKET` on AF_UNIX
- [ ] `AF_NETLINK` (uevent) -- only if a real libudev/libinput is wanted
- [ ] `AF_INET` -- optional (`Xorg -nolisten tcp`)
- [ ] `/sys/bus/pci` (only if a `libpciaccess`-based driver is targeted)
- [ ] udev: either `-Dudev=false`, or a static fake libudev

## 3. Userspace ports

Suggested build order:

1. [ ] `xorgproto`, `xtrans`, `util-macros`
2. [ ] `libdrm` (no GPU drivers, e.g. `-Dintel=disabled` etc.)
3. [ ] `pixman`
4. [ ] `freetype`, `libfontenc`, `libXfont2`
5. [ ] `libxcvt`, `libxkbfile`
6. [ ] `xkbcomp` + `xkeyboard-config`
7. [ ] `xorg-server` (see 3.1)
8. [ ] `libxcb`, `libX11`, `libXext`, ... (client libraries)
9. [ ] test clients: `xdpyinfo`, `xev`, `xterm` or `xclock`
10. [ ] build tooling: cross `meson` / `ninja`, `pkg-config` (the existing
        `cross.txt` in `ports/toolchain` is the starting point)

### 3.1 Target `xorg-server` configuration

```
meson setup build \
  -Dudev=false -Dudev_kms=false \
  -Dglamor=false -Ddri3=false -Ddri2=false -Dglx=false \
  -Dsecure-rpc=false -Dsystemd_logind=false \
  -Dxorg=true -Dxephyr=false -Dxnest=false -Dxvfb=false \
  -Dxkb_dir=/usr/share/X11/xkb \
  -Dxkb_output_dir=/var/lib/xkb
```

Test launch: `Xorg -nolisten tcp -nohotplug -noreset :0 vt1`
with a minimal `xorg.conf` (`Driver "modesetting"`, `kmsdev
"/dev/dri/card0"`, evdev for input, `AutoAddDevices off`).

### 3.2 Module loading
- [x] `dlopen` / `dlsym` / `dlclose` / `dlerror` / `dladdr` in the mlibc port (Xorg's `.so` modules).
      Nothing to write in the library: mlibc already carries them -- `dlfcn.cpp` in `libc.so` wraps the
      rtld's `__dlapi_*` exports in `ld.so`, and every sysdep the rtld calls (`Open`, `Read`, `Seek`,
      `VmMap`, `Close`, `TcbSet`, `Stat`) is implemented in `sysdeps/prismos` (not an `ENOSYS`).  The rtld binds eagerly, so a plain
      DSO needs no lazy-binding support; `-ldl` is mlibc's empty `libdl`, the symbols are in `libc.so`;
      the default search path is `/lib`, `/lib64`, `/usr/lib`, `/usr/lib64` (+ `LD_LIBRARY_PATH`).
      What was missing was a proof: `ports/toolchain/dltest` (`make dltest`, `build.sh stage dl`) builds
      `/usr/bin/dltest` (a dynamic PIE linked `--export-dynamic`) and `/usr/lib/libdlplug.so`, a plug-in
      shaped like an Xorg module (constructor, initialised data, relocated function-pointer table, imports
      from libc **and from the host executable**, a general-dynamic TLS variable); 16 checks, run by
      `boot_test.py`'s userland scenario (`dltest: 16/16 checks passed`).  The stage also asserts the
      exports and the ELF shape.  **Still to run under QEMU**: the sources compile against the port's
      headers and the check logic passes against glibc, but no PrismOS boot has executed it yet -- the
      TLS and host-import checks are the likeliest to show a kernel / rtld gap.
- [ ] plan B: build `modesetting`, `fb`, `evdev` statically into the server

## 4. Validation milestones

| Milestone | Test |
|-----------|------|
| M1 | a test program installs an `SA_SIGINFO` handler and receives `si_code` |
| M2 | two processes share a page via `shm_open` + `MAP_SHARED` |
| M3 | `libdrm`: `drmModeGetResources()` returns the virtual connector |
| M4 | `Xorg :0` starts and shows a gray background (modesetting, shadow fb) |
| M5 | `xdpyinfo` connects through `/tmp/.X11-unix/X0` |
| M6 | keyboard and mouse respond (`xev`) |
| M7 | `xterm` or `xclock` shows a window |
| M8 | MIT-SHM active (`xdpyinfo` lists `MIT-SHM`) |

## 5. After Xorg

- [ ] C++ (phase 4.3) for Mesa (llvmpipe/softpipe), then XFCE
- [ ] Mesa + DRI3 + `memfd` / PRIME
- [ ] a light window manager (`twm`, `openbox`) before XFCE
- [ ] a real GPU driver (virtio-gpu) to move off the firmware framebuffer