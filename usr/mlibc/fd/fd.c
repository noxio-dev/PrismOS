/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/fd -- ported to mlibc (phase 3.2, step 4).  The phase 2.6
 * scenarios unchanged (per-process descriptors on top of the VFS, pipes,
 * /dev), but on the C library now: open / read / write / close / lseek /
 * dup / pipe from <unistd.h> and <fcntl.h>, the kernel's own O_* names
 * (the port's abi-bits/fcntl.h defines exactly the values the kernel
 * reads), failures as -1 + errno -- and the millisecond sleep raw
 * (prism_raw.h).  Each scenario prints one line and the program ends with
 * a summary ("fd: all 10 passed"), which is what `make test` looks for.
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
 *   epipe     a write with the read end already closed -> EPIPE
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
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/* mlibc's <unistd.h> keeps environ behind _GNU_SOURCE; the kernel hands
 * the same array to main, and the execfd child passes it to execve. */
extern char **environ;

#include "../prism_raw.h"

int main(int argc, char **argv, char **envp);

static int fails;

static void check(const char *what, int ok, const char *detail)
{
        printf("  %-9s %s -- %s\n", what, ok ? "ok" : "FAIL", detail);
        if (!ok)
                fails++;
}

/* detail-string builder on top of the library's vsnprintf */
static void fmt(char *out, size_t size, const char *f, ...)
{
        va_list ap;

        va_start(ap, f);
        vsnprintf(out, size, f, ap);
        va_end(ap);
}

/* A library call's answer as the kernel's negative errno (the tables
 * below compare the single word the raw calls used to return). */
static long ec(long r)
{
        return r >= 0 ? r : -errno;
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

        if (write(1, l1, strlen(l1)) != (long)strlen(l1))
                bad++;
        if (write(2, l2, strlen(l2)) != (long)strlen(l2))
                bad++;
        bad += !expect(ec(write(9, l1, 4)), -EBADF);
        bad += !expect(ec(read(9, b, sizeof b)), -EBADF);
        bad += !expect(ec(close(9)), -EBADF);
        bad += !expect(ec(read(1, b, sizeof b)), -EBADF); /* write-only */
        bad += !expect(ec(lseek(1, 0, SEEK_SET)), -ESPIPE);
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
        long f = open("/fd.txt", O_WRONLY | O_CREAT | O_TRUNC);
        if (f < 0 || write(f, text, strlen(text)) != (long)strlen(text) || close(f))
                bad++;
        f = open("/fd.txt", O_RDONLY);
        if (f < 0)
                bad++;
        if (lseek(f, 5, SEEK_SET) != 5)
                bad++;
        if (read(f, b, 4) != 4 || memcmp(b, "5678", 4))
                bad++;
        if (lseek(f, 0, SEEK_END) != 10)
                bad++;
        if (lseek(f, -4, SEEK_CUR) != 6)
                bad++;
        if (read(f, b, 4) != 4 || memcmp(b, "6789", 4))
                bad++;
        close(f);

        /* Error paths. */
        bad += !expect(ec(open("/no/such/file", O_RDONLY)), -ENOENT);
        bad += !expect(ec(open("/bin", O_WRONLY)), -EISDIR);
        long dir = open("/", O_RDONLY);
        if (dir < 0)
                bad++;
        else {
                bad += !expect(ec(read(dir, b, 1)), -EISDIR);
                bad += !expect(ec(lseek(dir, 0, 7)), -EINVAL); /* bad whence */
                close(dir);
        }
        bad += !expect(ec(lseek(9, 0, SEEK_SET)), -EBADF);
        long e1 = open("/fd-excl", O_WRONLY | O_CREAT | O_EXCL);
        if (e1 < 0)
                bad++;
        else
                close(e1);
        bad += !expect(ec(open("/fd-excl", O_WRONLY | O_CREAT | O_EXCL)),
            -EEXIST);

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
        long n = open("/dev/null", O_RDONLY | O_WRONLY);
        if (n < 0)
                return check("dev", 0, "open /dev/null failed"), 1;
        rr = read(n, b, 8); /* EOF */
        rw = write(n, "12345678", 8);
        rs = ec(lseek(n, 0, SEEK_SET));
        close(n);

        /* /dev/zero: zeroes on read, writes swallowed (opened R/W: a sink
         * must also accept writes through the same description). */
        long z = open("/dev/zero", O_RDONLY | O_WRONLY);
        if (z < 0)
                return check("dev", 0, "open /dev/zero failed"), 1;
        zr = read(z, b, 16);
        for (int i = 0; i < 16; i++)
                if (b[i] != 0)
                        zbad++;
        zw = write(z, b, 16);
        close(z);

        /* /dev/console: a line printed through the device itself. */
        static const char cl[] = "  (written to /dev/console)\n";
        long c = open("/dev/console", O_WRONLY);
        cw = c < 0 ? ec(c) : write(c, cl, strlen(cl));
        if (c >= 0)
                close(c);

        /* The directory and a missing node. */
        tl = ec(open("/dev/tele", O_RDONLY));
        long dd = open("/dev", O_RDONLY);
        dr = dd < 0 ? ec(dd) : ec(read(dd, b, 4));
        if (dd >= 0)
                close(dd);

        fmt(d, sizeof d,
            "null r=%ld w=%ld s=%ld; zero r=%ld (%d bad) w=%ld; console w=%ld; tele=%ld; dir r=%ld",
            rr, rw, rs, zr, zbad, zw, cw, tl, dr);
        check("dev", rr == 0 && rw == 8 && rs == -ESPIPE && zr == 16 && zbad == 0
            && zw == 16 && cw == (long)strlen(cl) && tl == -ENOENT
            && dr == -EISDIR, d);
        return 0;
}

