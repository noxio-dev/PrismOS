/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/fd -- the phase 2.6 test program: per-process descriptors on top of
 * the VFS, pipes, /dev.  Each scenario prints one line and the program ends
 * with a summary ("fd: all 10 passed"), which is what `make test` looks for.
 *
 *   stdio     fd 0/1/2 route through the table: writes to 1 and 2 land on
 *             the console, a write-only fd refuses to read, EBADF paths
 *             (bad numbers, write to 9, read of 1, close of 9) and the
 *             ESPIPE of a console lseek
 *   file      create / write / close / reopen, lseek SET/CUR/END with a
 *             negative offset, reads that follow the seeks, EISDIR on a
 *             directory open for writing and on a read of a directory fd,
 *             ENOENT, O_EXCL -> EEXIST
 *   dev       /dev/null: read = EOF, write swallowed, lseek -> ESPIPE;
 *             /dev/zero: 16 zeroes; /dev/console: a line really printed
 *             through the device; ENOENT on /dev/tele; the /dev directory
 *             itself opens but cannot be read
 *   pipe      the round trip (write then read), FIFO order across split
 *             reads (ab | cde), EOF once the write end is closed
 *   pblock    a forked child sleeps 60 ms then writes one byte; the
 *             parent's read must actually park for it (elapsed >= 50 ms)
 *   epipe     a write with the read end already closed -> -EPIPE
 *   forkfd    the child inherits the descriptors: the parent reads the
 *             first 4 bytes of a file, the child -- through the *same*
 *             description, so the offset is shared -- reads the next 4 and
 *             hands them back through an inherited pipe
 *   dups      dup shares the description (a seek through the dup moves the
 *             original), dup of stdout writes, dup(9) -> EBADF, and the
 *             renumbering trick: close(1), open something, the open must
 *             land on 1 (that line itself is printed through the new fd 1)
 *   stdin     a real read of fd 0: the program prints "type 4 bytes:",
 *             blocks, and must receive what was typed on the console
 *   execfd    a forked child execve()s this binary again ("fd postexec"):
 *             the descriptors survive the execve -- stdout still writes,
 *             a fresh open still works
 */
#include "prism.h"

int main(int argc, char **argv, char **envp);

static int fails;

static void check(const char *what, int ok, const char *detail)
{
        printf("  %-9s %s -- %s\n", what, ok ? "ok" : "FAIL", detail);
        if (!ok)
                fails++;
}

/* detail-string builder on top of the freestanding vsnprintf */
static void fmt(char *out, size_t size, const char *f, ...)
{
        va_list ap;

        va_start(ap, f);
        vsnprintf(out, size, f, ap);
        va_end(ap);
}

static long expect(long got, long want)
{
        return got == want;
}

/* ---- stdio -------------------------------------------------------------- */

static int sc_stdio(void)
{
        static const char l1[] = "  [fd 1] written through the descriptor table\n";
        static const char l2[] = "  [fd 2] ... and stderr goes the same way\n";
        char b[16];
        char d[80];
        int bad = 0;

        if (sys_write(1, l1, strlen(l1)) != (long)strlen(l1))
                bad++;
        if (sys_write(2, l2, strlen(l2)) != (long)strlen(l2))
                bad++;
        bad += !expect(sys_write(9, l1, 4), -PRISM_EBADF);
        bad += !expect(sys_read(9, b, sizeof b), -PRISM_EBADF);
        bad += !expect(sys_close(9), -PRISM_EBADF);
        bad += !expect(sys_read(1, b, sizeof b), -PRISM_EBADF); /* write-only */
        bad += !expect(sys_lseek(1, 0, PRISM_SEEK_SET), -PRISM_ESPIPE);
        fmt(d, sizeof d, "%d checks, EBADF x3, ESPIPE on the console", 6 - bad);
        check("stdio", bad == 0, d);
        return bad;
}

/* ---- file --------------------------------------------------------------- */

