/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/unixsock -- AF_UNIX stream sockets, end to end (kernel syscalls
 * 90..102, sys/kern/src/user/fd/unix.rs).  One line per scenario, and a
 * summary ("unixsock: all 10 passed") which is what `make test` looks for.
 * The state machine under it is host-tested by tests/unix_host.rs; this
 * program checks the part that needs a kernel: descriptors, blocking,
 * fork, names, poll.
 *
 *   pair      socketpair round trip in both directions, a short read keeps
 *             the rest, EOF once the peer closed, EPIPE (MSG_NOSIGNAL) to a
 *             closed peer
 *   named     bind creates the node, a second bind -> EADDRINUSE, connect
 *             before listen -> ECONNREFUSED, listen / connect / accept,
 *             getsockname / getpeername, ENOENT for a name nobody bound
 *   abstract  the same in the abstract namespace (sun_path[0] == 0)
 *   forkecho  a forked child accepts and echoes; the parent makes 200
 *             request / reply round trips (the elapsed time is reported:
 *             parked sockets are woken at once, not at a 10 ms quantum)
 *   rights    SCM_RIGHTS: a pipe's read end crosses the socket and reads
 *             the bytes written before it was sent; a reader with no room
 *             for descriptors gets MSG_CTRUNC and still gets its byte
 *   nonblock  SOCK_NONBLOCK: read / recv / accept answer EAGAIN, a writer
 *             fills the buffer and gets EAGAIN, a drain makes room again
 *   shutdown  SHUT_WR -> EOF at the peer, the reverse direction still flows;
 *             EINVAL on a bad `how`, a no-op (0) on an unconnected socket
 *   poll      POLLIN follows the data, POLLHUP|POLLIN follows a close, and a
 *             poll blocked in the kernel wakes when a forked child writes
 *   opts      SO_TYPE, SO_PEERCRED (our own pid / uid), FIONREAD, fstat says
 *             S_ISSOCK, setsockopt habits accepted, unknown option refused
 *   errors    the families / types / states the kernel answers honestly:
 *             AF_INET, SOCK_DGRAM, a non-socket fd, accept on a connected
 *             socket, connect twice
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails;

static void check(const char *what, int ok, const char *detail)
{
        printf("  %-9s %s -- %s\n", what, ok ? "ok" : "FAIL", detail);
        if (!ok)
                fails++;
}

static void fmt(char *out, size_t size, const char *f, ...)
{
        va_list ap;

        va_start(ap, f);
        vsnprintf(out, size, f, ap);
        va_end(ap);
}

/* A call's answer as the kernel's negative errno. */
static long ec(long r)
{
        return r >= 0 ? r : -errno;
}

static long expect(long got, long want)
{
        return got == want;
}

