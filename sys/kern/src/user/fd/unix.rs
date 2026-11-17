// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// AF_UNIX stream sockets: the syscall half (the state machine is in
// user/unix_core.rs, host-tested by tests/unix_host.rs).
//
// This module is a *child* of user/fd.rs on purpose: a socket is one more
// kind of open file description (`Kind::Unix`), and a child module sees the
// descriptor layer's private parts (the fd table, the flags cell, the park /
// copy helpers) without widening any of them.
//
// Syscalls (the ABI keeps growing Linux-shaped; the numbers are PrismOS's):
//
//    90 socket(domain, type, protocol)       91 socketpair(domain, type, proto, int sv[2])
//    92 bind(fd, addr, len)                  93 listen(fd, backlog)
//    94 accept4(fd, addr, int *len, flags)   95 connect(fd, addr, len)
//    96 sendmsg(fd, msghdr *, flags)         97 recvmsg(fd, msghdr *, flags)
//    98 shutdown(fd, how)                    99 getsockname(fd, addr, int *len)
//   100 getpeername(fd, addr, int *len)     101 getsockopt(fd, level, name, val, int *len)
//   102 setsockopt(fd, level, name, val, len)
//
// plus read(2) / write(2) / poll / select / fstat / close / dup / fork /
// fcntl / ioctl(FIONREAD, FIONBIO) on the descriptor, which all go through the
// existing descriptor layer.  `send` / `recv` / `sendto` / `recvfrom` are
// mlibc's business: it turns them into sendmsg / recvmsg.
//
// What is here: SOCK_STREAM in the AF_UNIX family; named sockets on the file
// system (bind creates the node, connect resolves it) and in the abstract
// namespace (sun_path[0] == 0); listen backlog; non-blocking accept / connect /
// send / recv; shutdown; SCM_RIGHTS (file descriptions in flight, up to 253
// per message, MSG_CMSG_CLOEXEC); SO_PEERCRED; MSG_PEEK / MSG_DONTWAIT /
// MSG_NOSIGNAL / MSG_WAITALL.
//
// What is not (answered honestly, never faked): SOCK_DGRAM / SEQPACKET
// (-ESOCKTNOSUPPORT), other families (-EAFNOSUPPORT), SCM_CREDENTIALS,
// out-of-band data, sendto on a connected socket with an address, and the
// socket *type* of the name on disk (a bound path stats as a regular file --
// the VFS has no socket inode type yet; connect() does not check write
// permission on it either).
//
// Blocking follows the house rule of fd.rs: every wait is bounded (10 s, then
// -EAGAIN, which a library treats as "try again"), interruptible (-EINTR when
// a handler is about to run) and never holds a RefCell borrow across the park
// -- another process runs while this one sleeps.  Unlike pipes, a socket
// operation that changes what somebody waits for (data, room, a close, a
// queued connection) *wakes* the parked processes at once (`notify`), so a
// request / reply round trip does not pay the 10 ms park quantum.
//
// Re-entrancy: dropping a description can close a socket, which touches its
// channels.  Nothing here drops an `Rc<FileDesc>` while a channel is borrowed
// (the core hands discarded rights back for exactly that reason).

use alloc::rc::{Rc, Weak};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::addr_of_mut;

use super::{copy_in, copy_out, fs_errno, park_once, uptime_ms, FileDesc, Kind};
use super::{EAGAIN, EINTR, EMFILE, ENOENT, WAIT_MAX_MS};
use crate::fs::vfs::{O_NONBLOCK, O_READ, O_WRITE};
use crate::user::process;
use crate::user::signal;
use crate::user::unix_core::{self, Endpoint, Listener, PeerCred, CHAN_CAP};
use crate::user::usermode::{EBADF, EFAULT, EINVAL};

type Ep = Endpoint<Rc<FileDesc>>;

// ------------------------------------------------------------------ errno
const EPIPE: i64 = 32;
const ENOTSOCK: i64 = 88;
const EMSGSIZE: i64 = 90;
const ENOPROTOOPT: i64 = 92;
const EPROTONOSUPPORT: i64 = 93;
const ESOCKTNOSUPPORT: i64 = 94;
const EOPNOTSUPP: i64 = 95;
const EAFNOSUPPORT: i64 = 97;
const EADDRINUSE: i64 = 98;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ECONNREFUSED: i64 = 111;

// ------------------------------------------------------------- constants
const AF_UNIX: u16 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;
const SOL_SOCKET: u64 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
const MSG_PEEK: u64 = 0x0002;
const MSG_CTRUNC: i32 = 0x0008;
const MSG_WAITALL: u64 = 0x0100;
const MSG_DONTWAIT: u64 = 0x0040;
const MSG_NOSIGNAL: u64 = 0x4000;
const MSG_OOB: u64 = 0x0001;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;
const SO_REUSEADDR: u64 = 2;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_KEEPALIVE: u64 = 9;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_LINGER: u64 = 13;
const SO_PASSCRED: u64 = 16;
const SO_PEERCRED: u64 = 17;
const SO_ACCEPTCONN: u64 = 30;
const SO_PROTOCOL: u64 = 38;
const SO_DOMAIN: u64 = 39;
const FIONREAD: u64 = 0x541B;
const FIONBIO: u64 = 0x5421;

