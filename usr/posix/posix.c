/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/posix -- the phase 3.1 test program: the newlib syscall set.  Each
 * scenario prints one line and the program ends with a summary ("posix:
 * all 10 passed"), which is what `make test` looks for.
 *
 *   stat      stat / fstat with the x86-64 Linux struct stat: file types
 *             (regular, directory, char device with rdev, fifo), sizes,
 *             blksize, the ENOENT path, fstat through a fd, fstat of the
 *             console (a tty) and of a pipe end
 *   brk       the program break: query, grow two pages and write across
 *             them, shrink, brk below the base answers the current break
 *   mmap      anonymous mappings (zeroed, RW, page aligned), W^X refused,
 *             len 0 refused, MAP_FIXED replaces, munmap gives the pages
 *             back, a fork child writes its own COW copy while the parent
 *             keeps its bytes, and a MAP_PRIVATE file mapping of
 *             /bin/hello reads back the ELF magic (through pread: the
 *             fd's offset never moves)
 *   cwd       a per-process cwd: chdir into a fresh directory, getcwd
 *             round trip, a *relative* open that lands there (not in the
 *             shell's cwd), ".." walking back, ENOTDIR / ENOENT / ERANGE
 *   dup2      dup2 onto an exact number (and onto itself), the
 *             renumbering trick, EBADF paths, F_DUPFD at or past a
 *             number, F_GETFD / F_SETFD (FD_CLOEXEC), F_GETFL, and the
 *             O_CLOEXEC side of execve: a forked child execve()s this
 *             binary again ("posix postexec") and finds the marked fd
 *             closed while a plain dup survives
 *   nonblk    O_NONBLOCK through fcntl(F_SETFL): an empty pipe read
 *             answers -EAGAIN at once, a full pipe write ditto, and the
 *             EOF still works after the write end closes
 *   sigpipe   the POSIX rule: a forked child that writes to a readerless
 *             pipe dies by signal 13 (wait() says so), while the parent
 *             -- SIGPIPE ignored through signal(SIGPIPE, SIG_IGN) --
 *             gets a plain -EPIPE and carries on
 *   getdents  directory reading, linux_dirent64 records: /bin yields
 *             hello / fork / fd / posix as DT_REG plus "." and ".." as
 *             DT_DIR, /dev yields the three devices as DT_CHR, the loop
 *             ends at EOF, a regular file fd answers -ENOTDIR
 *   fsops     the name space: mkdir (EEXIST the second time), unlink,
 *             rmdir (ENOTEMPTY with a file inside, EISDIR through
 *             unlink), rename (old name gone, new one there), link
 *             (nlink 2, back to 1 after the unlink), the ENOENT paths
 *   uname     uname fills the six fields (PrismOS / x86_64 / 3.1) and
 *             ioctl(1, TCGETS) succeeds on the console while a regular
 *             file fd answers -ENOTTY (the isatty pair)
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

/* ---- stat / fstat ------------------------------------------------------- */

static int sc_stat(void)
{
        struct prism_stat st, st2;
        char d[96];
        int bad = 0;

        bad += !expect(sys_stat("/bin/hello", &st), 0);
        bad += !(PRISM_S_ISREG(st.st_mode) && st.st_size > 0 && st.st_blksize == 4096 && st.st_ino > 0);
        bad += !expect(sys_stat("/dev", &st), 0) || !PRISM_S_ISDIR(st.st_mode);
        bad += !expect(sys_stat("/dev/null", &st), 0);
        bad += !(PRISM_S_ISCHR(st.st_mode) && st.st_rdev == ((1 << 8) | 2));
        bad += !expect(sys_stat("/no/such/file", &st), -PRISM_ENOENT);

        long f = sys_open("/bin/hello", PRISM_O_READ);
        bad += !(f >= 0);
        bad += !expect(sys_fstat(f, &st), 0) || !PRISM_S_ISREG(st.st_mode);
        /* fstat must agree with stat on the same file. */
        bad += !expect(sys_stat("/bin/hello", &st2), 0);
        bad += !(st.st_size == st2.st_size && st.st_ino == st2.st_ino);
        sys_close(f);

        bad += !expect(sys_fstat(1, &st), 0); /* the console: a char device */
        bad += !(PRISM_S_ISCHR(st.st_mode) && st.st_rdev == ((1 << 8) | 1));
        int p[2];
        bad += !expect(sys_pipe(p), 0);
        bad += !expect(sys_fstat(p[0], &st), 0) || !PRISM_S_ISFIFO(st.st_mode);
        bad += !expect(sys_fstat(9, &st), -PRISM_EBADF);
        sys_close(p[0]);
        sys_close(p[1]);

        fmt(d, sizeof d, "types, rdev, fifo, blksize, %d checks", 15 - bad);
        check("stat", bad == 0, d);
        return bad;
}

/* ---- brk ---------------------------------------------------------------- */

static int sc_brk(void)
{
        char d[96];
        int bad = 0;

        unsigned long b0 = sys_brk(0);
        bad += !(b0 > 0 && (b0 & 0xfff) == 0); /* page aligned by construction */

        /* Grow by two pages and write across both. */
        unsigned long b1 = sys_brk(b0 + 8192);
        bad += !expect(b1, b0 + 8192);
        memset((void *)b0, 0xAB, 8192);
        bad += !(((unsigned char *)b0)[0] == 0xAB && ((unsigned char *)b0)[8191] == 0xAB);

        /* Shrink by one page: the first page stays usable. */
        unsigned long b2 = sys_brk(b0 + 4096);
        bad += !expect(b2, b0 + 4096);
        ((unsigned char *)b0)[100] = 7;
        bad += !expect(((unsigned char *)b0)[100], 7);

        /* A break below the base (or 0) is a query: the current one. */
        bad += !expect(sys_brk(0), b0 + 4096);
        bad += !expect(sys_brk(b0 - 16), b0 + 4096);

        /* Grow back, then leave the heap where it started (the pages
         * return to the PMM; usertest's page accounting stays honest). */
        bad += !expect(sys_brk(b1), b0 + 8192);
        bad += !expect(sys_brk(b0), b0);

        fmt(d, sizeof d, "grow 2 pages, write, shrink, query, %d checks", 9 - bad);
        check("brk", bad == 0, d);
        return bad;
}

/* ---- mmap / munmap ------------------------------------------------------ */

static int sc_mmap(void)
{
        char d[96];
        int bad = 0;
        unsigned char *m;
        long a;

        /* Anonymous: zeroed, writable, page aligned. */
        a = sys_mmap(0, 8192, PRISM_PROT_READ | PRISM_PROT_WRITE,
                     PRISM_MAP_PRIVATE | PRISM_MAP_ANONYMOUS, -1, 0);
        bad += !(a > 0 && (a & 0xfff) == 0);
        m = (unsigned char *)a;
        bad += !(m[0] == 0 && m[4095] == 0 && m[8191] == 0);
        m[17] = 0x5A;
        m[5000] = 0xA5;
        bad += !expect(m[17] | m[5000], 0x5A | 0xA5);

        /* W^X and the argument checks. */
        bad += !expect(sys_mmap(0, 4096, PRISM_PROT_READ | PRISM_PROT_WRITE | PRISM_PROT_EXEC,
                                PRISM_MAP_PRIVATE | PRISM_MAP_ANONYMOUS, -1, 0), -PRISM_EACCES);
        bad += !expect(sys_mmap(0, 0, PRISM_PROT_READ,
                                PRISM_MAP_PRIVATE | PRISM_MAP_ANONYMOUS, -1, 0), -PRISM_EINVAL);

        /* MAP_FIXED replaces what is there; munmap gives it back. */
        bad += !expect(sys_mmap(a, 4096, PRISM_PROT_READ | PRISM_PROT_WRITE,
                                PRISM_MAP_PRIVATE | PRISM_MAP_ANONYMOUS | PRISM_MAP_FIXED, -1, 0), a);
        ((unsigned char *)a)[3] = 9;
        bad += !expect(((unsigned char *)a)[3], 9);
        bad += !expect(sys_munmap(a, 8192), 0);
        bad += !expect(sys_munmap(a, 8192), 0); /* holes are fine */

        /* Copy-on-write across fork: the child's writes stay private. */
        a = sys_mmap(0, 4096, PRISM_PROT_READ | PRISM_PROT_WRITE,
                     PRISM_MAP_PRIVATE | PRISM_MAP_ANONYMOUS, -1, 0);
        bad += !(a > 0);
        ((unsigned char *)a)[0] = 'P';
        long pid = sys_fork();
        if (pid == 0) {
                ((unsigned char *)a)[0] = 'C'; /* the child's own copy */
                sys_exit(0);
        }
        int status = 0;
        bad += !expect(sys_wait(pid, &status, 0), pid);
        bad += !expect(status, 0);
        bad += !expect(((unsigned char *)a)[0], 'P'); /* parent untouched */
        sys_munmap(a, 4096);

        /* MAP_PRIVATE of a file: the ELF magic of /bin/hello, filled
         * through pread (the fd offset was 4 and must still be 4). */
        long f = sys_open("/bin/hello", PRISM_O_READ);
        bad += !(f >= 0);
        bad += !expect(sys_lseek(f, 4, PRISM_SEEK_SET), 4);
        a = sys_mmap(0, 8192, PRISM_PROT_READ, PRISM_MAP_PRIVATE, f, 0);
        bad += !(a > 0);
        bad += !(memcmp((void *)a, "\x7f" "ELF", 4) == 0);
        unsigned char b[4];
        bad += !expect(sys_read(f, b, 4), 4); /* bytes 4..8: offset intact */
        bad += !expect(memcmp(b, (unsigned char *)a + 4, 4), 0);
        bad += !expect(sys_munmap(a, 8192), 0);
        sys_close(f);

        fmt(d, sizeof d, "anon, W^X, FIXED, munmap, fork COW, file map, %d checks", 20 - bad);
        check("mmap", bad == 0, d);
        return bad;
}

/* ---- per-process cwd ---------------------------------------------------- */

static int sc_cwd(void)
{
        char d[96];
        char cwd[128];
        int bad = 0;

        bad += !expect(sys_mkdir("/posix-cwd", 0755), 0);
        bad += !expect(sys_chdir("/posix-cwd"), 0);
        bad += !expect(sys_getcwd(cwd, sizeof cwd), strlen("/posix-cwd") + 1);
        bad += !expect(strcmp(cwd, "/posix-cwd"), 0);

        /* A relative open lands in the *process's* cwd, not the shell's. */
        long f = sys_open("relative.txt", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_TRUNC);
        bad += !(f >= 0);
        bad += !expect(sys_write(f, "relative", 8), 8);
        sys_close(f);
        {
                struct prism_stat st;
                bad += !expect(sys_stat("/posix-cwd/relative.txt", &st), 0);
        }

        /* ".." walks back; the file is reachable through the new cwd. */
        bad += !expect(sys_chdir(".."), 0);
        bad += !expect(sys_getcwd(cwd, sizeof cwd), 2);
        bad += !expect(strcmp(cwd, "/"), 0);
        f = sys_open("posix-cwd/relative.txt", PRISM_O_READ);
        bad += !(f >= 0);
        sys_close(f);

        /* Error paths. */
        bad += !expect(sys_chdir("/bin/hello"), -PRISM_ENOTDIR);
        bad += !expect(sys_chdir("/no/such/dir"), -PRISM_ENOENT);
        bad += !expect(sys_getcwd(cwd, 1), -PRISM_ERANGE);

        /* Cleanup. */
        bad += !expect(sys_unlink("/posix-cwd/relative.txt"), 0);
        bad += !expect(sys_rmdir("/posix-cwd"), 0);

        fmt(d, sizeof d, "chdir, getcwd, relative open, .. , %d checks", 16 - bad);
        check("cwd", bad == 0, d);
        return bad;
}

/* ---- dup2 / fcntl / O_CLOEXEC ------------------------------------------- */

static int sc_dup2(void)
{
        char d[96];
        static const char line[] = "  (written through a dup2 of stdout)\n";
        int bad = 0;

        bad += !expect(sys_dup2(1, 7), 7);
        if (sys_write(7, line, strlen(line)) != (long)strlen(line))
                bad++;
        bad += !expect(sys_dup2(1, 7), 7); /* onto itself: a plain check */

        /* The renumbering trick, fd.c style. */
        long spare = sys_open("/posix-dup.txt", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_TRUNC);
        bad += !(spare >= 0);
        bad += !expect(sys_dup2(spare, 3), 3);
        sys_close(spare);
        sys_close(3);

        bad += !expect(sys_dup2(99, 5), -PRISM_EBADF);
        bad += !expect(sys_dup2(1, 99), -PRISM_EBADF);
        bad += !expect(sys_fcntl(99, PRISM_F_GETFD, 0), -PRISM_EBADF);

        /* F_DUPFD: the lowest free number at or past the argument. */
        long n = sys_fcntl(1, PRISM_F_DUPFD, 10);
        bad += !expect(n, 10);

        /* F_GETFD / F_SETFD: FD_CLOEXEC belongs to the number (shown on
         * fd 7, then unmarked -- the dup below must survive execve). */
        bad += !expect(sys_fcntl(7, PRISM_F_GETFD, 0), 0);
        bad += !expect(sys_fcntl(7, PRISM_F_SETFD, PRISM_FD_CLOEXEC), 0);
        bad += !expect(sys_fcntl(7, PRISM_F_GETFD, 0), PRISM_FD_CLOEXEC);
        bad += !expect(sys_fcntl(7, PRISM_F_SETFD, 0), 0);
        long fl = sys_fcntl(1, PRISM_F_GETFL, 0);
        bad += !(fl > 0 && (fl & PRISM_O_WRITE) != 0);

        /* O_CLOEXEC across execve: fd 20 is marked, the F_DUPFD (10)
         * is not.  A forked child execve()s this binary again and
         * reports from the other side. */
        long f = sys_open("/posix-clo.txt", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_TRUNC);
        bad += !(f >= 0);
        bad += !expect(sys_dup2(f, 20), 20);
        bad += !expect(sys_fcntl(20, PRISM_F_SETFD, PRISM_FD_CLOEXEC), 0);
        sys_close(f);
        long pid = sys_fork();
        if (pid == 0) {
                char *argv[] = { "posix", "postexec", 0 };
                char *env[] = { 0 };
                sys_execve("/bin/posix", argv, env);
                sys_exit(1); /* execve failed */
        }
        int status = 1;
        bad += !expect(sys_wait(pid, &status, 0), pid);
        bad += !expect(status, 0);
        sys_close(n);
        bad += !expect(sys_unlink("/posix-clo.txt"), 0);
        bad += !expect(sys_unlink("/posix-dup.txt"), 0);

        fmt(d, sizeof d, "dup2, F_DUPFD, FD_CLOEXEC + execve, %d checks", 21 - bad);
        check("dup2", bad == 0, d);
        return bad;
}

/* The child half of the execve check above: the marked number is gone,
 * the plain dup survived, stdout still writes (this line itself). */
static void postexec_mode(void)
{
        int bad = 0;

        bad += !expect(sys_fcntl(20, PRISM_F_GETFD, 0), -PRISM_EBADF);
        bad += !expect(sys_fcntl(10, PRISM_F_GETFD, 0), 0);
        printf("postexec: O_CLOEXEC fd closed, dupfd alive, stdout ok (%d checks)\n", 2 - bad);
        sys_close(10);
        sys_exit(bad == 0 ? 0 : 1);
}

/* ---- O_NONBLOCK ---------------------------------------------------------- */

static int sc_nonblk(void)
{
        char d[96];
        char b[16];
        static char big[4096];
        int p[2];
        int bad = 0;

        bad += !expect(sys_pipe(p), 0);
        bad += !expect(sys_fcntl(p[0], PRISM_F_SETFL, PRISM_O_NONBLOCK), 0);
        bad += !expect(sys_fcntl(p[0], PRISM_F_GETFL, 0) & PRISM_O_NONBLOCK, PRISM_O_NONBLOCK);

        /* Empty pipe, writers alive, O_NONBLOCK: -EAGAIN at once. */
        bad += !expect(sys_read(p[0], b, sizeof b), -PRISM_EAGAIN);

        /* Round trip still works. */
        bad += !expect(sys_write(p[1], "ab", 2), 2);
        bad += !expect(sys_read(p[0], b, sizeof b), 2);
        bad += !expect(memcmp(b, "ab", 2), 0);

        /* Fill the 16 KiB buffer (writes cap at 4096 per call). */
        bad += !expect(sys_fcntl(p[1], PRISM_F_SETFL, PRISM_O_NONBLOCK), 0);
        for (int i = 0; i < 4; i++)
                bad += !expect(sys_write(p[1], big, sizeof big), (long)sizeof big);
        bad += !expect(sys_write(p[1], big, sizeof big), -PRISM_EAGAIN);

        /* Close the write end and drain: the 16 KiB come back, then the
         * EOF itself. */
        sys_close(p[1]);
        long got = 0;
        long n;
        while ((n = sys_read(p[0], b, sizeof b)) > 0)
                got += n;
        bad += !expect(n, 0);
        bad += !expect(got, 16384);
        sys_close(p[0]);

        fmt(d, sizeof d, "EAGAIN read, 16 KiB fill + EAGAIN write, EOF, %d checks", 15 - bad);
        check("nonblk", bad == 0, d);
        return bad;
}

/* ---- SIGPIPE -------------------------------------------------------------- */

static int sc_sigpipe(void)
{
        char d[96];
        int p[2];
        int bad = 0;

        /* Default disposition: the writer dies by signal 13. */
        bad += !expect(sys_pipe(p), 0);
        long pid = sys_fork();
        if (pid == 0) {
                sys_close(p[0]);
                sys_write(p[1], "x", 1); /* readerless: SIGPIPE, no return */
                sys_exit(0);             /* not reached when the rule holds */
        }
        sys_close(p[0]); /* the parent's copy too: readers must hit 0 */
        int status = 0;
        bad += !expect(sys_wait(pid, &status, 0), pid);
        bad += !expect(PRISM_WIFSIGNALED(status), 1);
        bad += !expect(PRISM_WTERMSIG(status), PRISM_SIGPIPE);

        /* SIG_IGN: the parent gets -EPIPE and carries on. */
        bad += !expect(sys_signal(PRISM_SIGPIPE, PRISM_SIG_IGN), 0);
        sys_close(p[0]);
        bad += !expect(sys_write(p[1], "y", 1), -PRISM_EPIPE);
        sys_close(p[1]);
        bad += !expect(sys_signal(PRISM_SIGPIPE, PRISM_SIG_DFL), (long)PRISM_SIG_IGN);

        fmt(d, sizeof d, "child died by signal 13, SIG_IGN -> -EPIPE, %d checks", 7 - bad);
        check("sigpipe", bad == 0, d);
        return bad;
}

/* ---- getdents -------------------------------------------------------------- */

/* Count one name in a getdents stream (with its type). */
static int seen_type(char *buf, long len, const char *name, unsigned char want_type)
{
        long off = 0;

        while (off < len) {
                struct prism_dirent *e = (struct prism_dirent *)(buf + off);
                if (strcmp(e->d_name, name) == 0 && e->d_type == want_type)
                        return 1;
                off += e->d_reclen;
        }
        return 0;
}

static int sc_getdents(void)
{
        char d[96];
        static char buf[1024];
        int bad = 0;

        long dir = sys_open("/bin", PRISM_O_READ);
        bad += !(dir >= 0);
        int saw_hello = 0, saw_fork = 0, saw_fd = 0, saw_posix = 0, saw_dot = 0;
        long n;
        long total = 0;
        while ((n = sys_getdents(dir, buf, sizeof buf)) > 0) {
                saw_hello += seen_type(buf, n, "hello", PRISM_DT_REG);
                saw_fork += seen_type(buf, n, "fork", PRISM_DT_REG);
                saw_fd += seen_type(buf, n, "fd", PRISM_DT_REG);
                saw_posix += seen_type(buf, n, "posix", PRISM_DT_REG);
                saw_dot += seen_type(buf, n, ".", PRISM_DT_DIR);
                total += n;
        }
        bad += !expect(n, 0); /* the loop must end at EOF, not an error */
        bad += !(saw_hello && saw_fork && saw_fd && saw_posix && saw_dot);
        bad += !(total > 0);
        /* The cursor moved: rewind, and one more batch still works. */
        bad += !expect(sys_lseek(dir, 0, PRISM_SEEK_SET), 0);
        bad += !(sys_getdents(dir, buf, sizeof buf) > 0);
        sys_close(dir);

        /* /dev: the three devices, as character files. */
        dir = sys_open("/dev", PRISM_O_READ);
        bad += !(dir >= 0);
        int saw_console = 0, saw_null = 0, saw_zero = 0;
        while ((n = sys_getdents(dir, buf, sizeof buf)) > 0) {
                saw_console += seen_type(buf, n, "console", PRISM_DT_CHR);
                saw_null += seen_type(buf, n, "null", PRISM_DT_CHR);
                saw_zero += seen_type(buf, n, "zero", PRISM_DT_CHR);
        }
        bad += !expect(n, 0);
        bad += !(saw_console && saw_null && saw_zero);
        sys_close(dir);

        /* A regular file is not a directory. */
        long f = sys_open("/bin/hello", PRISM_O_READ);
        bad += !expect(sys_getdents(f, buf, sizeof buf), -PRISM_ENOTDIR);
        sys_close(f);
        bad += !expect(sys_getdents(1, buf, sizeof buf), -PRISM_ENOTDIR);

        fmt(d, sizeof d, "/bin DT_REG, /dev DT_CHR, rewind, ENOTDIR, %d checks", 11 - bad);
        check("getdents", bad == 0, d);
        return bad;
}

/* ---- the name space -------------------------------------------------------- */

static int sc_fsops(void)
{
        struct prism_stat st;
        char d[96];
        int bad = 0;

        /* mkdir, EEXIST, a file inside, ENOTEMPTY, rmdir. */
        bad += !expect(sys_mkdir("/posix-d", 0755), 0);
        bad += !expect(sys_mkdir("/posix-d", 0755), -PRISM_EEXIST);
        long f = sys_open("/posix-d/f.txt", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_TRUNC);
        bad += !(f >= 0);
        sys_close(f);
        bad += !expect(sys_rmdir("/posix-d"), -PRISM_ENOTEMPTY);
        bad += !expect(sys_unlink("/posix-d/f.txt"), 0);
        bad += !expect(sys_rmdir("/posix-d"), 0);
        bad += !expect(sys_stat("/posix-d", &st), -PRISM_ENOENT);

        /* rename: the old name goes, the new one is there. */
        f = sys_open("/posix-r1", PRISM_O_WRITE | PRISM_O_CREATE | PRISM_O_TRUNC);
        bad += !(f >= 0 && sys_write(f, "renamed", 7) == 7);
        sys_close(f);
        bad += !expect(sys_rename("/posix-r1", "/posix-r2"), 0);
        bad += !expect(sys_stat("/posix-r1", &st), -PRISM_ENOENT);
        bad += !expect(sys_stat("/posix-r2", &st), 0);
        bad += !PRISM_S_ISREG(st.st_mode);

        /* link: two names, one inode (nlink 2), unlink brings it back. */
        bad += !expect(sys_link("/posix-r2", "/posix-r3"), 0);
        bad += !expect(sys_stat("/posix-r2", &st), 0) || !expect(st.st_nlink, 2);
        bad += !expect(sys_stat("/posix-r3", &st), 0) || !expect(st.st_ino > 0, 1);
        bad += !expect(sys_unlink("/posix-r3"), 0);
        bad += !expect(sys_stat("/posix-r2", &st), 0) || !expect(st.st_nlink, 1);
        bad += !expect(sys_unlink("/posix-r2"), 0);

        /* Error paths. */
        bad += !expect(sys_mkdir("/posix-dd", 0755), 0);
        bad += !expect(sys_unlink("/posix-dd"), -PRISM_EISDIR);
        bad += !expect(sys_rmdir("/posix-dd"), 0);
        bad += !expect(sys_unlink("/no/such/file"), -PRISM_ENOENT);
        bad += !expect(sys_rmdir("/no/such/dir"), -PRISM_ENOENT);

        fmt(d, sizeof d, "mkdir/unlink/rmdir/rename/link + errno, %d checks", 23 - bad);
        check("fsops", bad == 0, d);
        return bad;
}

/* ---- uname / ioctl ------------------------------------------------------------ */

static int sc_uname(void)
{
        struct prism_utsname u;
        char d[96];
        int bad = 0;
        unsigned char termios[60];
        unsigned char ws[8];

        bad += !expect(sys_uname(&u), 0);
        bad += !expect(strcmp(u.sysname, "PrismOS"), 0);
        bad += !expect(strcmp(u.machine, "x86_64"), 0);
        bad += !expect(strcmp(u.release, "3.1"), 0);
        bad += !expect(u.nodename[0] != 0, 1);

        /* The isatty pair: the console answers, a file does not. */
        bad += !expect(sys_ioctl(1, PRISM_TCGETS, termios), 0);
        bad += !expect(sys_ioctl(1, PRISM_TIOCGWINSZ, ws), 0);
        unsigned cols = ws[2] | (ws[3] << 8);
        bad += !expect(cols, 80);
        long f = sys_open("/bin/hello", PRISM_O_READ);
        bad += !expect(sys_ioctl(f, PRISM_TCGETS, termios), -PRISM_ENOTTY);
        sys_close(f);

        fmt(d, sizeof d, "PrismOS x86_64 3.1, TCGETS/WINSZ, ENOTTY, %d checks", 9 - bad);
        check("uname", bad == 0, d);
        return bad;
}

/* ---- main ----------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
        (void)envp;

        if (argc == 2 && strcmp(argv[1], "postexec") == 0) {
                postexec_mode();
                return 0;
        }

        printf("posix (pid %ld): the newlib syscall set\n", sys_getpid());
        sc_stat();
        sc_brk();
        sc_mmap();
        sc_cwd();
        sc_dup2();
        sc_nonblk();
        sc_sigpipe();
        sc_getdents();
        sc_fsops();
        sc_uname();
        if (fails == 0)
                printf("posix: all 10 passed\n");
        else
                printf("posix: %d FAILURE(S)\n", fails);
        return fails == 0 ? 0 : 1;
}