static long now_ms(void)
{
        struct timespec ts;

        clock_gettime(CLOCK_MONOTONIC, &ts);
        return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

/* A file-system address: returns the length to pass to bind / connect. */
static socklen_t fs_addr(struct sockaddr_un *a, const char *path)
{
        memset(a, 0, sizeof *a);
        a->sun_family = AF_UNIX;
        strcpy(a->sun_path, path);
        return offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1;
}

/* An abstract address: sun_path[0] == 0, the name's bytes, no NUL. */
static socklen_t abs_addr(struct sockaddr_un *a, const char *name)
{
        memset(a, 0, sizeof *a);
        a->sun_family = AF_UNIX;
        memcpy(a->sun_path + 1, name, strlen(name));
        return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

/* ---- pair --------------------------------------------------------------- */

static int sc_pair(void)
{
        int sv[2];
        char b[16];
        char d[80];
        int bad = 0;

        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
                check("pair", 0, "socketpair failed");
                return 1;
        }
        bad += write(sv[0], "ping", 4) != 4;
        bad += read(sv[1], b, sizeof b) != 4 || memcmp(b, "ping", 4);
        bad += write(sv[1], "pong!", 5) != 5;
        bad += read(sv[0], b, 2) != 2 || memcmp(b, "po", 2); /* short read */
        bad += read(sv[0], b, sizeof b) != 3 || memcmp(b, "ng!", 3);
        close(sv[0]);
        bad += read(sv[1], b, sizeof b) != 0; /* EOF */
        bad += !expect(ec(send(sv[1], "x", 1, MSG_NOSIGNAL)), -EPIPE);
        close(sv[1]);
        fmt(d, sizeof d, "%d failed: both ways, short read, EOF, EPIPE", bad);
        check("pair", bad == 0, d);
        return bad;
}

/* ---- named -------------------------------------------------------------- */

static int sc_named(void)
{
        static const char path[] = "/usx.sock";
        struct sockaddr_un a, got;
        socklen_t al, gl;
        int l, l2, c, s;
        char b[16];
        char d[96];
        int bad = 0;

        unlink(path);
        al = fs_addr(&a, path);
        l = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += l < 0;
        bad += bind(l, (struct sockaddr *)&a, al) != 0;
        l2 = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += !expect(ec(bind(l2, (struct sockaddr *)&a, al)), -EADDRINUSE);
        /* bound but not listening yet: refused */
        c = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += !expect(ec(connect(c, (struct sockaddr *)&a, al)), -ECONNREFUSED);
        bad += listen(l, 4) != 0;
        bad += connect(c, (struct sockaddr *)&a, al) != 0;
        gl = sizeof got;
        s = accept(l, (struct sockaddr *)&got, &gl);
        bad += s < 0;
        bad += write(c, "hello", 5) != 5;
        bad += read(s, b, sizeof b) != 5 || memcmp(b, "hello", 5);
        /* the accepted socket answers with the listener's name; the
         * client's peer is that name too */
        gl = sizeof got;
        memset(&got, 0, sizeof got);
        bad += getsockname(s, (struct sockaddr *)&got, &gl) != 0 || strcmp(got.sun_path, path);
        gl = sizeof got;
        memset(&got, 0, sizeof got);
        bad += getpeername(c, (struct sockaddr *)&got, &gl) != 0 || strcmp(got.sun_path, path);
        /* a name nobody bound */
        {
                struct sockaddr_un nope;
                socklen_t nl = fs_addr(&nope, "/usx.nope");
                int x = socket(AF_UNIX, SOCK_STREAM, 0);

                bad += !expect(ec(connect(x, (struct sockaddr *)&nope, nl)), -ENOENT);
                close(x);
        }
        close(c);
        close(s);
        close(l);
        close(l2);
        unlink(path);
        fmt(d, sizeof d, "%d failed: bind, EADDRINUSE, ECONNREFUSED, accept, names", bad);
        check("named", bad == 0, d);
        return bad;
}

/* ---- abstract ----------------------------------------------------------- */

static int sc_abstract(void)
{
        struct sockaddr_un a, got;
        socklen_t al, gl;
        int l, l2, c, s;
        char b[8];
        char d[96];
        int bad = 0;

        al = abs_addr(&a, "prism-test");
        l = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += bind(l, (struct sockaddr *)&a, al) != 0;
        l2 = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += !expect(ec(bind(l2, (struct sockaddr *)&a, al)), -EADDRINUSE);
        bad += listen(l, 1) != 0;
        c = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += connect(c, (struct sockaddr *)&a, al) != 0;
        s = accept(l, 0, 0); /* NULL address: allowed */
        bad += s < 0;
        bad += write(s, "abs", 3) != 3;
        bad += read(c, b, sizeof b) != 3 || memcmp(b, "abs", 3);
        gl = sizeof got;
        memset(&got, 0, sizeof got);
        bad += getpeername(c, (struct sockaddr *)&got, &gl) != 0 || gl != al || got.sun_path[0] != 0 ||
               memcmp(got.sun_path + 1, "prism-test", 10);
        close(c);
        close(s);
        close(l);
        /* the name is free again once its socket is gone */
        {
                int l3 = socket(AF_UNIX, SOCK_STREAM, 0);

                bad += bind(l3, (struct sockaddr *)&a, al) != 0;
                close(l3);
        }
        close(l2);
        fmt(d, sizeof d, "%d failed: abstract bind, EADDRINUSE, name freed on close", bad);
        check("abstract", bad == 0, d);
        return bad;
}

/* ---- forkecho ----------------------------------------------------------- */

static int sc_forkecho(void)
{
        struct sockaddr_un a;
        socklen_t al;
        int l, c, i;
        pid_t pid;
        int st = 0;
        char out[16], in[16];
        long t0, dt;
        char d[96];
        int bad = 0;

        al = abs_addr(&a, "prism-echo");
        l = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += bind(l, (struct sockaddr *)&a, al) != 0;
        bad += listen(l, 2) != 0;
        pid = fork();
        if (pid == 0) {
                /* the server: accept one client, echo until EOF */
                int s = accept(l, 0, 0);
                char buf[64];
                long n;

                if (s < 0)
                        _exit(2);
                while ((n = read(s, buf, sizeof buf)) > 0)
                        if (write(s, buf, n) != n)
                                _exit(3);
                _exit(0);
        }
        c = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += connect(c, (struct sockaddr *)&a, al) != 0; /* parks until the child accepts */
        t0 = now_ms();
        for (i = 0; i < 200; i++) {
                fmt(out, sizeof out, "req%05d", i);
                if (write(c, out, 8) != 8 || read(c, in, 8) != 8 || memcmp(in, out, 8)) {
                        bad++;
                        break;
                }
        }
        dt = now_ms() - t0;
        close(c);
        waitpid(pid, &st, 0);
        bad += !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
        close(l);
        fmt(d, sizeof d, "%d failed: 200 echoes across a fork in %ld ms", bad, dt);
        check("forkecho", bad == 0, d);
        return bad;
}

/* ---- rights ------------------------------------------------------------- */

static int send_fd(int sock, int fd, char byte)
{
        struct msghdr m;
        struct iovec iv;
        union {
                char buf[CMSG_SPACE(sizeof(int))];
                struct cmsghdr align;
        } u;
        struct cmsghdr *c;

        memset(&m, 0, sizeof m);
        memset(&u, 0, sizeof u);
        iv.iov_base = &byte;
        iv.iov_len = 1;
        m.msg_iov = &iv;
        m.msg_iovlen = 1;
        m.msg_control = u.buf;
        m.msg_controllen = sizeof u.buf;
        c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof(int));
        return sendmsg(sock, &m, 0);
}