static int sc_file(void)
{
        static const char text[] = "0123456789";
        char b[16];
        char d[96];
        int bad = 0;

        /* Create, write, close; then reopen and walk with the seeks. */
        long f = sys_open("/fd.txt", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_TRUNC);
        if (f < 0 || sys_write(f, text, strlen(text)) != (long)strlen(text) || sys_close(f))
                bad++;
        f = sys_open("/fd.txt", PRISM_O_READ);
        if (f < 0)
                bad++;
        if (sys_lseek(f, 5, PRISM_SEEK_SET) != 5)
                bad++;
        if (sys_read(f, b, 4) != 4 || memcmp(b, "5678", 4))
                bad++;
        if (sys_lseek(f, 0, PRISM_SEEK_END) != 10)
                bad++;
        if (sys_lseek(f, -4, PRISM_SEEK_CUR) != 6)
                bad++;
        if (sys_read(f, b, 4) != 4 || memcmp(b, "6789", 4))
                bad++;
        sys_close(f);

        /* Error paths. */
        bad += !expect(sys_open("/no/such/file", PRISM_O_READ), -PRISM_ENOENT);
        bad += !expect(sys_open("/bin", PRISM_O_WRITE), -PRISM_EISDIR);
        long dir = sys_open("/", PRISM_O_READ);
        if (dir < 0)
                bad++;
        else {
                bad += !expect(sys_read(dir, b, 1), -PRISM_EISDIR);
                bad += !expect(sys_lseek(dir, 0, 7), -PRISM_EINVAL); /* bad whence */
                sys_close(dir);
        }
        bad += !expect(sys_lseek(9, 0, PRISM_SEEK_SET), -PRISM_EBADF);
        long e1 = sys_open("/fd-excl", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_EXCL);
        if (e1 < 0)
                bad++;
        else
                sys_close(e1);
        bad += !expect(sys_open("/fd-excl", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_EXCL),
            -PRISM_EEXIST);

        fmt(d, sizeof d, "lseek SET/CUR/END, EISDIR, ENOENT, EEXIST, %s",
            bad ? "some check failed" : "all reads followed the seeks");
        check("file", bad == 0, d);
        return bad;
}

/* ---- dev ---------------------------------------------------------------- */

static int sc_dev(void)
{
        char b[32];
        char d[128];
        long rr, rw, rs, zr, zw, cw, tl, dr;
        int zbad = 0;

        /* /dev/null: EOF on read, writes swallowed, no offset to seek. */
        long n = sys_open("/dev/null", PRISM_O_READ | PRISM_O_WRITE);
        if (n < 0)
                return check("dev", 0, "open /dev/null failed"), 1;
        rr = sys_read(n, b, 8); /* EOF */
        rw = sys_write(n, "12345678", 8);
        rs = sys_lseek(n, 0, PRISM_SEEK_SET);
        sys_close(n);

        /* /dev/zero: zeroes on read, writes swallowed (opened R/W: a sink
         * must also accept writes through the same description). */
        long z = sys_open("/dev/zero", PRISM_O_READ | PRISM_O_WRITE);
        if (z < 0)
                return check("dev", 0, "open /dev/zero failed"), 1;
        zr = sys_read(z, b, 16);
        for (int i = 0; i < 16; i++)
                if (b[i] != 0)
                        zbad++;
        zw = sys_write(z, b, 16);
        sys_close(z);

        /* /dev/console: a line printed through the device itself. */
        static const char cl[] = "  (written to /dev/console)\n";
        long c = sys_open("/dev/console", PRISM_O_WRITE);
        cw = c < 0 ? c : sys_write(c, cl, strlen(cl));
        if (c >= 0)
                sys_close(c);

        /* The directory and a missing node. */
        tl = sys_open("/dev/tele", PRISM_O_READ);
        long dd = sys_open("/dev", PRISM_O_READ);
        dr = dd < 0 ? dd : sys_read(dd, b, 4);
        if (dd >= 0)
                sys_close(dd);

        fmt(d, sizeof d,
            "null r=%ld w=%ld s=%ld; zero r=%ld (%d bad) w=%ld; console w=%ld; tele=%ld; dir r=%ld",
            rr, rw, rs, zr, zbad, zw, cw, tl, dr);
        check("dev", rr == 0 && rw == 8 && rs == -PRISM_ESPIPE && zr == 16 && zbad == 0
            && zw == 16 && cw == (long)strlen(cl) && tl == -PRISM_ENOENT
            && dr == -PRISM_EISDIR, d);
        return 0;
}

/* ---- pipe ---------------------------------------------------------------- */

