// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// AF_UNIX stream sockets, the pure half (phase 6 groundwork for X11).
//
// This file knows nothing about processes, descriptors or user memory: it is
// the state machine of a local stream connection, generic over `T`, the type
// of what `SCM_RIGHTS` carries (the kernel instantiates it with
// `Rc<FileDesc>`; tests/unix_host.rs with a plain integer).  The glue --
// syscalls, blocking, user-memory copies, the name registry -- lives in
// user/unix.rs, which is the only user of this module in the kernel.
//
// Model:
//
//   * a *channel* (`Chan`) is one direction of a connection: a bounded byte
//     queue, plus the file descriptions in flight (`SCM_RIGHTS`) tagged with
//     the stream position of the byte they ride on, plus two bits -- the
//     sender is gone / shut down (`wr_closed`: the reader sees EOF once the
//     queue drains) and the reader is gone / shut down (`rd_closed`: the
//     writer gets EPIPE);
//   * an *endpoint* (`Endpoint`) is one side of a connection: the channel it
//     reads (`rx`) and the one it writes (`tx`).  Dropping an endpoint is
//     close(2): its tx gets `wr_closed`, its rx gets `rd_closed` and is
//     emptied, so the peer sees EOF / EPIPE with no further bookkeeping;
//   * a *listener* (`Listener`) is the accept queue: connect(2) builds a
//     connected pair, keeps the client half and queues the server half here;
//     accept(2) takes it out.  A listener that goes away drops its queue,
//     which closes the queued server halves -- the waiting clients see EOF.
//
// Rights ride on the first byte of the write that carried them (Linux's rule):
// a receive never merges data across two batches of rights, so a reader asking
// for rights gets exactly the batch that belongs to the bytes it read.

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Bytes one direction of a connection buffers before a writer blocks.
pub const CHAN_CAP: usize = 64 * 1024;

/// SOMAXCONN-ish ceiling for listen(2)'s backlog.
pub const MAX_BACKLOG: usize = 128;

/// Credentials of the process on the other side (SO_PEERCRED).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

// ----------------------------------------------------------------- channel

/// One direction of a connection.
pub struct Chan<T> {
    buf: VecDeque<u8>,
    /// Rights in flight: (stream position of the carrying byte, the rights).
    rights: VecDeque<(u64, Vec<T>)>,
    /// Bytes ever pushed / popped: the stream positions rights are tagged with.
    pushed: u64,
    popped: u64,
    /// No more data will arrive (sender closed or shut down its write side).
    pub wr_closed: bool,
    /// Nobody will read (receiver closed or shut down its read side).
    pub rd_closed: bool,
}

/// What `Chan::pop` took out.
pub struct Popped<T> {
    pub data: Vec<u8>,
    /// The rights that rode on the first byte (empty when none, or when the
    /// caller did not want them).
    pub rights: Vec<T>,
    /// Rights were discarded because the caller could not take them.
    pub ctrunc: bool,
    /// The discarded rights themselves.  They are handed back instead of
    /// being dropped here: dropping a description can close another socket
    /// whose endpoint borrows *this* channel, so the caller must release
    /// its borrow first and drop them afterwards (see the header's
    /// re-entrancy note in user/fd/unix.rs).
    pub discarded: Vec<T>,
}

impl<T> Chan<T> {
    pub fn new() -> Chan<T> {
        Chan {
            buf: VecDeque::new(),
            rights: VecDeque::new(),
            pushed: 0,
            popped: 0,
            wr_closed: false,
            rd_closed: false,
        }
    }

    /// Bytes waiting to be read.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Room left before a writer would block.
    pub fn space(&self) -> usize {
        CHAN_CAP - self.buf.len()
    }

    /// A read would not block: data is queued, or EOF is.
    pub fn readable(&self) -> bool {
        !self.buf.is_empty() || self.wr_closed
    }

    /// Append up to `space()` bytes of `data`; returns how many went in.
    /// `rights` is taken (left `None`) only when at least one byte was stored:
    /// it rides on the first of them.  The caller decides what a closed
    /// channel means (this function does not look at the close bits).
    pub fn push(&mut self, data: &[u8], rights: &mut Option<Vec<T>>) -> usize {
        let n = data.len().min(self.space());
        if n == 0 {
            return 0;
        }
        if let Some(r) = rights.take() {
            if !r.is_empty() {
                self.rights.push_back((self.pushed, r));
            }
        }
        self.buf.extend(&data[..n]);
        self.pushed += n as u64;
        n
    }

    /// Take up to `max` bytes.  When the first byte carries rights they come
    /// along (`take_rights`) or are discarded with `ctrunc` set; a read never
    /// crosses into the next batch of rights, so it stops just before the
    /// next carrying byte.
    pub fn pop(&mut self, max: usize, take_rights: bool) -> Popped<T> {
        let mut limit = max.min(self.buf.len());
        let mut rights = Vec::new();
        let mut discarded = Vec::new();
        let mut ctrunc = false;
        match self.rights.front() {
            Some((pos, _)) if *pos <= self.popped => {
                if let Some((_, r)) = self.rights.pop_front() {
                    if take_rights {
                        rights = r;
                    } else {
                        ctrunc = true;
                        discarded = r;
                    }
                }
                if let Some((np, _)) = self.rights.front() {
                    limit = limit.min((*np - self.popped) as usize);
                }
            }
            Some((pos, _)) => limit = limit.min((*pos - self.popped) as usize),
            None => {}
        }
        let data: Vec<u8> = self.buf.drain(..limit).collect();
        self.popped += data.len() as u64;
        Popped { data, rights, ctrunc, discarded }
    }