static int sc_rights(void)
{
        int sv[2], p[2], q[2];
        struct msghdr m;
        struct iovec iv;
        union {
                char buf[CMSG_SPACE(sizeof(int))];
                struct cmsghdr align;
        } u;
        struct cmsghdr *c;
        char byte = 0, b[16];
        int got = -1;
        char d[96];
        int bad = 0;

        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) || pipe(p) || pipe(q)) {
                check("rights", 0, "setup failed");
                return 1;
        }
        write(p[1], "through", 7);
        bad += send_fd(sv[0], p[0], 'R') != 1;
        close(p[0]); /* only the descriptor in flight keeps the read end open */

        memset(&m, 0, sizeof m);
        memset(&u, 0, sizeof u);
        iv.iov_base = &byte;
        iv.iov_len = 1;
        m.msg_iov = &iv;
        m.msg_iovlen = 1;
        m.msg_control = u.buf;
        m.msg_controllen = sizeof u.buf;
        bad += recvmsg(sv[1], &m, 0) != 1 || byte != 'R';
        c = CMSG_FIRSTHDR(&m);
        if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS &&
            c->cmsg_len == CMSG_LEN(sizeof(int)))
                memcpy(&got, CMSG_DATA(c), sizeof(int));
        else
                bad++;
        bad += (m.msg_flags & MSG_CTRUNC) != 0;
        bad += got < 0 || read(got, b, sizeof b) != 7 || memcmp(b, "through", 7);
        if (got >= 0)
                close(got);

        /* no room for descriptors: the byte arrives, the right is dropped */
        bad += send_fd(sv[0], q[0], 'T') != 1;
        memset(&m, 0, sizeof m);
        byte = 0;
        iv.iov_base = &byte;
        iv.iov_len = 1;
        m.msg_iov = &iv;
        m.msg_iovlen = 1;
        bad += recvmsg(sv[1], &m, 0) != 1 || byte != 'T';
        bad += !(m.msg_flags & MSG_CTRUNC);

        close(q[0]);
        close(q[1]);
        close(p[1]);
        close(sv[0]);
        close(sv[1]);
        fmt(d, sizeof d, "%d failed: a pipe crossed the socket, MSG_CTRUNC without room", bad);
        check("rights", bad == 0, d);
        return bad;
}