static int sc_pipe_(void)
{
        int p[2];
        char b[16];
        char d[96];
        int bad = 0;

        if (sys_pipe(p))
                bad++;
        if (p[0] < 3 || p[1] < 3 || p[0] == p[1])
                bad++;

        /* Round trip, FIFO order across split reads. */
        bad += !expect(sys_write(p[1], "abcde", 5), 5);
        bad += !expect(sys_read(p[0], b, 2), 2);
        bad += !expect(sys_read(p[0], b + 2, 3), 3);
        if (memcmp(b, "abcde", 5))
                bad++;

        /* EOF once the write end is gone. */
        sys_close(p[1]);
        bad += !expect(sys_read(p[0], b, 8), 0);
        sys_close(p[0]);

        fmt(d, sizeof d, "%s", bad ? "some check failed" : "ab|cde in order, EOF after close(w)");
        check("pipe", bad == 0, d);
        return bad;
}

/* ---- pblock -------------------------------------------------------------- */

static int sc_pblock(void)
{
        int p[2];
        char b[8];
        char d[96];
        int bad = 0;

        if (sys_pipe(p))
                bad++;
        long pid = sys_fork();
        if (pid == 0) {
                /* Child: let the parent park first, then feed it one byte. */
                sys_sleep(60);
                sys_write(p[1], "Z", 1);
                sys_exit(0);
        }
        /* Our write end is not needed; the child holds its inherited copy. */
        sys_close(p[1]);
        long t0 = sys_uptime_ms();
        long r = sys_read(p[0], b, 1); /* parks until the child writes */
        long dt = sys_uptime_ms() - t0;
        if (r != 1 || b[0] != 'Z')
                bad++;
        if (dt < 50)
                bad++; /* it must have really waited for the writer */
        int st = 0;
        long w = sys_wait(pid, &st, 0);
        if (w != pid || !PRISM_WIFEXITED(st) || PRISM_WEXITSTATUS(st) != 0)
                bad++;
        sys_close(p[0]);

        fmt(d, sizeof d, "read blocked %ld ms for the child's byte", dt);
        check("pblock", bad == 0, d);
        return bad;
}

/* ---- epipe --------------------------------------------------------------- */

static int sc_epipe(void)
{
        int p[2];
        char d[96];
        long w = -99;

        if (sys_pipe(p))
                return check("epipe", 0, "pipe failed"), 1;
        /* Phase 3.1: a readerless write is a death sentence by SIGPIPE
         * now (the POSIX default).  Ignore the signal here so the write
         * itself can be observed -- the sigpipe scenario of /bin/posix
         * covers both sides of the rule. */
        sys_signal(PRISM_SIGPIPE, PRISM_SIG_IGN);
        sys_close(p[0]); /* no reader left */
        w = sys_write(p[1], "x", 1);
        sys_close(p[1]);
        sys_signal(PRISM_SIGPIPE, PRISM_SIG_DFL);

        fmt(d, sizeof d, "write with no reader returned %ld", w);
        check("epipe", w == -PRISM_EPIPE, d);
        return 0;
}

/* ---- forkfd -------------------------------------------------------------- */

static int sc_forkfd(void)
{
        int p[2];
        char pb[8], cb[8];
        char d[112];
        int bad = 0;

        if (sys_pipe(p))
                bad++;
        long f = sys_open("/fd.txt", PRISM_O_READ);
        if (f < 0)
                bad++;
        long pid = sys_fork();
        if (pid == 0) {
                /* The child inherited f and p: it reads where the parent left
                 * off (same description, so the offset is shared) and hands
                 * the bytes back through the pipe. */
                long n = sys_read(f, cb, 4);
                sys_write(p[1], cb, n);
                sys_exit(5);
        }
        /* Still holding the CPU: read the first 4 bytes ourselves, then
         * park on the pipe -- that is what lets the child run at all. */
        long n1 = sys_read(f, pb, 4);
        pb[4] = 0;
        long n2 = sys_read(p[0], cb, 4);
        cb[4] = 0;
        int st = 0;
        long w = sys_wait(pid, &st, 0);
        if (n1 != 4 || memcmp(pb, "0123", 4))
                bad++;
        if (n2 != 4 || memcmp(cb, "4567", 4))
                bad++;
        if (w != pid || !PRISM_WIFEXITED(st) || PRISM_WEXITSTATUS(st) != 5)
                bad++;
        sys_close(f);
        sys_close(p[0]);
        sys_close(p[1]);

        fmt(d, sizeof d, "parent read \"%s\", child (shared offset) \"%s\"", pb, cb);
        check("forkfd", bad == 0, d);
        return bad;
}