    /// MSG_PEEK: copy of what `pop(max)` would return, nothing consumed.
    pub fn peek(&self, max: usize) -> Vec<u8> {
        let mut limit = max.min(self.buf.len());
        match self.rights.front() {
            Some((pos, _)) if *pos <= self.popped => {
                // The batch at the head is not consumed by a peek; data that
                // belongs to the *next* batch stays out of reach.
                if let Some((np, _)) = self.rights.get(1) {
                    limit = limit.min((*np - self.popped) as usize);
                }
            }
            Some((pos, _)) => limit = limit.min((*pos - self.popped) as usize),
            None => {}
        }
        self.buf.iter().take(limit).copied().collect()
    }

    /// The receiver goes away (close, or shutdown(SHUT_RD)): what is queued
    /// is dropped.  The rights that were in flight are *returned*, not
    /// dropped: the caller lets them go after releasing its borrow of the
    /// channel (a description released here may be the last reference to
    /// a socket whose own endpoint touches this very channel).
    #[must_use]
    pub fn close_read(&mut self) -> Vec<(u64, Vec<T>)> {
        self.rd_closed = true;
        self.buf.clear();
        self.popped = self.pushed;
        core::mem::take(&mut self.rights).into_iter().collect()
    }

    /// The sender goes away (close, or shutdown(SHUT_WR)): the reader drains
    /// what is queued, then sees EOF.
    pub fn close_write(&mut self) {
        self.wr_closed = true;
    }
}

// ---------------------------------------------------------------- endpoint

/// One side of a connected pair.
pub struct Endpoint<T> {
    pub rx: Rc<RefCell<Chan<T>>>,
    pub tx: Rc<RefCell<Chan<T>>>,
    /// Who is on the other side (SO_PEERCRED).
    pub peer_cred: PeerCred,
    /// Own / peer address as bound (absolute path, or "\0name" for the
    /// abstract namespace); None when unnamed.
    pub local: Option<String>,
    pub peer: Option<String>,
}

impl<T> Drop for Endpoint<T> {
    fn drop(&mut self) {
        self.tx.borrow_mut().close_write();
        // The borrow ends with the statement; only then do the rights in
        // flight (if any) drop.
        let junk = self.rx.borrow_mut().close_read();
        drop(junk);
    }
}

/// shutdown(2) `how`.
pub const SHUT_RD: u32 = 0;
pub const SHUT_WR: u32 = 1;
pub const SHUT_RDWR: u32 = 2;

impl<T> Endpoint<T> {
    /// Apply shutdown(how); false for an invalid `how`.
    pub fn shutdown(&self, how: u32) -> bool {
        match how {
            SHUT_RD => {
                let junk = self.rx.borrow_mut().close_read();
                drop(junk);
            }
            SHUT_WR => self.tx.borrow_mut().close_write(),
            SHUT_RDWR => {
                let junk = self.rx.borrow_mut().close_read();
                drop(junk);
                self.tx.borrow_mut().close_write();
            }
            _ => return false,
        }
        true
    }

    /// Poll-style readiness: (readable, writable, hup, err).
    /// `hup`: the peer is gone and nothing is left to read.  `err`: a write
    /// would fail with EPIPE.
    pub fn ready(&self) -> (bool, bool, bool, bool) {
        let rx = self.rx.borrow();
        let tx = self.tx.borrow();
        let broken = tx.rd_closed || tx.wr_closed;
        (rx.readable(), tx.space() > 0 || broken, rx.wr_closed && rx.len() == 0, broken)
    }
}

/// A connected pair: what socketpair(2) returns, and what connect(2) builds
/// (client half, server half).  The credentials are filled by the caller.
pub fn pair<T>() -> (Endpoint<T>, Endpoint<T>) {
    let a_to_b = Rc::new(RefCell::new(Chan::new()));
    let b_to_a = Rc::new(RefCell::new(Chan::new()));
    let a = Endpoint {
        rx: b_to_a.clone(),
        tx: a_to_b.clone(),
        peer_cred: PeerCred::default(),
        local: None,
        peer: None,
    };
    let b = Endpoint {
        rx: a_to_b,
        tx: b_to_a,
        peer_cred: PeerCred::default(),
        local: None,
        peer: None,
    };
    (a, b)
}

// ---------------------------------------------------------------- listener

/// The accept queue of a listening socket.
pub struct Listener<T> {
    backlog: VecDeque<Endpoint<T>>,
    max: usize,
    /// Credentials of whoever called listen(2): what connecting clients see
    /// as their peer.
    pub cred: PeerCred,
}

impl<T> Listener<T> {
    pub fn new(backlog: usize, cred: PeerCred) -> Listener<T> {
        Listener { backlog: VecDeque::new(), max: backlog.clamp(1, MAX_BACKLOG), cred }
    }

    /// listen(2) on an already listening socket: change the backlog.
    pub fn set_backlog(&mut self, backlog: usize) {
        self.max = backlog.clamp(1, MAX_BACKLOG);
    }

    /// Queue a server half; gives it back when the queue is full.
    pub fn queue(&mut self, ep: Endpoint<T>) -> Result<(), Endpoint<T>> {
        if self.backlog.len() >= self.max {
            Err(ep)
        } else {
            self.backlog.push_back(ep);
            Ok(())
        }
    }

    /// accept(2)'s dequeue.
    pub fn take(&mut self) -> Option<Endpoint<T>> {
        self.backlog.pop_front()
    }

    /// A connection is waiting.
    pub fn pending(&self) -> bool {
        !self.backlog.is_empty()
    }
}
