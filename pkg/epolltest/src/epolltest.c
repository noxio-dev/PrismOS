/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /usr/bin/epolltest -- epoll end to end (kernel syscalls 109..111,
 * sys/kern/src/user/fd/epoll.rs).  One line per scenario and a summary
 * ("epolltest: all 8 passed").  The interest list is host-tested by
 * tests/epoll_host.rs; this checks what needs a kernel: descriptors,
 * blocking, fork, nesting.
 *
 *   level     a pipe: nothing ready -> 0, written -> EPOLLIN with the data
 *             word we stored, still ready until drained (level-triggered)
 *   edge      EPOLLET reports once per arrival, not while data stays unread
 *   oneshot   EPOLLONESHOT reports once; EPOLL_CTL_MOD re-arms it
 *   hup       the write end closed -> EPOLLHUP on the read end
 *   socket    a socketpair: EPOLLOUT at once, EPOLLIN after the peer writes
 *   nested    an epoll watched by another epoll is readable with its child
 *   errors    EEXIST, ENOENT, EBADF, EINVAL (itself, bad op, not an epoll),
 *             EPERM on a regular file
 *   wake      a wait with a timeout is woken by a forked child's write
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;

static void check(const char *what, int ok, const char *detail)
{
        printf("  %-8s %s -- %s\n", what, ok ? "ok" : "FAIL", detail);
        if (!ok)
                fails++;
}

static int add(int ep, int fd, uint32_t events, uint64_t data)
{
        struct epoll_event ev = { .events = events, .data = { .u64 = data } };
        return epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev);
}

static void level(void)
{
        int p[2], ep = epoll_create1(0);
        struct epoll_event out[4];
        char c;
        pipe(p);
        add(ep, p[0], EPOLLIN, 0xABCDEF0123ull);
        int n0 = epoll_wait(ep, out, 4, 0);
        write(p[1], "x", 1);
        int n1 = epoll_wait(ep, out, 4, 0);
        int ok1 = n1 == 1 && (out[0].events & EPOLLIN) && out[0].data.u64 == 0xABCDEF0123ull;
        int n2 = epoll_wait(ep, out, 4, 0);
        read(p[0], &c, 1);
        int n3 = epoll_wait(ep, out, 4, 0);
        check("level", n0 == 0 && ok1 && n2 == 1 && n3 == 0, "idle 0, ready, still ready, drained 0");
        close(ep); close(p[0]); close(p[1]);
}

static void edge(void)
{
        int p[2], ep = epoll_create1(0);
        struct epoll_event out[2];
        pipe(p);
        add(ep, p[0], EPOLLIN | EPOLLET, 1);
        write(p[1], "a", 1);
        int a = epoll_wait(ep, out, 2, 0);
        int b = epoll_wait(ep, out, 2, 0);
        write(p[1], "b", 1);
        int c = epoll_wait(ep, out, 2, 0);
        check("edge", a == 1 && b == 0 && c <= 1,
              "one report for the first byte, none while unread");
        close(ep); close(p[0]); close(p[1]);
}

static void oneshot(void)
{
        int p[2], ep = epoll_create1(0);
        struct epoll_event out[2], ev = { .events = EPOLLIN | EPOLLONESHOT, .data = { .u32 = 7 } };
        pipe(p);
        epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
        write(p[1], "x", 1);
        int a = epoll_wait(ep, out, 2, 0);
        int b = epoll_wait(ep, out, 2, 0);
        epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev);
        int c = epoll_wait(ep, out, 2, 0);
        check("oneshot", a == 1 && b == 0 && c == 1, "reports once, MOD re-arms");
        close(ep); close(p[0]); close(p[1]);
}

static void hup(void)
{
        int p[2], ep = epoll_create1(0);
        struct epoll_event out[2];
        pipe(p);
        add(ep, p[0], EPOLLIN, 0);
        close(p[1]);
        int n = epoll_wait(ep, out, 2, 0);
        check("hup", n == 1 && (out[0].events & EPOLLHUP), "writer closed -> EPOLLHUP");
        close(ep); close(p[0]);
}