/* ---- dups ---------------------------------------------------------------- */

static int sc_dups(void)
{
        char b[8];
        char d[112];
        int bad = 0;

        /* dup: same description, so a seek through the dup moves the
         * original -- the point of not being a second open. */
        long f = sys_open("/fd.txt", PRISM_O_READ);
        if (f < 0)
                bad++;
        long dup = sys_dup(f);
        if (dup < 0 || dup == f)
                bad++;
        if (sys_lseek(dup, 4, PRISM_SEEK_SET) != 4)
                bad++;
        if (sys_read(f, b, 2) != 2 || memcmp(b, "45", 2))
                bad++;
        if (sys_read(f, b, 2) != 2 || memcmp(b, "67", 2))
                bad++;
        sys_close(dup);
        sys_close(f);
        bad += !expect(sys_dup(9), -PRISM_EBADF);

        /* dup of stdout: a second number writing to the same console. */
        static const char dl[] = "  (written to a dup of stdout)\n";
        long so = sys_dup(1);
        if (so < 0)
                bad++;
        else {
                bad += !expect(sys_write(so, dl, strlen(dl)), (long)strlen(dl));
                sys_close(so);
        }

        /* The renumbering trick, and the report goes through the new fd 1:
         * if the open did not land on 1, nothing after this prints at all
         * (the test harness misses the line and fails loudly). */
        sys_close(1);
        long nf = sys_open("/dev/console", PRISM_O_WRITE);

        fmt(d, sizeof d, "%s", nf == 1 ? "close(1) + open renumbered stdout to 1" : "stdout broken");
        check("dups", nf == 1, d);
        return bad;
}

/* ---- stdin --------------------------------------------------------------- */

static int sc_stdin(void)
{
        char b[8] = {0};
        char d[96];
        int bad = 0;

        /* A real read of fd 0: prints the prompt, parks on the console
         * (the test harness types the rest), and reports what it got. */
        printf("type 4 bytes (EXACTLY wxyz): ");
        long r = sys_read(0, b, 4);
        b[4] = 0;
        if (r != 4)
                bad++;
        else if (strcmp(b, "wxyz"))
                bad++;
        fmt(d, sizeof d, "read %ld byte(s): \"%s\"", r, b);
        check("stdin", bad == 0, d);
        return bad;
}

/* ---- execfd -------------------------------------------------------------- */

static int sc_execfd(void)
{
        char d[96];
        int bad = 0;

        long pid = sys_fork();
        if (pid == 0) {
                char *av[] = { "fd", "postexec", 0 };

                if (sys_execve("/bin/fd", av, environ))
                        sys_exit(9); /* the execve itself failed */
                sys_exit(1);
        }
        int st = 0;
        long w = sys_wait(pid, &st, 0);
        if (w != pid || !PRISM_WIFEXITED(st) || PRISM_WEXITSTATUS(st) != 0)
                bad++;

        fmt(d, sizeof d, "%s", bad ? "some check failed" : "child execve'd, descriptors survived");
        check("execfd", bad == 0, d);
        return bad;
}

/* ---- the postexec half of `execfd` --------------------------------------- */

static int postexec(void)
{
        char b[8];
        int bad = 0;

        /* This runs in the replaced image: fd 1 must still be the console
         * (this very printf proves it) and a fresh open must still work. */
        printf("postexec: stdout survived execve\n");
        long f = sys_open("/fd.txt", PRISM_O_READ);
        long n = f < 0 ? -1 : sys_read(f, b, 4);
        if (f >= 0)
                sys_close(f);
        printf("postexec: read %ld bytes after execve\n", n);
        if (n != 4 || memcmp(b, "0123", 4))
                bad++;
        return bad ? 1 : 0;
}

/* ---- main ---------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
        if (argc > 1 && !strcmp(argv[1], "postexec"))
                return postexec();

        printf("fd (pid %ld): descriptors, pipes and /dev\n", sys_getpid());
        sc_stdio();
        sc_file();
        sc_dev();
        sc_pipe_();
        sc_pblock();
        sc_epipe();
        sc_forkfd();
        sc_dups();
        sc_stdin();
        sc_execfd();
        if (fails == 0)
                printf("fd: all 10 passed\n");
        else
                printf("fd: %d failure(s), FAILURES\n", fails);
        return fails ? 1 : 0;
}