/// struct sockaddr_un is 110 bytes; the family word is two of them.
const SUN_MAX: u64 = 110;
/// Most descriptors one SCM_RIGHTS message may carry (Linux's SCM_MAX_FD).
const SCM_MAX_FD: usize = 253;
/// Largest control buffer / iovec count read from user space.
const CTL_MAX: usize = 4096;
const IOV_MAX: usize = 1024;
/// The size of a cmsghdr on x86-64 (len, pad, level, type), data follows.
const CMSG_HDR: usize = 16;

// ------------------------------------------------------------ the socket

enum State {
    /// socket() or bind() done, nothing else yet.
    Fresh,
    Listening(Listener<Rc<FileDesc>>),
    Connected(Ep),
}

/// What a `Kind::Unix` description points at.
pub(crate) struct UnixSock {
    state: State,
    /// The name it was bound to (see `Key`), if any.
    bound: Option<String>,
    /// Who made it (listen's credentials, SO_PEERCRED on an unconnected one).
    cred: PeerCred,
}

impl Drop for UnixSock {
    fn drop(&mut self) {
        // Close the connection / the accept queue first, then tell the
        // sleepers: they re-check and find EOF, EPIPE or ECONNREFUSED.
        let st = core::mem::replace(&mut self.state, State::Fresh);
        drop(st);
        notify();
    }
}

fn my_cred() -> PeerCred {
    process::current().map_or(PeerCred::default(), |p| PeerCred {
        pid: p.pid as u32,
        uid: p.cred.euid,
        gid: p.cred.egid,
    })
}

fn new_desc(state: State, bound: Option<String>, extra: u32) -> Rc<FileDesc> {
    let sock = UnixSock { state, bound, cred: my_cred() };
    Rc::new(FileDesc::new(Kind::Unix(Rc::new(RefCell::new(sock))), O_READ | O_WRITE | extra))
}

fn install(desc: Rc<FileDesc>, cloexec: bool) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(desc, cloexec) {
        Some(fd) => fd as i64,
        None => -EMFILE,
    }
}

/// The description and socket behind `fd`: -EBADF, or -ENOTSOCK for a
/// descriptor that is something else.
fn sock_of(fd: u64) -> Result<(Rc<FileDesc>, Rc<RefCell<UnixSock>>), i64> {
    let Some(cur) = process::current() else { return Err(-EFAULT) };
    let Some(d) = cur.fds.get(fd as usize) else { return Err(-EBADF) };
    let s = match &d.kind {
        Kind::Unix(s) => s.clone(),
        _ => return Err(-ENOTSOCK),
    };
    Ok((d, s))
}

// ------------------------------------------------- parked-process wake-ups

/// Pids currently inside `park_once` (fd.rs registers them).  Guarded like
/// futex.rs's waiter table: one CPU, syscalls run with interrupts masked.
static mut PARKED: Vec<u64> = Vec::new();

pub(super) fn parked_enter(pid: u64) {
    let flags = crate::arch::io::cli_save();
    // SAFETY: interrupts masked, single CPU, nobody else walks the vector.
    unsafe { (*addr_of_mut!(PARKED)).push(pid) };
    // SAFETY: flags came from cli_save on this stack.
    unsafe { crate::arch::io::restore_flags(flags) };
}

pub(crate) fn parked_leave(pid: u64) {
    let flags = crate::arch::io::cli_save();
    // SAFETY: as parked_enter.
    unsafe {
        let t = &mut *addr_of_mut!(PARKED);
        if let Some(i) = t.iter().position(|p| *p == pid) {
            t.swap_remove(i);
        }
    }
    // SAFETY: as parked_enter.
    unsafe { crate::arch::io::restore_flags(flags) };
}

/// Something a sleeper may be waiting for just happened: flip every parked
/// process back to Ready so it re-checks now instead of at the next 10 ms
/// quantum.  Spurious wake-ups are harmless (every waiter re-checks), and a
/// process killed while parked is pruned here (it never reached
/// `parked_leave`).
pub(crate) fn notify() {
    let flags = crate::arch::io::cli_save();
    // SAFETY: as parked_enter.
    let pids: Vec<u64> = unsafe {
        let t = &mut *addr_of_mut!(PARKED);
        t.retain(|p| process::has_live(*p));
        t.clone()
    };
    // SAFETY: as parked_enter.
    unsafe { crate::arch::io::restore_flags(flags) };
    for pid in pids {
        process::wake_parked(pid);
    }
}

// ------------------------------------------------------- the name registry

/// A socket name: the absolute path for a file-system name, or '\0' followed
/// by the bytes (one char per byte) for an abstract one.
type Key = String;

/// name -> the socket that bound it.  Weak: a closed socket frees its name
/// (the file it created stays, like on Linux).
static mut REG: Vec<(Key, Weak<RefCell<UnixSock>>)> = Vec::new();