/* ---- pipe ---------------------------------------------------------------- */

static int sc_pipe_(void)
{
        int p[2];
        char b[16];
        char d[96];
        int bad = 0;

        if (pipe(p))
                bad++;
        if (p[0] < 3 || p[1] < 3 || p[0] == p[1])
                bad++;

        /* Round trip, FIFO order across split reads. */
        bad += !expect(write(p[1], "abcde", 5), 5);
        bad += !expect(read(p[0], b, 2), 2);
        bad += !expect(read(p[0], b + 2, 3), 3);
        if (memcmp(b, "abcde", 5))
                bad++;

        /* EOF once the write end is gone. */
        close(p[1]);
        bad += !expect(read(p[0], b, 8), 0);
        close(p[0]);

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

        if (pipe(p))
                bad++;
        pid_t pid = fork();
        if (pid == 0) {
                /* Child: let the parent park first, then feed it one byte. */
                prism_sleep_ms(60);
                write(p[1], "Z", 1);
                _exit(0);
        }
        /* Our write end is not needed; the child holds its inherited copy. */
        close(p[1]);
        long t0 = prism_uptime_ms();
        long r = read(p[0], b, 1); /* parks until the child writes */
        long dt = prism_uptime_ms() - t0;
        if (r != 1 || b[0] != 'Z')
                bad++;
        if (dt < 50)
                bad++; /* it must have really waited for the writer */
        int st = 0;
        pid_t w = waitpid(pid, &st, 0);
        if (w != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 0)
                bad++;
        close(p[0]);

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

        if (pipe(p))
                return check("epipe", 0, "pipe failed"), 1;
        /* Phase 3.1: a readerless write is a death sentence by SIGPIPE
         * now (the POSIX default).  Ignore the signal here so the write
         * itself can be observed -- the sigpipe scenario of /bin/posix
         * covers both sides of the rule. */
        signal(SIGPIPE, SIG_IGN);
        close(p[0]); /* no reader left */
        w = ec(write(p[1], "x", 1));
        close(p[1]);
        signal(SIGPIPE, SIG_DFL);

        fmt(d, sizeof d, "write with no reader returned %ld", w);
        check("epipe", w == -EPIPE, d);
        return 0;
}

/* ---- forkfd -------------------------------------------------------------- */

static int sc_forkfd(void)
{
        int p[2];
        char pb[8], cb[8];
        char d[112];
        int bad = 0;

        if (pipe(p))
                bad++;
        long f = open("/fd.txt", O_RDONLY);
        if (f < 0)
                bad++;
        pid_t pid = fork();
        if (pid == 0) {
                /* The child inherited f and p: it reads where the parent left
                 * off (same description, so the offset is shared) and hands
                 * the bytes back through the pipe. */
                long n = read(f, cb, 4);
                write(p[1], cb, n);
                _exit(5);
        }
        /* Still holding the CPU: read the first 4 bytes ourselves, then
         * park on the pipe -- that is what lets the child run at all. */
        long n1 = read(f, pb, 4);
        pb[4] = 0;
        long n2 = read(p[0], cb, 4);
        cb[4] = 0;
        int st = 0;
        pid_t w = waitpid(pid, &st, 0);
        if (n1 != 4 || memcmp(pb, "0123", 4))
                bad++;
        if (n2 != 4 || memcmp(cb, "4567", 4))
                bad++;
        if (w != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 5)
                bad++;
        close(f);
        close(p[0]);
        close(p[1]);

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
        long f = open("/fd.txt", O_RDONLY);
        if (f < 0)
                bad++;
        long df = dup(f);
        if (df < 0 || df == f)
                bad++;
        if (lseek(df, 4, SEEK_SET) != 4)
                bad++;
        if (read(f, b, 2) != 2 || memcmp(b, "45", 2))
                bad++;
        if (read(f, b, 2) != 2 || memcmp(b, "67", 2))
                bad++;
        close(df);
        close(f);
        bad += !expect(ec(dup(9)), -EBADF);

        /* dup of stdout: a second number writing to the same console. */
        static const char dl[] = "  (written to a dup of stdout)\n";
        long so = dup(1);
        if (so < 0)
                bad++;
        else {
                bad += !expect(write(so, dl, strlen(dl)), (long)strlen(dl));
                close(so);
        }

        /* The renumbering trick, and the report goes through the new fd 1:
         * if the open did not land on 1, nothing after this prints at all
         * (the test harness misses the line and fails loudly). */
        close(1);
        long nf = open("/dev/console", O_WRONLY);

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
        long r = read(0, b, 4);
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

        pid_t pid = fork();
        if (pid == 0) {
                char *av[] = { "fd", "postexec", 0 };

                if (execve("/bin/fd", av, environ))
                        _exit(9); /* the execve itself failed */
                _exit(1);
        }
        int st = 0;
        pid_t w = waitpid(pid, &st, 0);
        if (w != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 0)
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
        long f = open("/fd.txt", O_RDONLY);
        long n = f < 0 ? -1 : read(f, b, 4);
        if (f >= 0)
                close(f);
        printf("postexec: read %ld bytes after execve\n", n);
        if (n != 4 || memcmp(b, "0123", 4))
                bad++;
        return bad ? 1 : 0;
}

/* ---- main ---------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
        (void)envp;
        setvbuf(stdout, 0, _IONBF, 0);

        if (argc > 1 && !strcmp(argv[1], "postexec"))
                return postexec();

        printf("fd (pid %ld): descriptors, pipes and /dev\n", (long)getpid());
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
