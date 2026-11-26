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
* An unknown number returns `-ENOSYS`. Numbers 0..125 are assigned; 126 and up
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
| 22 | `mmap(addr, len, prot, flags, fd, off)` | anonymous (shared or private) and `MAP_PRIVATE` file mappings; `MAP_SHARED` on a file works on tmpfs only (`-ENODEV` elsewhere; never executable; range must lie inside the file); `PROT_WRITE \| PROT_EXEC` is refused (W^X); without `MAP_FIXED` the address is a hint that is ignored |
| 23 | `munmap(addr, len)` | absent pages are skipped |
| 40 | `mprotect(addr, len, prot)` | copy-on-write aware |

## Files and directories

| # | Call | Notes |
|---|------|-------|
| 1 | `write(fd, buf, len)` | `SIGPIPE` rule on pipes |
| 13 | `read(fd, buf, len)` | |
| 14 | `open(path, flags)` | `O_NONBLOCK`, `O_CLOEXEC`, `O_DIRECTORY` (0x2000: `-ENOTDIR` unless the path is a directory, device nodes included; with `O_CREATE` `-EINVAL`; not kept in `F_GETFL`) supported |
| 15 | `close(fd)` | |
| 16 | `lseek(fd, off, whence)` | |
| 17 | `dup(fd)` | |
| 26 | `dup2(old, new)` | |
| 18 | `pipe(int *fds)` | |
| 27 | `fcntl(fd, cmd, arg)` | `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD`, `F_GETFL`, `F_SETFL` (`O_NONBLOCK`, `O_APPEND`, `O_ASYNC`), `F_SETOWN` / `F_GETOWN` / `F_SETSIG` / `F_GETSIG` (SIGIO, below) |
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
| 113 | `ftruncate(fd, len)` | open, writable regular file; tmpfs growth adds holes (no memory until written or mapped), a shrink frees the frames and zeroes the tail; `-EINVAL` on pipes/sockets/devices |
| 114 | `truncate(path, len)` | same, by path, with the VFS write-permission check |
| 115 | `fallocate(fd, mode, off, len)` | mode 0 and `FALLOC_FL_KEEP_SIZE` only; other modes `-EOPNOTSUPP` |
| 116 | `memfd_create(name, flags)` | anonymous file on `/dev/shm` (created then unlinked), `MFD_CLOEXEC` honoured, `MFD_ALLOW_SEALING` accepted but no seals; `-ENOSYS` without `/dev/shm` |
| 124 | `eventfd2(initval, flags)` | a 64-bit counter descriptor (`user/fd/eventfd.rs`, rules in `user/eventfd_core.rs`, `make eventfdtest`); `EFD_SEMAPHORE` (1), `EFD_NONBLOCK`, `EFD_CLOEXEC`: both the Linux values (04000, 02000000) and this port's `O_NONBLOCK` / `O_CLOEXEC` (0100, 0200) are accepted; any other bit is `-EINVAL` |
| 125 | `epoll_pwait(epfd, events, maxevents, timeout_ms, sigmask, sigsetsize)` | 4th..6th arguments in `r10`, `r8`, `r9`; `sigmask` NULL is plain `epoll_wait`; `sigsetsize` must be 8, else `-EINVAL` |
| 126 | `timerfd_create(clockid, flags)` | a timer descriptor (`user/fd/timerfd.rs`, rules in `user/timerfd_core.rs`, `make timerfdtest`); `CLOCK_REALTIME` (0), `CLOCK_MONOTONIC` (1), `CLOCK_BOOTTIME` (7, same as monotonic), any other clock `-EINVAL`; `TFD_NONBLOCK` / `TFD_CLOEXEC` accepted with both the Linux and this port's values, like eventfd; any other flag bit `-EINVAL` |
| 127 | `timerfd_settime(fd, flags, new, old)` | 4th argument in `r10`; `new` / `old` are `struct itimerspec` (32 bytes: interval then value); `old` may be NULL; `TFD_TIMER_ABSTIME` (1), `TFD_TIMER_CANCEL_ON_SET` (2, accepted, nothing to cancel); a bad timespec is `-EINVAL`; value 0 disarms |
| 128 | `timerfd_gettime(fd, cur)` | time left until the next expiry (0 when disarmed) and the interval |
| 129 | `signalfd4(fd, mask, sizemask, flags)` | `flags` (4th argument) in `r10`; `fd` -1 makes a new descriptor, another `fd` must be a signalfd and only gets a new mask; `sizemask` must be 8, else `-EINVAL`; `SFD_NONBLOCK` / `SFD_CLOEXEC` accepted with both the Linux and this port's values, like eventfd; any other flag bit `-EINVAL` (`user/fd/signalfd.rs`, rules in `user/signalfd_core.rs`, `make signalfdtest`) |