fn reg_lookup(key: &str) -> Option<Rc<RefCell<UnixSock>>> {
    let flags = crate::arch::io::cli_save();
    // SAFETY: read-only walk with interrupts masked.
    let r = unsafe { (*addr_of_mut!(REG)).iter().find(|(k, _)| k == key).and_then(|(_, w)| w.upgrade()) };
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    r
}

/// Claim `key` for `sock`.  `replace`: a file-system name just created a new
/// node, so whatever held the old (unlinked) one loses it; an abstract name
/// that a live socket holds is -EADDRINUSE.
fn reg_insert(key: &str, sock: &Rc<RefCell<UnixSock>>, replace: bool) -> bool {
    let flags = crate::arch::io::cli_save();
    // SAFETY: structure change with interrupts masked.
    let ok = unsafe {
        let t = &mut *addr_of_mut!(REG);
        t.retain(|(_, w)| w.strong_count() > 0);
        if let Some(i) = t.iter().position(|(k, _)| k == key) {
            if replace {
                t.remove(i);
                true
            } else {
                false
            }
        } else {
            true
        }
    };
    if ok {
        // SAFETY: as above.
        unsafe { (*addr_of_mut!(REG)).push((String::from(key), Rc::downgrade(sock))) };
    }
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    ok
}

// ------------------------------------------------------------- addresses

enum Addr {
    Path(Key),
    Abstract(Key),
}

/// `struct sockaddr_un` from user space: family, then the name.
fn read_addr(ptr: u64, len: u64) -> Result<Addr, i64> {
    if !(2..=SUN_MAX).contains(&len) {
        return Err(-EINVAL);
    }
    let n = len as usize;
    let mut raw = [0u8; SUN_MAX as usize];
    if !copy_in(ptr, 0, &mut raw[..n]) {
        return Err(-EFAULT);
    }
    if u16::from_le_bytes([raw[0], raw[1]]) != AF_UNIX {
        return Err(-EAFNOSUPPORT);
    }
    let p = &raw[2..n];
    if p.is_empty() {
        return Err(-EINVAL); // an unnamed address names nothing to bind / reach
    }
    if p[0] == 0 {
        let mut k = String::new();
        for &b in p {
            k.push(b as char); // one char per byte: a lossless, comparable key
        }
        return Ok(Addr::Abstract(k));
    }
    let end = p.iter().position(|&b| b == 0).unwrap_or(p.len());
    let Ok(s) = core::str::from_utf8(&p[..end]) else { return Err(-ENOENT) };
    if s.is_empty() {
        return Err(-ENOENT);
    }
    Ok(Addr::Path(process::abs_current(s)))
}

/// Write a socket name back as a `sockaddr_un`: `addr` gets as much as `*lenp`
/// allows, `*lenp` becomes the full length (Linux's rule).  `addr == 0` is
/// "caller does not care" (accept allows it).
fn put_addr(addr: u64, lenp: u64, key: Option<&String>) -> i64 {
    if addr == 0 {
        return 0;
    }
    let mut full: Vec<u8> = alloc::vec![AF_UNIX as u8, 0u8];
    if let Some(k) = key {
        if k.starts_with('\0') {
            full.extend(k.chars().map(|c| c as u32 as u8));
        } else {
            full.extend_from_slice(k.as_bytes());
            full.push(0);
        }
    }
    let mut lb = [0u8; 4];
    if !copy_in(lenp, 0, &mut lb) {
        return -EFAULT;
    }
    let n = (u32::from_le_bytes(lb) as usize).min(full.len());
    if !copy_out(addr, 0, &full[..n]) || !copy_out(lenp, 0, &(full.len() as u32).to_le_bytes()) {
        return -EFAULT;
    }
    0
}

// ----------------------------------------------------- socket / socketpair

/// Validate `type` (SOCK_STREAM plus the two flag bits); returns
/// (description flags, close-on-exec).
fn check_type(domain: u64, ty: u64, proto: u64) -> Result<(u32, bool), i64> {
    if domain != AF_UNIX as u64 {
        return Err(-EAFNOSUPPORT);
    }
    if ty & !(0xf | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(-EINVAL);
    }
    match ty & 0xf {
        SOCK_STREAM => {}
        2 | 5 => return Err(-ESOCKTNOSUPPORT), // datagram / seqpacket: not here
        _ => return Err(-EINVAL),
    }
    if proto != 0 {
        return Err(-EPROTONOSUPPORT);
    }
    Ok((if ty & SOCK_NONBLOCK != 0 { O_NONBLOCK } else { 0 }, ty & SOCK_CLOEXEC != 0))
}

/// `socket(domain, type, protocol)` (syscall 90).
pub fn sys_socket(domain: u64, ty: u64, proto: u64) -> i64 {
    let (nb, cloexec) = match check_type(domain, ty, proto) {
        Ok(v) => v,
        Err(e) => return e,
    };
    install(new_desc(State::Fresh, None, nb), cloexec)
}

