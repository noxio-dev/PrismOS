// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the AF_UNIX state machine (sys/kern/src/user/unix_core.rs):
// the byte channel, EOF / EPIPE on close and shutdown, SCM_RIGHTS batches
// riding on their first byte, the listener's accept queue.  The module is
// compiled unchanged; `T` (what the rights carry) is a plain integer here and
// an `Rc<FileDesc>` in the kernel.
//
//   rustc --edition 2021 --test tests/unix_host.rs -o /tmp/unix_host \
//     && /tmp/unix_host --test-threads=1

#![allow(dead_code, unused_imports)]

extern crate alloc;

#[path = "../sys/kern/src/user/unix_core.rs"]
mod unix_core;

use std::cell::Cell;
use std::rc::Rc;
use unix_core::*;

fn none<T>() -> Option<Vec<T>> {
    None
}

#[test]
fn bytes_flow_both_ways_in_order() {
    let (a, b) = pair::<u32>();
    assert_eq!(a.tx.borrow_mut().push(b"hello ", &mut none()), 6);
    assert_eq!(a.tx.borrow_mut().push(b"world", &mut none()), 5);
    let got = b.rx.borrow_mut().pop(64, false);
    assert_eq!(got.data, b"hello world");
    assert!(got.rights.is_empty() && !got.ctrunc);
    // the other direction is independent
    assert_eq!(b.tx.borrow_mut().push(b"pong", &mut none()), 4);
    assert_eq!(a.rx.borrow_mut().pop(2, false).data, b"po");
    assert_eq!(a.rx.borrow_mut().pop(64, false).data, b"ng");
    assert!(!a.rx.borrow().readable());
}

#[test]
fn capacity_bounds_a_writer() {
    let (a, b) = pair::<u32>();
    let big = vec![7u8; CHAN_CAP + 100];
    assert_eq!(a.tx.borrow_mut().push(&big, &mut none()), CHAN_CAP);
    assert_eq!(a.tx.borrow().space(), 0);
    assert_eq!(a.tx.borrow_mut().push(b"x", &mut none()), 0);
    // draining makes room again
    assert_eq!(b.rx.borrow_mut().pop(10, false).data.len(), 10);
    assert_eq!(a.tx.borrow().space(), 10);
    assert_eq!(a.tx.borrow_mut().push(&big, &mut none()), 10);
}

#[test]
fn peer_close_gives_eof_after_the_queue_drains() {
    let (a, b) = pair::<u32>();
    a.tx.borrow_mut().push(b"last words", &mut none());
    drop(a);
    // data first, then EOF
    assert!(b.rx.borrow().readable());
    assert_eq!(b.rx.borrow_mut().pop(64, false).data, b"last words");
    assert!(b.rx.borrow().wr_closed);
    assert!(b.rx.borrow().readable(), "EOF counts as readable");
    assert_eq!(b.rx.borrow().len(), 0);
    let (r, _w, hup, _e) = b.ready();
    assert!(r && hup);
}

#[test]
fn peer_close_breaks_the_writer() {
    let (a, b) = pair::<u32>();
    assert!(!a.ready().3);
    drop(b);
    let (_r, w, _h, err) = a.ready();
    assert!(err, "writing to a closed peer is EPIPE");
    assert!(w, "a doomed write must not park: poll says writable");
    assert!(a.tx.borrow().rd_closed);
}

#[test]
fn close_discards_what_nobody_will_read() {
    let (a, b) = pair::<u32>();
    a.tx.borrow_mut().push(b"unread", &mut none());
    let tx = a.tx.clone();
    drop(b); // b never read: its rx is emptied and marked
    assert_eq!(tx.borrow().len(), 0);
    assert!(tx.borrow().rd_closed);
}

#[test]
fn shutdown_halves() {
    let (a, b) = pair::<u32>();
    assert!(a.shutdown(SHUT_WR));
    // the peer reads EOF, but can still write to us
    assert!(b.rx.borrow().wr_closed);
    b.tx.borrow_mut().push(b"still here", &mut none());
    assert_eq!(a.rx.borrow_mut().pop(64, false).data, b"still here");
    // SHUT_RD on a: b's writes now fail
    assert!(a.shutdown(SHUT_RD));
    assert!(b.tx.borrow().rd_closed);
    assert!(!a.shutdown(9), "bad `how`");
}