static void sock(void)
{
        int sv[2], ep = epoll_create1(0);
        struct epoll_event out[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
        add(ep, sv[0], EPOLLIN | EPOLLOUT, 0);
        int a = epoll_wait(ep, out, 2, 0);
        int only_out = a == 1 && out[0].events == EPOLLOUT;
        write(sv[1], "hi", 2);
        int b = epoll_wait(ep, out, 2, 0);
        check("socket", only_out && b == 1 && (out[0].events & EPOLLIN) && (out[0].events & EPOLLOUT),
              "OUT at once, IN after the peer wrote");
        close(ep); close(sv[0]); close(sv[1]);
}

static void nested(void)
{
        int p[2], inner = epoll_create1(0), outer = epoll_create1(0);
        struct epoll_event out[2];
        pipe(p);
        add(inner, p[0], EPOLLIN, 0);
        add(outer, inner, EPOLLIN, 5);
        int a = epoll_wait(outer, out, 2, 0);
        write(p[1], "x", 1);
        int b = epoll_wait(outer, out, 2, 0);
        int loop = add(inner, outer, EPOLLIN, 0); /* would close a cycle */
        check("nested", a == 0 && b == 1 && out[0].data.u32 == 5 && loop == -1 && errno == ELOOP,
              "child readiness shows through, a cycle is ELOOP");
        close(outer); close(inner); close(p[0]); close(p[1]);
}

static void errors(void)
{
        int p[2], ep = epoll_create1(0), ep2 = epoll_create1(0);
        struct epoll_event ev = { .events = EPOLLIN };
        pipe(p);
        int ok = add(ep, p[0], EPOLLIN, 0) == 0;
        ok &= add(ep, p[0], EPOLLIN, 0) == -1 && errno == EEXIST;
        ok &= epoll_ctl(ep, EPOLL_CTL_DEL, p[1], &ev) == -1 && errno == ENOENT;
        ok &= epoll_ctl(ep, EPOLL_CTL_ADD, 999, &ev) == -1 && errno == EBADF;
        ok &= epoll_ctl(ep, EPOLL_CTL_ADD, ep, &ev) == -1 && errno == EINVAL;
        ok &= epoll_ctl(ep, 99, p[1], &ev) == -1 && errno == EINVAL;
        ok &= epoll_ctl(p[0], EPOLL_CTL_ADD, p[1], &ev) == -1 && errno == EINVAL;
        ok &= epoll_wait(ep, &ev, 0, 0) == -1 && errno == EINVAL;
        int f = open("/etc/passwd", O_RDONLY);
        ok &= f >= 0 && add(ep2, f, EPOLLIN, 0) == -1 && errno == EPERM;
        check("errors", ok, "EEXIST ENOENT EBADF EINVAL EPERM");
        close(ep); close(ep2); close(p[0]); close(p[1]);
}

static void wake(void)
{
        int p[2], ep = epoll_create1(0);
        struct epoll_event out[2];
        pipe(p);
        add(ep, p[0], EPOLLIN, 0);
        pid_t pid = fork();
        if (pid == 0) {
                usleep(100 * 1000);
                write(p[1], "x", 1);
                _exit(0);
        }
        int n = epoll_wait(ep, out, 2, 3000);
        int st;
        waitpid(pid, &st, 0);
        check("wake", n == 1 && (out[0].events & EPOLLIN), "a blocked wait wakes on the child's write");
        close(ep); close(p[0]); close(p[1]);
}

int main(void)
{
        printf("epolltest: epoll_create1 / epoll_ctl / epoll_wait\n");
        level(); edge(); oneshot(); hup(); sock(); nested(); errors(); wake();
        if (fails)
                printf("epolltest: %d failure(s), FAILURES\n", fails);
        else
                printf("epolltest: all 8 passed\n");
        return fails ? 1 : 0;
}