/// `socketpair(domain, type, protocol, int sv[2])` (syscall 91).
pub fn sys_socketpair(domain: u64, ty: u64, proto: u64, sv: u64) -> i64 {
    let (nb, cloexec) = match check_type(domain, ty, proto) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let me = my_cred();
    let (mut a, mut b) = unix_core::pair::<Rc<FileDesc>>();
    a.peer_cred = me;
    b.peer_cred = me;
    let da = new_desc(State::Connected(a), None, nb);
    let db = new_desc(State::Connected(b), None, nb);
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(fa) = cur.fds.install(da, cloexec) else { return -EMFILE };
    let Some(fb) = cur.fds.install(db, cloexec) else {
        cur.fds.close(fa);
        return -EMFILE;
    };
    let ok = copy_out(sv, 0, &(fa as i32).to_le_bytes()) && copy_out(sv, 4, &(fb as i32).to_le_bytes());
    if !ok {
        cur.fds.close(fa);
        cur.fds.close(fb);
        return -EFAULT;
    }
    0
}

// ------------------------------------------- bind / listen / accept / connect

/// `bind(fd, addr, len)` (syscall 92).  A file-system name creates the node
/// (-EADDRINUSE when it exists, like Linux); an abstract one only claims the
/// name.
pub fn sys_bind(fd: u64, addr: u64, len: u64) -> i64 {
    let (_d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    {
        let b = s.borrow();
        if b.bound.is_some() || !matches!(b.state, State::Fresh) {
            return -EINVAL; // already named / already connected
        }
    }
    let (key, replace) = match read_addr(addr, len) {
        Ok(Addr::Abstract(k)) => (k, false),
        Ok(Addr::Path(p)) => {
            let mask = process::current().map_or(0o022, |p| p.umask);
            match crate::fs::fsys::vfs().create(&p, 0o777 & !mask) {
                Ok(()) => {}
                Err(crate::fs::vfs::FsError::Exists) => return -EADDRINUSE,
                Err(e) => return -fs_errno(e),
            }
            (p, true)
        }
        Err(e) => return e,
    };
    if !reg_insert(&key, &s, replace) {
        return -EADDRINUSE;
    }
    s.borrow_mut().bound = Some(key);
    0
}

/// `listen(fd, backlog)` (syscall 93).
pub fn sys_listen(fd: u64, backlog: u64) -> i64 {
    let (_d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let backlog = (backlog as i32).max(0) as usize;
    let me = my_cred();
    let mut b = s.borrow_mut();
    if b.bound.is_none() {
        return -EINVAL; // no listening on an unbound socket
    }
    match &mut b.state {
        State::Listening(l) => {
            l.set_backlog(backlog);
            0
        }
        State::Fresh => {
            b.state = State::Listening(Listener::new(backlog, me));
            b.cred = me;
            0
        }
        State::Connected(_) => -EINVAL,
    }
}

/// `accept4(fd, addr, int *len, flags)` (syscall 94).
pub fn sys_accept(fd: u64, addr: u64, lenp: u64, flags: u64) -> i64 {
    let (d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return -EINVAL;
    }
    let nonblock = d.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    loop {
        let got = {
            let mut b = s.borrow_mut();
            match &mut b.state {
                State::Listening(l) => Some(l.take()),
                _ => None,
            }
        };
        match got {
            None => return -EINVAL, // not a listening socket
            Some(Some(ep)) => {
                notify(); // a connector may be waiting for backlog room
                let peer = ep.peer.clone();
                let local = ep.local.clone();
                let nb = if flags & SOCK_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
                let nd = new_desc(State::Connected(ep), local, nb);
                let newfd = install(nd, flags & SOCK_CLOEXEC != 0);
                if newfd < 0 {
                    return newfd; // the connection was dropped: the client sees EOF
                }
                let r = put_addr(addr, lenp, peer.as_ref());
                if r < 0 {
                    if let Some(cur) = process::current() {
                        cur.fds.close(newfd as usize);
                    }
                    return r;
                }
                return newfd;
            }
            Some(None) => {}
        }
        if signal::interrupt_pending() {
            return -EINTR;
        }
        if nonblock || uptime_ms() - start >= WAIT_MAX_MS {
            return -EAGAIN;
        }
        park_once();
    }
}

/// `connect(fd, addr, len)` (syscall 95).
pub fn sys_connect(fd: u64, addr: u64, len: u64) -> i64 {
    let (d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    {
        let b = s.borrow();
        match b.state {
            State::Fresh => {}
            State::Connected(_) => return -EISCONN,
            State::Listening(_) => return -EINVAL,
        }
    }
    let key = match read_addr(addr, len) {
        Ok(Addr::Abstract(k)) => k,
        Ok(Addr::Path(p)) => {
            // The node must exist (ENOENT); a node nobody listens on is a
            // refused connection, like a stale socket file on Linux.
            if let Err(e) = crate::fs::fsys::vfs().stat(&p) {
                return -fs_errno(e);
            }
            p
        }
        Err(e) => return e,
    };
    let Some(target) = reg_lookup(&key) else { return -ECONNREFUSED };
    let server_cred = match &target.borrow().state {
        State::Listening(l) => l.cred,
        _ => return -ECONNREFUSED,
    };
    let (mut cl, mut sv) = unix_core::pair::<Rc<FileDesc>>();
    cl.local = s.borrow().bound.clone();
    cl.peer = Some(key.clone());
    cl.peer_cred = server_cred;
    sv.local = Some(key);
    sv.peer = cl.local.clone();
    sv.peer_cred = my_cred();

    let nonblock = d.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    let mut pending = Some(sv);
    loop {
        let outcome = {
            let mut t = target.borrow_mut();
            match &mut t.state {
                State::Listening(l) => match l.queue(pending.take().unwrap()) {
                    Ok(()) => 0,
                    Err(back) => {
                        pending = Some(back);
                        1
                    }
                },
                _ => 2,
            }
        };
        match outcome {
            0 => break,
            2 => return -ECONNREFUSED,
            _ => {}
        }
        // Backlog full: wait for an accept, unless the listener is gone
        // (only our own reference left: every description of it closed).
        if Rc::strong_count(&target) == 1 {
            return -ECONNREFUSED;
        }
        if signal::interrupt_pending() {
            return -EINTR;
        }
        if nonblock || uptime_ms() - start >= WAIT_MAX_MS {
            return -EAGAIN;
        }
        park_once();
    }
    s.borrow_mut().state = State::Connected(cl);
    notify(); // wake the accepter
    0
}

// ------------------------------------------------------------ data transfer

/// Send `data` (all of it, blocking in the bounded way); `rights` ride on the
/// first byte.  Returns the bytes queued, or a negative errno.
fn send_data(s: &Rc<RefCell<UnixSock>>, data: &[u8], rights: Option<Vec<Rc<FileDesc>>>, nonblock: bool, nosignal: bool) -> i64 {
    let tx = match &s.borrow().state {
        State::Connected(ep) => ep.tx.clone(),
        _ => return -ENOTCONN,
    };
    let mut rights = rights;
    let start = uptime_ms();
    let mut done = 0usize;
    loop {
        let (n, broken) = {
            let mut c = tx.borrow_mut();
            if c.rd_closed || c.wr_closed {
                (0, true)
            } else {
                (c.push(&data[done..], &mut rights), false)
            }
        };
        if broken {
            return if done > 0 {
                done as i64
            } else if nosignal {
                -EPIPE
            } else {
                signal::pipe_sigpipe() // -EPIPE, or death by SIGPIPE
            };
        }
        if n > 0 {
            done += n;
            notify(); // a reader may be waiting
            if done == data.len() {
                return done as i64;
            }
            continue;
        }
        if data.is_empty() {
            return 0;
        }
        // Channel full.
        if signal::interrupt_pending() {
            return if done > 0 { done as i64 } else { -EINTR };
        }
        if nonblock || uptime_ms() - start >= WAIT_MAX_MS {
            return if done > 0 { done as i64 } else { -EAGAIN };
        }
        park_once();
    }
}

/// What `recv_data` produced.
struct Received {
    data: Vec<u8>,
    rights: Vec<Rc<FileDesc>>,
    ctrunc: bool,
}

/// Receive up to `max` bytes (empty data = EOF).  `take_rights`: the caller
/// has room for descriptors; otherwise the batch riding on the first byte is
/// discarded and reported as truncated.
fn recv_data(s: &Rc<RefCell<UnixSock>>, max: usize, take_rights: bool, peek: bool, nonblock: bool) -> Result<Received, i64> {
    let rx = match &s.borrow().state {
        State::Connected(ep) => ep.rx.clone(),
        _ => return Err(-ENOTCONN),
    };
    let start = uptime_ms();
    loop {
        let step = {
            let mut c = rx.borrow_mut();
            if c.len() > 0 {
                if peek {
                    Some((c.peek(max), Vec::new(), false, Vec::new()))
                } else {
                    let p = c.pop(max, take_rights);
                    Some((p.data, p.rights, p.ctrunc, p.discarded))
                }
            } else if c.wr_closed {
                return Ok(Received { data: Vec::new(), rights: Vec::new(), ctrunc: false });
            } else {
                None
            }
        };
        if let Some((data, rights, ctrunc, discarded)) = step {
            drop(discarded); // channel borrow released: now they may close
            if !peek {
                notify(); // a writer may be waiting for room
            }
            return Ok(Received { data, rights, ctrunc });
        }
        if signal::interrupt_pending() {
            return Err(-EINTR);
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        if uptime_ms() - start >= WAIT_MAX_MS {
            return Err(-EAGAIN);
        }
        park_once();
    }
}

/// read(2) on a socket (the caller already capped `len`).
pub(super) fn read(s: &Rc<RefCell<UnixSock>>, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    let nonblock = desc.flags() & O_NONBLOCK != 0;
    match recv_data(s, len, false, false, nonblock) {
        Ok(r) => {
            if !r.data.is_empty() && !copy_out(buf, 0, &r.data) {
                return -EFAULT;
            }
            r.data.len() as i64
        }
        Err(e) => e,
    }
}

/// write(2) on a socket.
pub(super) fn write(s: &Rc<RefCell<UnixSock>>, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    let mut data = alloc::vec![0u8; len];
    if !copy_in(buf, 0, &mut data) {
        return -EFAULT;
    }
    send_data(s, &data, None, desc.flags() & O_NONBLOCK != 0, false)
}

// --- msghdr (the x86-64 layout abi-bits/socket.h defines: name@0 namelen@8
// iov@16 iovlen@24 control@32 controllen@40 flags@48; the counts are 32-bit
// fields followed by padding, so only the low half is read).

struct MsgHdr {
    iov: u64,
    iovlen: usize,
    control: u64,
    controllen: usize,
}

fn read_u64(ptr: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    copy_in(ptr, 0, &mut b).then(|| u64::from_le_bytes(b))
}

fn read_u32(ptr: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    copy_in(ptr, 0, &mut b).then(|| u32::from_le_bytes(b))
}

fn read_msghdr(msg: u64) -> Result<MsgHdr, i64> {
    let (Some(iov), Some(iovlen), Some(control), Some(controllen)) =
        (read_u64(msg + 16), read_u32(msg + 24), read_u64(msg + 32), read_u32(msg + 40))
    else {
        return Err(-EFAULT);
    };
    let iovlen = iovlen as usize;
    if iovlen > IOV_MAX {
        return Err(-EMSGSIZE);
    }
    Ok(MsgHdr { iov, iovlen, control, controllen: controllen as usize })
}

/// (base, len) of each iovec entry.
fn read_iov(h: &MsgHdr) -> Result<Vec<(u64, usize)>, i64> {
    let mut v = Vec::with_capacity(h.iovlen);
    for i in 0..h.iovlen {
        let at = h.iov + (i as u64) * 16;
        let (Some(base), Some(len)) = (read_u64(at), read_u64(at + 8)) else { return Err(-EFAULT) };
        v.push((base, len as usize));
    }
    Ok(v)
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// The descriptions named by the SCM_RIGHTS messages of a control buffer.
fn parse_rights(control: u64, controllen: usize) -> Result<Vec<Rc<FileDesc>>, i64> {
    let mut out: Vec<Rc<FileDesc>> = Vec::new();
    if control == 0 || controllen < CMSG_HDR {
        return Ok(out);
    }
    let n = controllen.min(CTL_MAX);
    let mut raw = alloc::vec![0u8; n];
    if !copy_in(control, 0, &mut raw) {
        return Err(-EFAULT);
    }
    let mut off = 0usize;
    while off + CMSG_HDR <= n {
        let clen = u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as usize;
        let level = i32::from_le_bytes([raw[off + 8], raw[off + 9], raw[off + 10], raw[off + 11]]);
        let ty = i32::from_le_bytes([raw[off + 12], raw[off + 13], raw[off + 14], raw[off + 15]]);
        if clen < CMSG_HDR || off + clen > n {
            return Err(-EINVAL);
        }
        if level as u64 == SOL_SOCKET {
            match ty {
                SCM_RIGHTS => {
                    let payload = clen - CMSG_HDR;
                    if payload % 4 != 0 {
                        return Err(-EINVAL);
                    }
                    let Some(cur) = process::current() else { return Err(-EFAULT) };
                    for k in 0..payload / 4 {
                        let p = off + CMSG_HDR + k * 4;
                        let fd = i32::from_le_bytes([raw[p], raw[p + 1], raw[p + 2], raw[p + 3]]);
                        if fd < 0 {
                            return Err(-EBADF);
                        }
                        let Some(d) = cur.fds.get(fd as usize) else { return Err(-EBADF) };
                        out.push(d);
                        if out.len() > SCM_MAX_FD {
                            return Err(-EINVAL);
                        }
                    }
                }
                SCM_CREDENTIALS => {} // accepted and ignored: nothing consumes them
                _ => return Err(-EINVAL),
            }
        } else {
            return Err(-EINVAL);
        }
        off += align8(clen);
    }
    Ok(out)
}

/// `sendmsg(fd, msghdr *, flags)` (syscall 96).
pub fn sys_sendmsg(fd: u64, msg: u64, flags: u64) -> i64 {
    let (d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if flags & MSG_OOB != 0 {
        return -EOPNOTSUPP;
    }
    let h = match read_msghdr(msg) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let iov = match read_iov(&h) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // Gather, at most one channel's worth per call (a short count is the
    // house convention, as for a pipe write).
    let mut data: Vec<u8> = Vec::new();
    for (base, len) in iov {
        let take = len.min(CHAN_CAP - data.len());
        if take == 0 {
            continue;
        }
        let at = data.len();
        data.resize(at + take, 0);
        if !copy_in(base, 0, &mut data[at..]) {
            return -EFAULT;
        }
        if data.len() >= CHAN_CAP {
            break;
        }
    }
    let rights = match parse_rights(h.control, h.controllen) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let nonblock = d.flags() & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0;
    let rights = if rights.is_empty() { None } else { Some(rights) };
    send_data(&s, &data, rights, nonblock, flags & MSG_NOSIGNAL != 0)
}

/// `recvmsg(fd, msghdr *, flags)` (syscall 97).
pub fn sys_recvmsg(fd: u64, msg: u64, flags: u64) -> i64 {
    let (d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if flags & MSG_OOB != 0 {
        return -EOPNOTSUPP;
    }
    let h = match read_msghdr(msg) {
        Ok(h) => h,
        Err(e) => return e,
    };
    let iov = match read_iov(&h) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let total = iov.iter().fold(0usize, |a, (_, l)| a.saturating_add(*l)).min(CHAN_CAP);
    let nonblock = d.flags() & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0;
    let peek = flags & MSG_PEEK != 0;
    // Room for at least one descriptor decides whether rights may be taken.
    let fit = if h.control != 0 && h.controllen >= CMSG_HDR + 4 { (h.controllen - CMSG_HDR) / 4 } else { 0 };

    let mut data: Vec<u8> = Vec::new();
    let mut rights: Vec<Rc<FileDesc>> = Vec::new();
    let mut ctrunc = false;
    loop {
        let want = total - data.len();
        // Zero room (an empty iovec) still reports EOF / readiness: ask for
        // nothing and get nothing, like read(fd, buf, 0).
        if want == 0 {
            break;
        }
        match recv_data(&s, want, fit > 0 && rights.is_empty(), peek, nonblock && data.is_empty()) {
            Ok(r) => {
                if r.data.is_empty() {
                    break; // EOF
                }
                ctrunc |= r.ctrunc;
                data.extend_from_slice(&r.data);
                if !r.rights.is_empty() {
                    rights = r.rights;
                    break; // a batch ends the message: never merge two
                }
                // MSG_WAITALL keeps going until the buffer is full, EOF, or
                // an error after some data (which is then a short count).
                if flags & MSG_WAITALL == 0 || peek || data.len() >= total {
                    break;
                }
            }
            Err(e) => {
                if data.is_empty() {
                    return e;
                }
                break;
            }
        }
    }
    // Scatter the bytes.
    let mut at = 0usize;
    for (base, len) in iov {
        if at >= data.len() {
            break;
        }
        let n = len.min(data.len() - at);
        if !copy_out(base, 0, &data[at..at + n]) {
            return -EFAULT;
        }
        at += n;
    }
    // Install the descriptors that fit; the rest are dropped (MSG_CTRUNC).
    let mut ctl_used = 0usize;
    if !rights.is_empty() {
        let take = rights.len().min(fit);
        if take < rights.len() {
            ctrunc = true;
        }
        let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
        let mut fds: Vec<i32> = Vec::new();
        for r in rights.drain(..take) {
            let n = install(r, cloexec);
            if n < 0 {
                ctrunc = true;
                break;
            }
            fds.push(n as i32);
        }
        drop(rights); // whatever did not fit closes here, outside any borrow
        if !fds.is_empty() {
            let clen = CMSG_HDR + 4 * fds.len();
            let mut cm: Vec<u8> = Vec::with_capacity(align8(clen));
            cm.extend_from_slice(&(clen as u32).to_le_bytes());
            cm.extend_from_slice(&0u32.to_le_bytes());
            cm.extend_from_slice(&(SOL_SOCKET as i32).to_le_bytes());
            cm.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
            for f in &fds {
                cm.extend_from_slice(&f.to_le_bytes());
            }
            ctl_used = align8(clen).min(h.controllen);
            cm.resize(ctl_used, 0);
            if !copy_out(h.control, 0, &cm) {
                return -EFAULT;
            }
        }
    }
    // msg_namelen = 0 (connected stream), msg_controllen, msg_flags.
    let out_flags: i32 = if ctrunc { MSG_CTRUNC } else { 0 };
    if !copy_out(msg, 8, &0u32.to_le_bytes())
        || !copy_out(msg, 40, &(ctl_used as u32).to_le_bytes())
        || !copy_out(msg, 48, &out_flags.to_le_bytes())
    {
        return -EFAULT;
    }
    data.len() as i64
}

// ---------------------------------------------------- shutdown / names / opts

/// `shutdown(fd, how)` (syscall 98).
pub fn sys_shutdown(fd: u64, how: u64) -> i64 {
    let (_d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if how > 2 {
        return -EINVAL;
    }
    // Like Linux, shutdown on a socket with no connection is a no-op that
    // succeeds (POSIX says ENOTCONN, but the idiom "shutdown the listener
    // to unstick whoever sleeps in accept" has to keep working).
    if let State::Connected(ep) = &s.borrow().state {
        ep.shutdown(how as u32);
    }
    notify();
    0
}

/// `getsockname(fd, addr, int *len)` (syscall 99).
pub fn sys_getsockname(fd: u64, addr: u64, lenp: u64) -> i64 {
    let (_d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let name = {
        let b = s.borrow();
        match &b.state {
            State::Connected(ep) => ep.local.clone(),
            _ => b.bound.clone(),
        }
    };
    put_addr(addr, lenp, name.as_ref())
}

/// `getpeername(fd, addr, int *len)` (syscall 100).
pub fn sys_getpeername(fd: u64, addr: u64, lenp: u64) -> i64 {
    let (_d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let name = match &s.borrow().state {
        State::Connected(ep) => ep.peer.clone(),
        _ => return -ENOTCONN,
    };
    put_addr(addr, lenp, name.as_ref())
}

/// Copy a small option value out the getsockopt way: at most `*lenp` bytes,
/// `*lenp` becomes the real size.
fn put_opt(val: u64, lenp: u64, bytes: &[u8]) -> i64 {
    let Some(have) = read_u32(lenp) else { return -EFAULT };
    let n = (have as usize).min(bytes.len());
    if !copy_out(val, 0, &bytes[..n]) || !copy_out(lenp, 0, &(bytes.len() as u32).to_le_bytes()) {
        return -EFAULT;
    }
    0
}

/// `getsockopt(fd, level, name, val, int *len)` (syscall 101).
pub fn sys_getsockopt(fd: u64, level: u64, name: u64, val: u64, lenp: u64) -> i64 {
    let (_d, s) = match sock_of(fd) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if level != SOL_SOCKET {
        return -ENOPROTOOPT;
    }
    let int = |v: i32| put_opt(val, lenp, &v.to_le_bytes());
    let b = s.borrow();
    match name {
        SO_TYPE => int(SOCK_STREAM as i32),
        SO_ERROR => int(0),
        SO_DOMAIN => int(AF_UNIX as i32),
        SO_PROTOCOL => int(0),
        SO_ACCEPTCONN => int(matches!(b.state, State::Listening(_)) as i32),
        SO_REUSEADDR | SO_KEEPALIVE | SO_PASSCRED => int(0),
        SO_SNDBUF | SO_RCVBUF => int(CHAN_CAP as i32),
        SO_PEERCRED => {
            let c = match &b.state {
                State::Connected(ep) => ep.peer_cred,
                _ => b.cred,
            };
            let mut u = Vec::with_capacity(12);
            u.extend_from_slice(&(c.pid as i32).to_le_bytes());
            u.extend_from_slice(&c.uid.to_le_bytes());
            u.extend_from_slice(&c.gid.to_le_bytes());
            put_opt(val, lenp, &u)
        }
        _ => -ENOPROTOOPT,
    }
}

/// `setsockopt(fd, level, name, val, len)` (syscall 102): the options a
/// library sets out of habit are accepted (they change nothing here);
/// anything else is -ENOPROTOOPT.
pub fn sys_setsockopt(fd: u64, level: u64, name: u64, _val: u64, _len: u64) -> i64 {
    if let Err(e) = sock_of(fd) {
        return e;
    }
    if level != SOL_SOCKET {
        return -ENOPROTOOPT;
    }
    match name {
        SO_REUSEADDR | SO_KEEPALIVE | SO_LINGER | SO_PASSCRED | SO_SNDBUF | SO_RCVBUF => 0,
        _ => -ENOPROTOOPT,
    }
}

// ------------------------------------------- hooks for the descriptor layer

/// ioctl on a socket: FIONREAD (bytes readable) and FIONBIO (non-blocking
/// switch) -- the two a display library leans on.  None = not ours.
pub(super) fn ioctl(s: &Rc<RefCell<UnixSock>>, desc: &Rc<FileDesc>, req: u64, arg: u64) -> Option<i64> {
    match req {
        FIONREAD => {
            let n = match &s.borrow().state {
                State::Connected(ep) => ep.rx.borrow().len() as i32,
                _ => 0,
            };
            Some(if copy_out(arg, 0, &n.to_le_bytes()) { 0 } else { -EFAULT })
        }
        FIONBIO => {
            let Some(v) = read_u32(arg) else { return Some(-EFAULT) };
            let f = desc.flags();
            desc.flags.set(if v != 0 { f | O_NONBLOCK } else { f & !O_NONBLOCK });
            Some(0)
        }
        _ => None,
    }
}

/// poll / select readiness: (read, write, err, hup).
pub(super) fn ready(s: &Rc<RefCell<UnixSock>>) -> (bool, bool, bool, bool) {
    match &s.borrow().state {
        State::Connected(ep) => {
            let (r, w, hup, err) = ep.ready();
            (r, w, err, hup)
        }
        State::Listening(l) => (l.pending(), false, false, false),
        State::Fresh => (false, true, false, false),
    }
}

/// Where /proc/<pid>/fd/N points.
pub(super) fn proc_name() -> String {
    String::from("socket:[unix]")
}

/// fstat's `st_mode` file-type bits for a socket.
pub(super) const S_IFSOCK: u32 = 0o140000;