| 130 | `openat(dirfd, path, flags)` | `open` with a relative path resolved against `dirfd` (`AT_FDCWD` = -100, or an open directory; an absolute path ignores it); same flags as 14 |
| 131 | `fstatat(dirfd, path, buf, flags)` | flags (4th argument) in `r10`: `AT_SYMLINK_NOFOLLOW` (= lstat), `AT_EMPTY_PATH` (= fstat of `dirfd`) |
| 132 | `mkdirat(dirfd, path, mode)` | as 31 |
| 133 | `unlinkat(dirfd, path, flags)` | `AT_REMOVEDIR` = rmdir, else unlink |
| 134 | `readlinkat(dirfd, path, buf, size)` | `size` in `r10`; as 47 |
| 135 | `renameat(olddirfd, old, newdirfd, new)` | `new` in `r10`; no flags (renameat2 stays absent) |
| 136 | `symlinkat(target, newdirfd, linkpath)` | the target is stored verbatim, only the link path is resolved |
| 137 | `linkat(olddirfd, old, newdirfd, new, flags)` | `new` in `r10`, `flags` in `r8`; only 0 (`AT_SYMLINK_FOLLOW` is `-EINVAL`) |
| 138 | `faccessat(dirfd, path, mode, flags)` | flags in `r10`: `AT_EACCESS` (moot, real ids are used) and `AT_SYMLINK_NOFOLLOW` accepted |
| 139 | `fchmodat(dirfd, path, mode, flags)` | flags in `r10`, only 0 |
| 140 | `fchownat(dirfd, path, uid, gid, flags)` | `gid` in `r10`, `flags` in `r8`: `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH` (= fchown) |
| 141 | `shmget(key, size, flg)` | System V shared memory; `IPC_PRIVATE` / `IPC_CREAT` / `IPC_EXCL`, size up to 32 MiB (the size of `/dev/shm`) |
| 142 | `shmat(id, addr, flg)` | returns the mapping's address; `SHM_RDONLY`, `SHM_RND`, `SHM_REMAP`; `SHM_EXEC` is not honoured (shared mappings are never executable) |
| 143 | `shmdt(addr)` | `addr` must be the start of an attachment |
| 144 | `shmctl(id, cmd, buf)` | `IPC_RMID` (deferred to the last detach), `IPC_SET`, `IPC_STAT`, `IPC_INFO`, `SHM_INFO`, `SHM_STAT[_ANY]`, `SHM_LOCK/UNLOCK` (root, no-op); Linux x86-64 `shmid64_ds` |

The *at() family (`user/fd/at.rs`): a `dirfd` other than `AT_FDCWD` must be a descriptor of an open
directory (`-EBADF` if closed, `-ENOTDIR` if not a directory). The base is the path the directory was
opened by, so a directory renamed after the open keeps resolving through its old name (the one
deviation from Linux). `utimensat` (68) predates the family and still serves `AT_FDCWD` only.

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
* `epoll_pwait` swaps the blocked mask for the call. When a signal ends the
  wait (`-EINTR`) the temporary mask stays until the handler has run and
  `sigreturn` restores the caller's own (`SignalState::saved_mask`); the
  mlibc sysdep still has to be wired to syscall 125;
  `EPOLLEXCLUSIVE` / `EPOLLWAKEUP` are accepted and have no effect.

### eventfd (124)

`read` and `write` move exactly 8 bytes (a shorter buffer is `-EINVAL`). `read`
returns the whole counter and resets it to 0 (in `EFD_SEMAPHORE` mode: returns 1
and subtracts 1) and parks while it is 0. `write` adds the value, parks while
the sum would pass `u64::MAX - 1`, and rejects `u64::MAX` with `-EINVAL`.
`O_NONBLOCK` answers `-EAGAIN` at once instead of parking. A bad user pointer on
`read` puts the counter back. Readable when the counter is above 0, writable
when a write of 1 would fit, so `poll`, `select` and `epoll` all see it. A
parked reader or writer is woken by the shared `unix::notify`.

Differences from Linux: a wait is capped like every blocking call here (10 s,
then `-EAGAIN`); `fsync` is `-EINVAL`; `fstat` reports a generic 0600 inode and
`/proc/<pid>/fd` names it `anon_inode:[eventfd]`.

### timerfd (126..128)

`read` returns exactly 8 bytes: the number of expirations since the last read
(a shorter buffer is `-EINVAL`). A one-shot timer ends up disarmed; a periodic
one counts every missed interval and keeps its phase, so a late read does not
drift. `settime` forgets unread expirations and returns the previous setting;
the interval is kept even when the timer is disarmed. `write` is `-EINVAL`.
Readable when the count is above 0, never writable, so `poll`, `select` and
`epoll` all see it.

There is no kernel timer behind the descriptor: it is a deadline on a clock,
and whoever looks (read, poll probe, gettime, settime) counts what has expired.
A blocked `read` sleeps to the exact deadline; a `poll` / `epoll` waiter notices
an expiry at its next probe (the usual 10 ms park). `O_NONBLOCK` answers
`-EAGAIN`; a wait is capped like every blocking call here (10 s, then `-EAGAIN`).

