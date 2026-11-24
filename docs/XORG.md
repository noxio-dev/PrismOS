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
- [ ] `SIGIO`: `F_SETOWN`, `F_SETSIG`, `O_ASYNC`

### 2.2 Shared memory
- [x] `MAP_SHARED` on tmpfs files (file frames mapped directly; other file systems answer `-ENODEV`, a real page cache stays optional)
- [x] `ftruncate` / `truncate` / `fallocate` (syscalls 113..115)
- [x] `/dev/shm` (tmpfs, 32 MiB) + `memfd_create` (116); `shm_open` = mlibc's open on `/dev/shm`, **to verify at mlibc build time**; unlinked-but-open files stay alive
- [ ] SysV `shmget` / `shmat` / `shmdt` / `shmctl` (MIT-SHM)

### 2.3 Event descriptors
- [x] `eventfd` (`eventfd2`, 124: counter, semaphore mode, O_NONBLOCK, poll / epoll readiness; host tests `make eventfdtest`; **mlibc sysdep to wire**)
- [ ] `signalfd`
- [x] `timerfd` (`timerfd_create` / `timerfd_settime` / `timerfd_gettime`, 126..128: CLOCK_REALTIME / MONOTONIC / BOOTTIME, one-shot and periodic, absolute and relative, poll / epoll readiness; host tests `make timerfdtest`; **mlibc sysdep to wire**)
- [x] `epoll_pwait` (125: mask swap, restored after the handler on `-EINTR` via `SignalState::saved_mask`; **mlibc sysdep to wire**, it still refuses the call)

### 2.4 Miscellaneous syscalls
- [x] `getrlimit` / `setrlimit` / `prlimit64` (117..119, kernel side: stored and
      reported, **not enforced**; defaults are the real system sizes; **mlibc sysdep to wire**)
- [x] `mlock` / `munlock` (120, 121: a range check, there is no swap; **mlibc sysdep to wire**)
- [x] `sched_getaffinity` / `sched_setaffinity` (122, 123: one CPU, mask `0x1`);
      `sched_yield` is the existing syscall 4 (`Yield` sysdep already there);
      **mlibc sysdep to wire**
- [ ] `O_DIRECTORY`, `*at()` calls (`openat`, `fstatat`, ...) beyond `AT_FDCWD`

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
- [ ] check `dlopen` / `dlsym` in the mlibc port (Xorg's `.so` modules)
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