/* ---- nonblock ----------------------------------------------------------- */

static int sc_nonblock(void)
{
        int sv[2], l;
        struct sockaddr_un a;
        socklen_t al;
        static char blk[4096];
        char b[4096];
        long total = 0, r;
        char d[96];
        int bad = 0;

        if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sv)) {
                check("nonblock", 0, "socketpair failed");
                return 1;
        }
        bad += !expect(ec(read(sv[0], b, sizeof b)), -EAGAIN);
        bad += !expect(ec(recv(sv[0], b, 1, MSG_DONTWAIT)), -EAGAIN);
        while ((r = write(sv[0], blk, sizeof blk)) > 0)
                total += r;
        bad += !(r < 0 && errno == EAGAIN) || total < 16384;
        /* a drain makes room again */
        for (r = 0; r < total;) {
                long n = read(sv[1], b, sizeof b);

                if (n <= 0)
                        break;
                r += n;
        }
        bad += r != total;
        bad += write(sv[0], "x", 1) != 1;
        close(sv[0]);
        close(sv[1]);

        al = abs_addr(&a, "prism-nb");
        l = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
        bad += bind(l, (struct sockaddr *)&a, al) != 0 || listen(l, 1) != 0;
        bad += !expect(ec(accept(l, 0, 0)), -EAGAIN);
        close(l);
        fmt(d, sizeof d, "%d failed: EAGAIN on read / recv / accept, buffer filled at %ld bytes", bad, total);
        check("nonblock", bad == 0, d);
        return bad;
}

/* ---- shutdown ----------------------------------------------------------- */

static int sc_shutdown(void)
{
        int sv[2], u;
        char b[8];
        char d[96];
        int bad = 0;

        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
                check("shutdown", 0, "socketpair failed");
                return 1;
        }
        bad += write(sv[0], "last", 4) != 4;
        bad += shutdown(sv[0], SHUT_WR) != 0;
        bad += read(sv[1], b, sizeof b) != 4;     /* data first */
        bad += read(sv[1], b, sizeof b) != 0;     /* then EOF */
        bad += write(sv[1], "back", 4) != 4;      /* the other way still flows */
        bad += read(sv[0], b, sizeof b) != 4 || memcmp(b, "back", 4);
        bad += !expect(ec(send(sv[0], "x", 1, MSG_NOSIGNAL)), -EPIPE);
        bad += !expect(ec(shutdown(sv[0], 7)), -EINVAL);
        close(sv[0]);
        close(sv[1]);
        /* no connection: a no-op that succeeds, like Linux */
        u = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += shutdown(u, SHUT_RDWR) != 0;
        close(u);
        fmt(d, sizeof d, "%d failed: SHUT_WR is EOF one way only, bad how, no-op unconnected", bad);
        check("shutdown", bad == 0, d);
        return bad;
}

/* ---- poll --------------------------------------------------------------- */

static int sc_poll(void)
{
        int sv[2];
        struct pollfd pf;
        char b[8];
        pid_t pid;
        long t0;
        char d[96];
        int bad = 0;

        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
                check("poll", 0, "socketpair failed");
                return 1;
        }
        pf.fd = sv[1];
        pf.events = POLLIN;
        bad += poll(&pf, 1, 0) != 0;
        write(sv[0], "z", 1);
        pf.revents = 0;
        bad += poll(&pf, 1, 0) != 1 || !(pf.revents & POLLIN);
        read(sv[1], b, sizeof b);

        /* a poll blocked in the kernel is woken by a forked writer */
        pid = fork();
        if (pid == 0) {
                usleep(40000);
                write(sv[0], "w", 1);
                _exit(0);
        }
        t0 = now_ms();
        pf.revents = 0;
        bad += poll(&pf, 1, 3000) != 1 || !(pf.revents & POLLIN);
        bad += now_ms() - t0 >= 2500;
        waitpid(pid, 0, 0);
        read(sv[1], b, sizeof b);

        close(sv[0]); /* hang-up */
        pf.revents = 0;
        bad += poll(&pf, 1, 0) != 1 || !(pf.revents & (POLLIN | POLLHUP));
        close(sv[1]);
        fmt(d, sizeof d, "%d failed: POLLIN follows data, a blocked poll wakes, POLLHUP", bad);
        check("poll", bad == 0, d);
        return bad;
}