Differences from Linux: no `CLOCK_*_ALARM`, `TFD_TIMER_CANCEL_ON_SET` never
cancels (the wall clock cannot be stepped), `fsync` is `-EINVAL`, and
`/proc/<pid>/fd` names it `anon_inode:[timerfd]`.

### signalfd (129)

`read` returns whole 128-byte `struct signalfd_siginfo` records (`ssi_signo`,
`ssi_code`, `ssi_pid`, `ssi_uid` filled, the rest zero): one per pending signal
of the mask, lowest number first, as many as the buffer holds; a buffer under
128 bytes is `-EINVAL`. The signal is consumed, so it is not delivered
afterwards. Readable when a signal of the mask is pending, never writable
(`write` is `-EINVAL`), so `poll`, `select` and `epoll` all see it.
`SIGKILL` and `SIGSTOP` never stick in the mask. As on Linux the signals must
be blocked (`sigprocmask`) for a read to see them: an unblocked one is
delivered or acted on first. The pending set read is the calling process's.

**Blocked signals now stay pending.** A signal raised while blocked is queued
whatever its disposition (`SignalState::queues`, used by `kill`, `tgkill`, the
tty, `SIGALRM`, `SIGPIPE`), as POSIX says; before, only a signal with a handler
was, and a blocked one with the default action killed the process anyway. When
such a signal is unblocked, `deliver` applies its default action (terminating
ones end the process, the others are discarded). `sigtimedwait` benefits too.

Differences from Linux: signals 1..31 only (no real-time signals), the
`ssi_*` fields beyond uid are not filled, and `/proc/<pid>/fd` names it
`anon_inode:[signalfd]`.

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

## Limits, memory locking, CPU affinity

Implemented in `user/sysc.rs`; the limit table and its rules are in
`user/rlimit_core.rs` (`make rlimittest`).

| # | Call | Notes |
|---|------|-------|
| 117 | `getrlimit(resource, rlim)` | `struct rlimit` = two u64 (soft, hard); `-EINVAL` for a resource >= 16 |
| 118 | `setrlimit(resource, rlim)` | `-EINVAL` if soft > hard; `-EPERM` if a caller with euid != 0 raises the hard limit; RLIMIT_NOFILE's hard limit can never pass 64 (the descriptor table), even for root |
| 119 | `prlimit64(pid, resource, new, old)` | `old` in `r10`; `pid` must be 0 or the caller's own, otherwise `-EPERM`; the old value is copied out after the new one is applied |
| 120 | `mlock(addr, len)` | there is no swap, so this is a check: `-ENOMEM` if the page-rounded range is not fully mapped or an unprivileged caller exceeds the soft RLIMIT_MEMLOCK, `-EPERM` if that limit is 0; `len` 0 succeeds |
| 121 | `munlock(addr, len)` | same range check, nothing to undo |
| 122 | `sched_getaffinity(pid, len, mask)` | `len` must be a multiple of 8, at least 8; writes an 8-byte mask (CPU 0 only) and returns 8; `-ESRCH` for an unknown pid |
| 123 | `sched_setaffinity(pid, len, mask)` | accepted when the mask contains CPU 0, `-EINVAL` otherwise; pins nothing |

Differences from Linux:

* limits are stored, reported and copied by `fork` / `clone`, but **not
  enforced**: lowering RLIMIT_NOFILE does not stop `open` before 64 descriptors,
  RLIMIT_AS does not stop `mmap`. The defaults are the real system sizes
  rather than Linux's: RLIMIT_STACK 16 KiB (the fixed user stack),
  RLIMIT_NOFILE 64 (`MAX_FDS`), RLIMIT_CORE 0 soft / unlimited hard, the rest
  unlimited;
* `mlock` / `munlock` have no effect on residency (nothing is ever swapped);
* user processes run on one CPU, so the affinity mask is always `0x1`;
* `sched_yield` is syscall 4 (`yield`); there is no `sched_setscheduler` /
  `sched_getparam` family.

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
## SIGIO (fcntl `F_SETOWN`, `F_SETSIG`, `O_ASYNC`)

`F_SETFL` accepts `O_ASYNC` (the port's `0x400`, and Linux's `0x2000`, kept as `0x400`).
`F_SETOWN` takes a pid (> 0) or a process group (< 0), 0 clears it; `F_SETSIG` takes 0 (plain
`SIGIO`, 29) or a signal 1..31 (real-time numbers: `-EINVAL`). State lives on the open file
description, so dups and forks share it. The signal is sent to the owner on the
not-readable -> readable edge (`si_code` = `SI_SIGIO`, -5), probed in process context only:
at every syscall return and after each park. A process that neither makes a syscall nor
parks hears nothing until it does, and one signal goes out per edge, not per event: drain
to `EAGAIN`. `SIGIO` terminates by default; `SIGURG` is defined and ignored. Host tests:
`make sigiotest`.
