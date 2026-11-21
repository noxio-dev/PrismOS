# System calls

Dispatcher: `sys/kern/src/user/syscall.rs` (the match on the number).
Implementations: `user/{process,signal,futex,sysc,ident,fd}.rs`,
`user/fd/unix.rs` and `fs/fsys.rs`. The raw ring-3 helpers are in
`pkg/include/prism_raw.h`.

## ABI

Linux-like, x86-64 `syscall` instruction:

| Register | Role |
|----------|------|
| `rax` | syscall number on entry, result on return |
| `rdi`, `rsi`, `rdx` | arguments 1, 2, 3 |
| `r10`, `r8`, `r9` | arguments 4, 5, 6 |
| `rcx`, `r11` | clobbered by the CPU |

* Success returns a value `>= 0`; failure returns `-errno` (Linux numbering).
* An unknown number returns `-ENOSYS`. Numbers 0..112 are assigned; 113 and up
  are not.
* A pending signal is delivered between the return value and `sysretq`.
* `exit` never returns. `execve` returns only on failure, and the caller is
  then untouched.
* `ioctl()` is the standard libc call: mlibc's glibc-option `ioctl(fd, req,
  ...)` goes through the port's `Sysdeps<Ioctl>` (syscall 29), `errno` set
  from the negative return.  The request numbers and structures come from the
  Linux UAPI headers the sysroot ships (`<sys/ioctl.h>`, `<linux/{kd,vt,input}.h>`,
  `<drm/drm.h>` -- `ports/mlibc/linux-headers`).  `prism_ioctl()` in
  `prism_raw.h` stays as the raw form, for programs that avoid libc.

## Process and scheduling

| # | Call | Notes |
|---|------|-------|
| 0 | `exit(code)` | |
| 2 | `getpid()` | thread-group id (all threads of a process share it) |
| 4 | `yield()` | hands the CPU to a process of the same priority level or better |
| 5 | `sleep(ms)` | parks the process for at least `ms`, capped at 10 s |
| 6 | `setprio(level)` | levels 0 (best) to 3; can only lower the priority; returns the previous level, `-EINVAL` out of range, `-EPERM` if it would raise it |
| 7 | `execve(path, argv, envp)` | new address space and System V stack; pid, priority, CPU budget and fds are kept |
| 8 | `fork()` | copy-on-write; child resumes with `rax = 0` |
| 9 | `wait(pid, status_ptr, flags)` | `-ECHILD` if no child; `WNOHANG` returns 0 instead of blocking |
| 66 | `wait4(pid, status, flags, rusage)` | `wait` plus the child's rusage (4th argument in `r10`) |
| 67 | `getrusage(who, rusage)` | SELF / CHILDREN / THREAD |
| 49 | `getppid()` | |
| 59 | `setsid()` | |
| 60 | `setpgid(pid, pgid)` | |
| 61 | `getpgid(pid)` | |
| 70 | `setitimer(which, new, old)` | |
| 71 | `getitimer(which, cur)` | |
| 36 | `arch_prctl(code, addr)` | FS/GS base (TLS) |
| 37 | `futex(uaddr, op, val, timeout, uaddr2, val3)` | six arguments |

## Signals

| # | Call | Notes |
|---|------|-------|
| 10 | `kill(pid, sig)` | a negative pid names a process group |
| 11 | `signal(sig, handler)` | basic API |
| 12 | `sigreturn()` | what a handler returns into through the trampoline page; not called directly by programs |
| 38 | `sigaction(sig, act, oldact)` | handler, flags, restorer, 8-byte mask; `SA_SIGINFO` (handler gets `rsi` = `siginfo_t`, `rdx` = `ucontext_t`) and `SA_ONSTACK` are honoured; every delivery pushes the Linux rt frame (`ucontext_t` with gregs and the 512-byte FXSAVE image) that `sigreturn` reads back |
| 39 | `sigprocmask(how, set, oldset)` | |
| 72 | `sigtimedwait(set, info, timeout, sigsetsize)` | `sigsetsize` (4th argument, in `r10`) must be 8, otherwise `-EINVAL` |

## Memory

| # | Call | Notes |
|---|------|-------|
| 21 | `brk(addr)` | one break per process, growing up from the end of the image; on failure returns the old break (like Linux) |
| 22 | `mmap(addr, len, prot, flags, fd, off)` | anonymous (shared or private) and `MAP_PRIVATE` file mappings; `MAP_SHARED` on a file is `-ENOSYS`; `PROT_WRITE \| PROT_EXEC` is refused (W^X); without `MAP_FIXED` the address is a hint that is ignored |
| 23 | `munmap(addr, len)` | absent pages are skipped |
| 40 | `mprotect(addr, len, prot)` | copy-on-write aware |

## Files and directories

| # | Call | Notes |
|---|------|-------|
| 1 | `write(fd, buf, len)` | `SIGPIPE` rule on pipes |
| 13 | `read(fd, buf, len)` | |
| 14 | `open(path, flags)` | `O_NONBLOCK`, `O_CLOEXEC` supported |
| 15 | `close(fd)` | |
| 16 | `lseek(fd, off, whence)` | |
| 17 | `dup(fd)` | |
| 26 | `dup2(old, new)` | |
| 18 | `pipe(int *fds)` | |
| 27 | `fcntl(fd, cmd, arg)` | `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD`, `F_GETFL`, `F_SETFL` (e.g. `O_NONBLOCK`) |
| 28 | `getdents(fd, buf, len)` | |
| 19 | `stat(path, st)` | follows the final symlink |
| 58 | `lstat(path, st)` | does not follow the final symlink |
| 20 | `fstat(fd, st)` | |
| 48 | `statfs(path, buf)` | the mount a path sits on |
| 24 | `chdir(path)` | per-process cwd |
| 62 | `fchdir(fd)` | |
| 25 | `getcwd(buf, size)` | returns the length including the NUL, `-ERANGE` if too small |
| 30 | `unlink(path)` | |
| 31 | `mkdir(path, mode)` | |
| 32 | `rmdir(path)` | |
| 33 | `rename(old, new)` | |
| 34 | `link(old, new)` | |
| 46 | `symlink(target, linkpath)` | |
| 47 | `readlink(path, buf, bufsz)` | |
| 64 | `chmod(path, mode)` | journal-written |
| 65 | `fchmod(fd, mode)` | |
| 68 | `utimensat(dirfd, path, times[2], flags)` | `UTIME_NOW` and `UTIME_OMIT` supported; 4th argument in `r10` |
| 52 | `umask(mask)` | |
| 63 | `fsync(fd)` | flushes the open file's mount |
| 103 | `sync()` | flushes every mounted file system |
| 55 | `mount(source, target, fstype, flags)` | `flags` in `r10` |
| 56 | `umount(target)` | |
| 41 | `poll(fds, nfds, timeout_ms)` | |
| 42 | `select(nfds, r, w, e, tv)` | 4th and 5th arguments in `r10` / `r8` |

## epoll

Implemented in `user/fd/epoll.rs` over the host-tested interest list
`user/epoll_core.rs` (`make epolltest`; `/usr/bin/epolltest` from ring 3).

| # | Call | Notes |
|---|------|-------|
| 109 | `epoll_create1(flags)` | `EPOLL_CLOEXEC` only, else `-EINVAL` |
| 110 | `epoll_ctl(epfd, op, fd, event)` | `ADD` / `DEL` / `MOD`; 4th argument in `r10`. `-EEXIST`, `-ENOENT`, `-EBADF`, `-EINVAL` (epfd not an epoll, watching itself, bad op), `-ELOOP` (nesting cycle or deeper than 4), `-ENOSPC` (256 entries), `-EPERM` on regular files and directories |
| 111 | `epoll_wait(epfd, events, maxevents, timeout_ms)` | 4th argument in `r10`; `maxevents` 1..1024; `-EINTR` when a handler is about to run |
| 112 | `sigaltstack(ss, old_ss)` | Linux `stack_t` (24 bytes: `ss_sp`, `ss_flags` + pad, `ss_size`); `SS_DISABLE` / `SS_ONSTACK`; `-EPERM` while running on the area, `-ENOMEM` below 2048 bytes, `-EINVAL` for bad flags or an area outside user space. Per thread: copied by `fork`, dropped by `execve`, disabled in a new thread |

`struct epoll_event` is the packed 12-byte x86-64 layout (`u32 events`, `u64
data`). Supported: `EPOLLIN/OUT/ERR/HUP/RDHUP` (plus the `RDNORM`/`WRNORM`/band
aliases), `EPOLLET`, `EPOLLONESHOT`, epoll watched by another epoll, any
description `poll` can watch (pipes, sockets, ttys, evdev, DRM, /dev nodes).

Differences from Linux:

* the interest list holds weak references: an entry disappears once every
  descriptor on the watched description is closed (as on Linux), and `DEL` /
  `MOD` match on (fd number, description);
* readiness is sampled when somebody waits (the same probe as `poll`), so
  `EPOLLET` reports the bits that turned on since the *previous wait*: a rise
  and fall between two waits is not seen, and a second write to an already
  readable pipe is not a new edge;
* a timeout above 10 s is capped like every bounded wait here (`poll` does the
  same), and a negative timeout re-arms the cap and waits for an event;
* `epoll_pwait` with a signal mask is `-ENOSYS` (the mlibc sysdep refuses it);
  `EPOLLEXCLUSIVE` / `EPOLLWAKEUP` are accepted and have no effect.

### `ioctl(fd, request, arg)` (29)

Only a few requests are answered:

* Terminal: `TCGETS` / `TCSETS` / `TCSETSW` / `TCSETSF` (behind `tcgetattr` /
  `tcsetattr`), `TIOCSPGRP`
  (foreground claim, which is where ISIG's `^C` is sent), `TIOCGWINSZ`,
  `TIOCSCTTY`.
* Console keyboard (console only; the serial line answers `-ENOTTY`):
  `KDGKBTYPE`, `KDGKBMODE`, `KDSKBMODE`, `KDGKBENT`, `KDSKBENT`, `KDGKBLED`,
  `KDSKBLED`, `KDGETLED`, `KDSETLED`. `KDGKBMODE` writes the mode through the
  pointer `arg`; `KDSKBMODE` takes the mode itself as `arg`. Mode `K_MEDIUMRAW` (2) delivers
  one byte per key event: the Linux keycode, with bit 7 set on release.
* Virtual terminals (console only): the `VT_*` requests (`VT_OPENQRY`,
  `VT_GETMODE`, `VT_SETMODE`, `VT_GETSTATE`, `VT_ACTIVATE`, `VT_WAITACTIVE`, ...)
  and `KDGETMODE` / `KDSETMODE` (`KD_GRAPHICS` marks the VT as drawn by a client).
* A DRM descriptor (`/dev/dri/card0`, `sys/kern/src/drm/`): the ioctl space of
  magic `'d'` -- version, caps, master, GEM, dumb buffers, the KMS set (CRTC,
  plane, connector, framebuffer, page flip, atomic).  `mmap` of the fd maps a
  dumb buffer at the offset `DRM_IOCTL_MODE_MAP_DUMB` returned.
* An evdev descriptor (`/dev/input/event0`, `event1`, `sys/kern/src/input/`):
  the ioctl space of type `'E'` (`EVIOCGVERSION`, `EVIOCGBIT`, `EVIOCGKEY`,
  `EVIOCGRAB`, ...); `read` returns 24-byte `struct input_event`.
* `FIONREAD` / `FIONBIO` on sockets.

Anything else returns `-ENOTTY`.

## Time and system

| # | Call | Notes |
|---|------|-------|
| 3 | `uptime_ms()` | |
| 43 | `clock_gettime(clk, ts)` | nanosecond clock / RTC time |
| 53 | `gettimeofday(tv, tz)` | |
| 54 | `nanosleep(req, rem)` | |
| 35 | `uname(buf)` | the six classic fields: `PrismOS`, node name, version, build, `x86_64`, `(none)` |
| 69 | `sethostname(name, len)` | |
| 73 | `getrandom(buf, len, flags)` | |
| 57 | `reboot(cmd)` | flushes every file system first. Linux commands only: `0x01234567` restart, `0x4321FEDC` power off, `0xCDEF0123` halt; anything else is `-EINVAL` |

## Identity and permissions

Implemented in `user/ident.rs`.

| # | Call |
|---|------|
| 50 | `getuid()` |
| 51 | `getgid()` |
| 76 | `geteuid()` |
| 77 | `getegid()` |
| 74 | `setuid(uid)` |
| 75 | `setgid(gid)` |
| 78 | `setreuid(ruid, euid)` |
| 79 | `setregid(rgid, egid)` |
| 80 | `setresuid(r, e, s)` |
| 81 | `setresgid(r, e, s)` |
| 82 | `getresuid(r*, e*, s*)` |
| 83 | `getresgid(r*, e*, s*)` |
| 84 | `getgroups(size, list)` |
| 85 | `setgroups(size, list)` |
| 86 | `chown(path, uid, gid)` |
| 87 | `fchown(fd, uid, gid)` |
| 88 | `lchown(path, uid, gid)` |
| 89 | `access(path, mode)` |

## Sockets (AF_UNIX stream only)

Implemented in `user/fd/unix.rs`.

| # | Call | Notes |
|---|------|-------|
| 90 | `socket(domain, type, protocol)` | |
| 91 | `socketpair(domain, type, protocol, sv)` | 4th argument in `r10` |
| 92 | `bind(fd, addr, len)` | |
| 93 | `listen(fd, backlog)` | |
| 94 | `accept4(fd, addr, len, flags)` | `flags` in `r10` |
| 95 | `connect(fd, addr, len)` | |
| 96 | `sendmsg(fd, msg, flags)` | |
| 97 | `recvmsg(fd, msg, flags)` | |
| 98 | `shutdown(fd, how)` | |
| 99 | `getsockname(fd, addr, len)` | |
| 100 | `getpeername(fd, addr, len)` | |
| 101 | `getsockopt(fd, level, name, val, len)` | 4th and 5th in `r10` / `r8` |
| 102 | `setsockopt(fd, level, name, val, len)` | 4th and 5th in `r10` / `r8` |

## Threads (phase 4.2)

| # | Call | Notes |
|---|------|-------|
| 104 | `clone(flags, stack, ptid, ctid, tls)` | Linux x86-64 register order (`ctid` in `r10`, `tls` in `r8`). Needs `CLONE_VM\|SIGHAND\|THREAD`; fork-style flag sets give `-ENOSYS`. `CLONE_SETTLS`, `CLONE_PARENT_SETTID`, `CLONE_CHILD_SETTID`, `CLONE_CHILD_CLEARTID`, `CLONE_FILES` honoured. Returns the new tid; the child gets 0 on `stack` |
| 105 | `gettid()` | |
| 106 | `thread_exit()` | ends the caller only (clears `clear_child_tid` + futex wake); the last thread ends the process |
| 107 | `set_tid_address(ptr)` | returns the tid |
| 108 | `tgkill(tgid, tid, sig)` | |

`exit()` ends every thread of the group; `kill`/`wait` act on the group.

## Framebuffer (PrismOS specific)

| # | Call | Notes |
|---|------|-------|
| 44 | `fb_info(struct prism_fb_info *)` | geometry and pixel format of the console surface |
| 45 | `fb_blit(src, w, h, stride, x, y)` | copies caller-packed pixels onto the console surface; six arguments |

`struct prism_fb_info` and the raw wrappers live in `pkg/include/prism_raw.h`.

## Adding a syscall

1. Implement it in the matching file under `sys/kern/src/user/`.
2. Add an arm to the match in `syscall.rs` and a line to its header comment.
3. Add it to this table.
4. A userspace package that uses it must list the number in the `syscalls`
   array of its `prism.toml` (`[verify]`), or the build's loader-contract check
   fails.