#[test]
fn shutdown_both_equals_close_for_the_peer() {
    let (a, b) = pair::<u32>();
    a.tx.borrow_mut().push(b"abc", &mut none());
    assert!(a.shutdown(SHUT_RDWR));
    assert_eq!(b.rx.borrow_mut().pop(64, false).data, b"abc");
    assert!(b.rx.borrow().wr_closed);
    assert!(b.ready().3);
}

#[test]
fn rights_ride_the_first_byte_of_their_write() {
    let (a, b) = pair::<u32>();
    a.tx.borrow_mut().push(b"AB", &mut none());
    let mut r = Some(vec![10u32, 11]);
    a.tx.borrow_mut().push(b"CD", &mut r);
    assert!(r.is_none(), "rights consumed by the push that stored bytes");
    // first read: must stop before the carrying byte 'C'
    let p1 = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p1.data, b"AB");
    assert!(p1.rights.is_empty());
    // second read: the batch arrives with C
    let p2 = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p2.data, b"CD");
    assert_eq!(p2.rights, vec![10, 11]);
    assert!(!p2.ctrunc);
}

#[test]
fn two_batches_are_never_merged() {
    let (a, b) = pair::<u32>();
    let mut r1 = Some(vec![1u32]);
    let mut r2 = Some(vec![2u32, 3]);
    a.tx.borrow_mut().push(b"xx", &mut r1);
    a.tx.borrow_mut().push(b"yy", &mut r2);
    let p1 = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p1.data, b"xx");
    assert_eq!(p1.rights, vec![1]);
    let p2 = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p2.data, b"yy");
    assert_eq!(p2.rights, vec![2, 3]);
}

#[test]
fn a_reader_that_cannot_take_rights_discards_them() {
    let (a, b) = pair::<u32>();
    let mut r = Some(vec![5u32]);
    a.tx.borrow_mut().push(b"z", &mut r);
    let p = b.rx.borrow_mut().pop(64, false);
    assert_eq!(p.data, b"z");
    assert!(p.rights.is_empty());
    assert!(p.ctrunc);
    assert_eq!(p.discarded, vec![5], "handed back for the caller to drop");
    // and they are really gone
    a.tx.borrow_mut().push(b"w", &mut none());
    let p = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p.data, b"w");
    assert!(p.rights.is_empty() && !p.ctrunc);
}

#[test]
fn rights_with_no_room_are_not_consumed() {
    let (a, _b) = pair::<u32>();
    let fill = vec![0u8; CHAN_CAP];
    a.tx.borrow_mut().push(&fill, &mut none());
    let mut r = Some(vec![9u32]);
    assert_eq!(a.tx.borrow_mut().push(b"q", &mut r), 0);
    assert!(r.is_some(), "still the caller's: it will retry after parking");
}

#[test]
fn rights_in_flight_are_dropped_when_the_reader_closes() {
    // T with a drop counter: a description held in flight must be released
    // when its reader goes away, or a pipe end would stay open forever.
    struct Tok(Rc<Cell<u32>>);
    impl Drop for Tok {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let dropped = Rc::new(Cell::new(0));
    let (a, b) = pair::<Tok>();
    let mut r = Some(vec![Tok(dropped.clone()), Tok(dropped.clone())]);
    a.tx.borrow_mut().push(b"m", &mut r);
    assert_eq!(dropped.get(), 0);
    drop(b);
    assert_eq!(dropped.get(), 2);
    drop(a);
    assert_eq!(dropped.get(), 2);
}

#[test]
fn peek_does_not_consume_and_respects_batches() {
    let (a, b) = pair::<u32>();
    a.tx.borrow_mut().push(b"12", &mut none());
    let mut r = Some(vec![4u32]);
    a.tx.borrow_mut().push(b"34", &mut r);
    assert_eq!(b.rx.borrow().peek(64), b"12");
    assert_eq!(b.rx.borrow().peek(1), b"1");
    assert_eq!(b.rx.borrow().len(), 4);
    let p = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p.data, b"12");
    // peek at a batch head: rights stay queued
    assert_eq!(b.rx.borrow().peek(64), b"34");
    let p = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p.rights, vec![4]);
}

#[test]
fn read_end_shutdown_drops_rights_and_resets_positions() {
    let (a, b) = pair::<u32>();
    let mut r = Some(vec![1u32]);
    a.tx.borrow_mut().push(b"k", &mut r);
    assert!(b.shutdown(SHUT_RD));
    assert_eq!(b.rx.borrow().len(), 0);
    // positions stay consistent for a later (unreachable, but defensive) push
    let mut r2 = Some(vec![2u32]);
    a.tx.borrow_mut().push(b"l", &mut r2);
    let p = b.rx.borrow_mut().pop(64, true);
    assert_eq!(p.data, b"l");
    assert_eq!(p.rights, vec![2]);
}