/* ---- opts --------------------------------------------------------------- */

static int sc_opts(void)
{
        int sv[2], v = -1, n = -1;
        socklen_t vl;
        struct kucred {
                int pid;
                unsigned uid, gid;
        } uc;
        struct stat st;
        char d[96];
        int bad = 0;

        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
                check("opts", 0, "socketpair failed");
                return 1;
        }
        vl = sizeof v;
        bad += getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &v, &vl) != 0 || v != SOCK_STREAM;
        vl = sizeof uc;
        memset(&uc, 0, sizeof uc);
        bad += getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &uc, &vl) != 0 || uc.pid != (int)getpid() ||
               uc.uid != (unsigned)getuid();
        write(sv[0], "12345", 5);
        bad += ioctl(sv[1], FIONREAD, &n) != 0 || n != 5;
        bad += fstat(sv[0], &st) != 0 || !S_ISSOCK(st.st_mode);
        v = 1;
        bad += setsockopt(sv[0], SOL_SOCKET, SO_REUSEADDR, &v, sizeof v) != 0;
        bad += !expect(ec(setsockopt(sv[0], SOL_SOCKET, 12345, &v, sizeof v)), -ENOPROTOOPT);
        close(sv[0]);
        close(sv[1]);
        fmt(d, sizeof d, "%d failed: SO_TYPE, SO_PEERCRED, FIONREAD, S_ISSOCK, setsockopt", bad);
        check("opts", bad == 0, d);
        return bad;
}

/* ---- errors ------------------------------------------------------------- */

static int sc_errors(void)
{
        int sv[2], u, lz;
        struct sockaddr_un a;
        socklen_t al;
        char d[96];
        int bad = 0;

        bad += !expect(ec(socket(AF_INET, SOCK_STREAM, 0)), -EAFNOSUPPORT);
        bad += !expect(ec(socket(AF_UNIX, SOCK_DGRAM, 0)), -ESOCKTNOSUPPORT);
        al = abs_addr(&a, "prism-err");
        bad += !expect(ec(bind(0, (struct sockaddr *)&a, al)), -ENOTSOCK); /* fd 0 is the console */
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
                check("errors", 0, "socketpair failed");
                return 1;
        }
        bad += !expect(ec(accept(sv[0], 0, 0)), -EINVAL);   /* not listening */
        bad += !expect(ec(listen(sv[0], 1)), -EINVAL);      /* unbound / connected */
        /* connect on a connected socket -> EISCONN, even to a live listener */
        lz = socket(AF_UNIX, SOCK_STREAM, 0);
        bad += bind(lz, (struct sockaddr *)&a, al) != 0 || listen(lz, 1) != 0;
        bad += !expect(ec(connect(sv[0], (struct sockaddr *)&a, al)), -EISCONN);
        close(lz);
        u = socket(AF_UNIX, SOCK_STREAM, 0);
        {
                struct sockaddr_un got;
                socklen_t gl = sizeof got;

                bad += !expect(ec(getpeername(u, (struct sockaddr *)&got, &gl)), -ENOTCONN);
        }
        close(u);
        close(sv[0]);
        close(sv[1]);
        fmt(d, sizeof d, "%d failed: AF_INET, SOCK_DGRAM, ENOTSOCK, EINVAL, EISCONN, ENOTCONN", bad);
        check("errors", bad == 0, d);
        return bad;
}

int main(int argc, char **argv)
{
        (void)argc;
        (void)argv;
        setvbuf(stdout, 0, _IONBF, 0);

        printf("unixsock (pid %ld): AF_UNIX stream sockets\n", (long)getpid());
        sc_pair();
        sc_named();
        sc_abstract();
        sc_forkecho();
        sc_rights();
        sc_nonblock();
        sc_shutdown();
        sc_poll();
        sc_opts();
        sc_errors();
        if (fails == 0)
                printf("unixsock: all 10 passed\n");
        else
                printf("unixsock: %d failure(s), FAILURES\n", fails);
        return fails ? 1 : 0;
}