#[test]
fn listener_queues_up_to_the_backlog() {
    let cred = PeerCred { pid: 7, uid: 1000, gid: 100 };
    let mut l = Listener::<u32>::new(2, cred);
    assert!(!l.pending());
    let (c1, s1) = pair::<u32>();
    let (c2, s2) = pair::<u32>();
    let (c3, s3) = pair::<u32>();
    assert!(l.queue(s1).is_ok());
    assert!(l.queue(s2).is_ok());
    let back = l.queue(s3).err().expect("full");
    assert!(l.pending());
    // the refused half comes back intact: the connect can park and retry
    assert!(back.tx.borrow().wr_closed == false);
    // accept in FIFO order
    c1.tx.borrow_mut().push(b"first", &mut none());
    c2.tx.borrow_mut().push(b"second", &mut none());
    let a1 = l.take().unwrap();
    let a2 = l.take().unwrap();
    assert!(l.take().is_none());
    assert_eq!(a1.rx.borrow_mut().pop(64, false).data, b"first");
    assert_eq!(a2.rx.borrow_mut().pop(64, false).data, b"second");
    // and now there is room again
    assert!(l.queue(back).is_ok());
    drop(c3);
}

#[test]
fn dropping_a_listener_closes_the_waiting_clients() {
    let mut l = Listener::<u32>::new(4, PeerCred::default());
    let (c, s) = pair::<u32>();
    l.queue(s).ok().unwrap();
    drop(l);
    // the client's read side sees EOF, its writes fail
    assert!(c.rx.borrow().wr_closed);
    assert!(c.ready().3);
}

#[test]
fn backlog_is_clamped() {
    let mut l = Listener::<u32>::new(0, PeerCred::default());
    let (_c, s) = pair::<u32>();
    assert!(l.queue(s).is_ok(), "listen(0) still accepts one");
    let (_c2, s2) = pair::<u32>();
    assert!(l.queue(s2).is_err());
    l.set_backlog(100_000);
    for _ in 0..MAX_BACKLOG - 1 {
        let (c, s) = pair::<u32>();
        std::mem::forget(c);
        assert!(l.queue(s).is_ok());
    }
    let (_c3, s3) = pair::<u32>();
    assert!(l.queue(s3).is_err());
}

#[test]
fn readiness_of_a_fresh_pair() {
    let (a, b) = pair::<u32>();
    // nothing to read, room to write, nobody hung up
    assert_eq!(a.ready(), (false, true, false, false));
    b.tx.borrow_mut().push(b"x", &mut none());
    assert_eq!(a.ready(), (true, true, false, false));
    // a full tx is not writable
    a.tx.borrow_mut().push(&vec![0u8; CHAN_CAP], &mut none());
    assert_eq!(a.ready(), (true, false, false, false));
}

#[test]
fn a_socket_sent_over_itself_does_not_re_enter_its_channel() {
    // The only reference to endpoint `a` is a right in flight on the very
    // channel that endpoint `b` reads.  Closing `b` clears that channel,
    // which releases `a`, whose Drop touches the same channel again.  With
    // the rights dropped under the borrow this is a BorrowMutError.
    struct Held(std::cell::RefCell<Option<Endpoint<Held>>>);
    let (a, b) = pair::<Held>();
    let a_tx = a.tx.clone();
    let holder = Held(std::cell::RefCell::new(Some(a)));
    let mut r = Some(vec![holder]);
    a_tx.borrow_mut().push(b"!", &mut r);
    assert!(r.is_none());
    drop(b); // must not panic
    assert!(a_tx.borrow().rd_closed);
}

#[test]
fn shutdown_rd_releases_rights_outside_the_borrow() {
    struct Held(std::cell::RefCell<Option<Endpoint<Held>>>);
    let (a, b) = pair::<Held>();
    let a_tx = a.tx.clone();
    let holder = Held(std::cell::RefCell::new(Some(a)));
    let mut r = Some(vec![holder]);
    a_tx.borrow_mut().push(b"!", &mut r);
    assert!(b.shutdown(SHUT_RD)); // must not panic either
    drop(b);
}
