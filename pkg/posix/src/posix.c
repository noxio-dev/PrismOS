/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/posix -- ported to mlibc (phase 3.2, step 4).  The phase 3.1
 * scenarios unchanged, but through the C library now: stat / fstat from
 * <sys/stat.h>, mmap / munmap from <sys/mman.h>, the fcntl commands of
 * <fcntl.h> (the kernel's own O_* values), opendir / readdir for the
 * getdents walk, uname, tcgetattr / tcgetwinsize for the tty pair --
 * failures as -1 + errno, and two PrismOS-only calls raw (prism_raw.h):
 * the exact-return brk of its scenario, the millisecond clock of
 * nothing else here (this one needs no sleep or yield).  Each scenario
 * prints one line and the program ends with a summary ("posix: all 10
 * passed"), which is what `make test` looks for.
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
 *             /bin/hello reads back the ELF magic (the fd's offset never
 *             moves)
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
 *             answers EAGAIN at once, a full pipe write ditto, and the
 *             EOF still works after the write end closes
 *   sigpipe   the POSIX rule: a forked child that writes to a readerless
 *             pipe dies by signal 13 (wait() says so), while the parent
 *             -- SIGPIPE ignored through signal(SIGPIPE, SIG_IGN) --
 *             gets a plain EPIPE and carries on
 *   getdents  directory reading, now through opendir / readdir: /bin
 *             yields hello / fork / fd / posix as DT_REG plus "." and
 *             ".." as DT_DIR, /dev yields the three devices as DT_CHR,
 *             rewinddir rewinds, a regular file and the console answer
 *             ENOTDIR
 *   fsops     the name space: mkdir (EEXIST the second time), unlink,
 *             rmdir (ENOTEMPTY with a file inside, EISDIR through
 *             unlink), rename (old name gone, new one there), link
 *             (nlink 2, back to 1 after the unlink), the ENOENT paths
 *   uname     uname fills the six fields (PrismOS / x86_64 / 3.2 -- the
 *             release the kernel has carried since this phase started)
 *             and tcgetattr succeeds on the console while a regular file
 *             fd answers ENOTTY; tcgetwinsize reads 80 columns
 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#include "prism_raw.h"

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

/* A library call's answer as the kernel's negative errno -- the word the
 * tables below compare.  POSIX reports -1 + errno; this folds the two
 * back into the single value the raw calls used to return. */
static long ec(long r)
{
	return r >= 0 ? r : -errno;
}

/* mmap's answer in the same shape: the mapped address on success, the
 * negative errno behind MAP_FAILED otherwise. */
static long em(void *p)
{
	return p == MAP_FAILED ? -errno : (long)p;
}

static long expect(long got, long want)
{
	return got == want;
}

/* ---- stat / fstat ------------------------------------------------------- */

static int sc_stat(void)
{
	struct stat st, st2;
	char d[96];
	int bad = 0;

	bad += !expect(ec(stat("/bin/hello", &st)), 0);
	bad += !(S_ISREG(st.st_mode) && st.st_size > 0 && st.st_blksize == 4096 && st.st_ino > 0);
	bad += !expect(ec(stat("/dev", &st)), 0) || !S_ISDIR(st.st_mode);
	bad += !expect(ec(stat("/dev/null", &st)), 0);
	bad += !(S_ISCHR(st.st_mode) && st.st_rdev == ((1 << 8) | 2));
	bad += !expect(ec(stat("/no/such/file", &st)), -ENOENT);

	long f = open("/bin/hello", O_RDONLY);
	bad += !(f >= 0);
	bad += !expect(ec(fstat(f, &st)), 0) || !S_ISREG(st.st_mode);
	/* fstat must agree with stat on the same file. */
	bad += !expect(ec(stat("/bin/hello", &st2)), 0);
	bad += !(st.st_size == st2.st_size && st.st_ino == st2.st_ino);
	close(f);

	bad += !expect(ec(fstat(1, &st)), 0); /* the console: a char device */
	bad += !(S_ISCHR(st.st_mode) && st.st_rdev == ((1 << 8) | 1));
	int p[2];
	bad += !expect(ec(pipe(p)), 0);
	bad += !expect(ec(fstat(p[0], &st)), 0) || !S_ISFIFO(st.st_mode);
	bad += !expect(ec(fstat(9, &st)), -EBADF);
	close(p[0]);
	close(p[1]);

	fmt(d, sizeof d, "types, rdev, fifo, blksize, %d checks", 15 - bad);
	check("stat", bad == 0, d);
	return bad;
}

/* ---- brk ---------------------------------------------------------------- */

static int sc_brk(void)
{
	char d[96];
	int bad = 0;

	/* mlibc's allocator is mmap-based and its interface carries no brk,
	 * so this scenario speaks the kernel's own call (prism_raw.h). */
	unsigned long b0 = prism_brk(0);
	bad += !(b0 > 0 && (b0 & 0xfff) == 0); /* page aligned by construction */

	/* Grow by two pages and write across both. */
	unsigned long b1 = prism_brk(b0 + 8192);
	bad += !expect(b1, b0 + 8192);
	memset((void *)b0, 0xAB, 8192);
	bad += !(((unsigned char *)b0)[0] == 0xAB && ((unsigned char *)b0)[8191] == 0xAB);

	/* Shrink by one page: the first page stays usable. */
	unsigned long b2 = prism_brk(b0 + 4096);
	bad += !expect(b2, b0 + 4096);
	((unsigned char *)b0)[100] = 7;
	bad += !expect(((unsigned char *)b0)[100], 7);

	/* A break below the base (or 0) is a query: the current one. */
	bad += !expect(prism_brk(0), b0 + 4096);
	bad += !expect(prism_brk(b0 - 16), b0 + 4096);

	/* Grow back, then leave the heap where it started (the pages
	 * return to the PMM; usertest's page accounting stays honest). */
	bad += !expect(prism_brk(b1), b0 + 8192);
	bad += !expect(prism_brk(b0), b0);

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
	a = em(mmap(0, 8192, PROT_READ | PROT_WRITE,
	            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
	bad += !(a > 0 && (a & 0xfff) == 0);
	m = (unsigned char *)a;
	bad += !(m[0] == 0 && m[4095] == 0 && m[8191] == 0);
	m[17] = 0x5A;
	m[5000] = 0xA5;
	bad += !expect(m[17] | m[5000], 0x5A | 0xA5);

	/* W^X and the argument checks. */
	bad += !expect(em(mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
	                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)), -EACCES);
	bad += !expect(em(mmap(0, 0, PROT_READ,
	                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)), -EINVAL);

	/* MAP_FIXED replaces what is there; munmap gives it back. */
	bad += !expect(em(mmap((void *)a, 4096, PROT_READ | PROT_WRITE,
	                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0)), a);
	((unsigned char *)a)[3] = 9;
	bad += !expect(((unsigned char *)a)[3], 9);
	bad += !expect(ec(munmap((void *)a, 8192)), 0);
	bad += !expect(ec(munmap((void *)a, 8192)), 0); /* holes are fine */

	/* Copy-on-write across fork: the child's writes stay private. */
	a = em(mmap(0, 4096, PROT_READ | PROT_WRITE,
	            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
	bad += !(a > 0);
	((unsigned char *)a)[0] = 'P';
	pid_t pid = fork();
	if (pid == 0) {
		((unsigned char *)a)[0] = 'C'; /* the child's own copy */
		_exit(0);
	}
	int status = 0;
	bad += !expect(waitpid(pid, &status, 0), pid);
	bad += !expect(status, 0);
	bad += !expect(((unsigned char *)a)[0], 'P'); /* parent untouched */
	munmap((void *)a, 4096);

	/* MAP_PRIVATE of a file: the ELF magic of /bin/hello, filled
	 * through the offset the fd already sits at (it must not move). */
	long f = open("/bin/hello", O_RDONLY);
	bad += !(f >= 0);
	bad += !expect(lseek(f, 4, SEEK_SET), 4);
	a = em(mmap(0, 8192, PROT_READ, MAP_PRIVATE, f, 0));
	bad += !(a > 0);
	bad += !(memcmp((void *)a, "\x7f" "ELF", 4) == 0);
	unsigned char b[4];
	bad += !expect(read(f, b, 4), 4); /* bytes 4..8: offset intact */
	bad += !expect(memcmp(b, (unsigned char *)a + 4, 4), 0);
	bad += !expect(ec(munmap((void *)a, 8192)), 0);
	close(f);

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

	bad += !expect(ec(mkdir("/posix-cwd", 0755)), 0);
	bad += !expect(ec(chdir("/posix-cwd")), 0);
	bad += !(getcwd(cwd, sizeof cwd) == cwd && strcmp(cwd, "/posix-cwd") == 0);

	/* A relative open lands in the *process's* cwd, not the shell's. */
	long f = open("relative.txt", O_WRONLY | O_CREAT | O_TRUNC);
	bad += !(f >= 0);
	bad += !expect(write(f, "relative", 8), 8);
	close(f);
	{
		struct stat st;
		bad += !expect(ec(stat("/posix-cwd/relative.txt", &st)), 0);
	}

	/* ".." walks back; the file is reachable through the new cwd. */
	bad += !expect(ec(chdir("..")), 0);
	bad += !(getcwd(cwd, sizeof cwd) == cwd && strcmp(cwd, "/") == 0);
	f = open("posix-cwd/relative.txt", O_RDONLY);
	bad += !(f >= 0);
	close(f);

	/* Error paths. */
	bad += !expect(ec(chdir("/bin/hello")), -ENOTDIR);
	bad += !expect(ec(chdir("/no/such/dir")), -ENOENT);
	bad += !(getcwd(cwd, 1) == 0 && errno == ERANGE);

	/* Cleanup. */
	bad += !expect(ec(unlink("/posix-cwd/relative.txt")), 0);
	bad += !expect(ec(rmdir("/posix-cwd")), 0);

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

	bad += !expect(dup2(1, 7), 7);
	if (write(7, line, strlen(line)) != (long)strlen(line))
		bad++;
	bad += !expect(dup2(1, 7), 7); /* onto itself: a plain check */

	/* The renumbering trick, fd.c style. */
	long spare = open("/posix-dup.txt", O_WRONLY | O_CREAT | O_TRUNC);
	bad += !(spare >= 0);
	bad += !expect(dup2(spare, 3), 3);
	close(spare);
	close(3);

	bad += !expect(ec(dup2(99, 5)), -EBADF);
	bad += !expect(ec(dup2(1, 99)), -EBADF);
	bad += !expect(ec(fcntl(99, F_GETFD, 0)), -EBADF);

	/* F_DUPFD: the lowest free number at or past the argument. */
	long n = fcntl(1, F_DUPFD, 10);
	bad += !expect(n, 10);

	/* F_GETFD / F_SETFD: FD_CLOEXEC belongs to the number (shown on
	 * fd 7, then unmarked -- the dup below must survive execve). */
	bad += !expect(fcntl(7, F_GETFD, 0), 0);
	bad += !expect(ec(fcntl(7, F_SETFD, FD_CLOEXEC)), 0);
	bad += !expect(fcntl(7, F_GETFD, 0), FD_CLOEXEC);
	bad += !expect(ec(fcntl(7, F_SETFD, 0)), 0);
	long fl = fcntl(1, F_GETFL, 0);
	bad += !(fl > 0 && (fl & O_WRONLY) != 0);

	/* O_CLOEXEC across execve: fd 20 is marked, the F_DUPFD (10)
	 * is not.  A forked child execve()s this binary again and
	 * reports from the other side. */
	long f = open("/posix-clo.txt", O_WRONLY | O_CREAT | O_TRUNC);
	bad += !(f >= 0);
	bad += !expect(dup2(f, 20), 20);
	bad += !expect(ec(fcntl(20, F_SETFD, FD_CLOEXEC)), 0);
	close(f);
	pid_t pid = fork();
	if (pid == 0) {
		char *av[] = { "posix", "postexec", 0 };
		char *env[] = { 0 };
		execve("/bin/posix", av, env);
		_exit(1); /* execve failed */
	}
	int status = 1;
	bad += !expect(waitpid(pid, &status, 0), pid);
	bad += !expect(status, 0);
	close(n);
	bad += !expect(ec(unlink("/posix-clo.txt")), 0);
	bad += !expect(ec(unlink("/posix-dup.txt")), 0);

	fmt(d, sizeof d, "dup2, F_DUPFD, FD_CLOEXEC + execve, %d checks", 21 - bad);
	check("dup2", bad == 0, d);
	return bad;
}

/* The child half of the execve check above: the marked number is gone,
 * the plain dup survived, stdout still writes (this line itself). */
static void postexec_mode(void)
{
	int bad = 0;

	bad += !expect(ec(fcntl(20, F_GETFD, 0)), -EBADF);
	bad += !expect(fcntl(10, F_GETFD, 0), 0);
	printf("postexec: O_CLOEXEC fd closed, dupfd alive, stdout ok (%d checks)\n", 2 - bad);
	close(10);
	_exit(bad == 0 ? 0 : 1);
}

/* ---- O_NONBLOCK ---------------------------------------------------------- */

static int sc_nonblk(void)
{
	char d[96];
	char b[16];
	static char big[4096];
	int p[2];
	int bad = 0;

	bad += !expect(ec(pipe(p)), 0);
	bad += !expect(ec(fcntl(p[0], F_SETFL, O_NONBLOCK)), 0);
	bad += !expect(fcntl(p[0], F_GETFL, 0) & O_NONBLOCK, O_NONBLOCK);

	/* Empty pipe, writers alive, O_NONBLOCK: EAGAIN at once. */
	bad += !expect(ec(read(p[0], b, sizeof b)), -EAGAIN);

	/* Round trip still works. */
	bad += !expect(write(p[1], "ab", 2), 2);
	bad += !expect(read(p[0], b, sizeof b), 2);
	bad += !expect(memcmp(b, "ab", 2), 0);

	/* Fill the 16 KiB buffer (writes cap at 4096 per call). */
	bad += !expect(ec(fcntl(p[1], F_SETFL, O_NONBLOCK)), 0);
	for (int i = 0; i < 4; i++)
		bad += !expect(write(p[1], big, sizeof big), (long)sizeof big);
	bad += !expect(ec(write(p[1], big, sizeof big)), -EAGAIN);

	/* Close the write end and drain: the 16 KiB come back, then the
	 * EOF itself. */
	close(p[1]);
	long got = 0;
	long n;
	while ((n = read(p[0], b, sizeof b)) > 0)
		got += n;
	bad += !expect(n, 0);
	bad += !expect(got, 16384);
	close(p[0]);

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
	bad += !expect(ec(pipe(p)), 0);
	pid_t pid = fork();
	if (pid == 0) {
		close(p[0]);
		write(p[1], "x", 1); /* readerless: SIGPIPE, no return */
		_exit(0);             /* not reached when the rule holds */
	}
	close(p[0]); /* the parent's copy too: readers must hit 0 */
	int status = 0;
	bad += !expect(waitpid(pid, &status, 0), pid);
	bad += !expect(WIFSIGNALED(status), 1);
	bad += !expect(WTERMSIG(status), SIGPIPE);

	/* SIG_IGN: the parent gets EPIPE and carries on. */
	bad += !(signal(SIGPIPE, SIG_IGN) == SIG_DFL);
	close(p[0]);
	bad += !expect(ec(write(p[1], "y", 1)), -EPIPE);
	close(p[1]);
	bad += !(signal(SIGPIPE, SIG_DFL) == SIG_IGN);

	fmt(d, sizeof d, "child died by signal 13, SIG_IGN -> EPIPE, %d checks", 7 - bad);
	check("sigpipe", bad == 0, d);
	return bad;
}

/* ---- sessions and process groups (phase 3.3.4) --------------------------- */

static volatile sig_atomic_t usr1_hit;

static void usr1_catch(int sig)
{
	(void)sig;
	usr1_hit = 1;
}

/* The session ladder, in a process that is guaranteed *not* to be a
 * session leader: whoever exec'd this program may already be one (the
 * kernel shell's exec makes every process its own session), and a
 * forked child never is.  Returns the failure count as the exit code. */
static int session_ladder(void)
{
	int bad = 0;

	/* setsid (59): a session and a group of my own, both led by me. */
	errno = 0;
	pid_t sid = setsid();
	bad += !expect(sid, (long)getpid());
	bad += !expect(getpgrp(), sid);
	/* A session leader is pinned into its group (POSIX). */
	bad += !expect(ec(setpgid(0, 0)), -EPERM);
	/* And nobody setsids twice. */
	bad += !expect(ec(setsid()), -EPERM);

	/* A forked child makes a group of its own inside my session (60),
	 * the group kill spelling reaches it (kill(-pgrp)), and it answers
	 * through the signal that cuts its parked read short: the pipe's
	 * read end never answers, the SIGUSR1 through the group does. */
	int p[2], q[2];
	bad += !expect(ec(pipe(p)), 0);
	bad += !expect(ec(pipe(q)), 0);
	pid_t pid = fork();
	if (pid == 0) {
		if (setpgid(0, 0) < 0 || getpgrp() != getpid())
			_exit(9);
		close(p[0]);
		close(q[1]);
		signal(SIGUSR1, usr1_catch);
		if (write(p[1], "R", 1) != 1)
			_exit(8); /* the ready byte: my group exists */
		char c;
		ssize_t r = read(q[0], &c, 1); /* parks until the group kill */
		_exit(r < 0 && errno == EINTR && usr1_hit ? 0 : 7);
	}
	close(p[1]);
	close(q[0]);
	char c;
	bad += !expect(ec(read(p[0], &c, 1)), 1); /* wait for its group */
	/* q[1] stays open: the child's read must park, not see EOF. */
	bad += !expect(ec(kill(-pid, 0)), 0);      /* the group exists */
	bad += !expect(ec(kill(-pid, SIGUSR1)), 0);
	int status = 0;
	bad += !expect(ec(waitpid(pid, &status, 0)), pid);
	close(p[0]);
	close(q[1]);
	bad += !expect(WIFEXITED(status), 1);
	bad += !expect(WEXITSTATUS(status), 0);

	return bad;
}

static int sc_session(void)
{
	char d[96];
	int bad = 0;

	/* lstat (58): the usr-merge symlink answers as itself, not as the
	 * directory it points at -- its own type, its own length. */
	struct stat st;
	bad += !expect(ec(lstat("/bin", &st)), 0);
	bad += !expect(S_ISLNK(st.st_mode), 1);
	char target[64];
	ssize_t n = readlink("/bin", target, sizeof target - 1);
	bad += !expect(n > 0, 1);
	if (n > 0)
		target[n] = 0;
	bad += !expect(strcmp(target, "usr/bin"), 0);
	bad += !expect(ec(lstat("/no/such/link", &st)), -ENOENT);

	/* The shell parked this program's group in the foreground before
	 * exec'ing it (TIOCSPGRP): the terminal agrees.  Without a session
	 * (the kernel shell's exec -- nobody claimed the terminal) the
	 * foreground is unclaimed, 0, and the check does not apply. */
	int fg = 0;
	if (isatty(0)) {
		bad += !expect(ec(prism_ioctl(0, TIOCGPGRP, &fg)), 0);
		if (fg != 0)
			bad += !expect((pid_t)fg, getpgrp());
	}

	/* The session ladder in a forked worker, for whoever exec'd us. */
	pid_t worker = fork();
	if (worker == 0)
		_exit(session_ladder());
	int status = 0;
	bad += !expect(ec(waitpid(worker, &status, 0)), worker);
	bad += !expect(WIFEXITED(status), 1);
	bad += !expect(WEXITSTATUS(status), 0);

	fmt(d, sizeof d, "setsid + setpgid + kill(-pgrp) + lstat, %d checks", (fg ? 10 : 9) - bad);
	check("session", bad == 0, d);
	return bad;
}

/* ---- the directories (opendir / readdir) ---------------------------------- */

/* One name seen in a directory, with its type. */
static int seen_type(DIR *dir, const char *name, unsigned char want_type)
{
	struct dirent *e;

	rewinddir(dir);
	while ((e = readdir(dir)))
		if (strcmp(e->d_name, name) == 0)
			return e->d_type == want_type;
	return 0;
}

static int sc_getdents(void)
{
	char d[96];
	int bad = 0;

	/* /bin: the four programs as regular files, "." and ".." as
	 * directories, the walk ending on its own. */
	DIR *dir = opendir("/bin");
	bad += !(dir != 0);
	int saw_hello = 0, saw_fork = 0, saw_fd = 0, saw_posix = 0, saw_dot = 0;
	int entries = 0;
	struct dirent *e;
	if (dir) {
		while ((e = readdir(dir)))
			entries++;
		saw_hello = seen_type(dir, "hello", DT_REG);
		saw_fork = seen_type(dir, "fork", DT_REG);
		saw_fd = seen_type(dir, "fd", DT_REG);
		saw_posix = seen_type(dir, "posix", DT_REG);
		saw_dot = seen_type(dir, ".", DT_DIR);
		/* The cursor moved: rewind, and one more walk still works. */
		rewinddir(dir);
		bad += !(readdir(dir) != 0);
		closedir(dir);
	}
	bad += !(entries > 0);
	bad += !(saw_hello && saw_fork && saw_fd && saw_posix && saw_dot);

	/* /dev: the three devices, as character files. */
	dir = opendir("/dev");
	bad += !(dir != 0);
	if (dir) {
		int saw_console = seen_type(dir, "console", DT_CHR);
		int saw_null = seen_type(dir, "null", DT_CHR);
		int saw_zero = seen_type(dir, "zero", DT_CHR);
		closedir(dir);
		bad += !(saw_console && saw_null && saw_zero);
	}

	/* A regular file is not a directory; neither is the console. */
	bad += !(opendir("/bin/hello") == 0 && errno == ENOTDIR);
	bad += !(opendir("/dev/console") == 0 && errno == ENOTDIR);

	fmt(d, sizeof d, "/bin DT_REG, /dev DT_CHR, rewind, ENOTDIR, %d checks", 11 - bad);
	check("getdents", bad == 0, d);
	return bad;
}

/* ---- the name space -------------------------------------------------------- */

static int sc_fsops(void)
{
	struct stat st;
	char d[96];
	int bad = 0;

	/* mkdir, EEXIST, a file inside, ENOTEMPTY, rmdir. */
	bad += !expect(ec(mkdir("/posix-d", 0755)), 0);
	bad += !expect(ec(mkdir("/posix-d", 0755)), -EEXIST);
	long f = open("/posix-d/f.txt", O_WRONLY | O_CREAT | O_TRUNC);
	bad += !(f >= 0);
	close(f);
	bad += !expect(ec(rmdir("/posix-d")), -ENOTEMPTY);
	bad += !expect(ec(unlink("/posix-d/f.txt")), 0);
	bad += !expect(ec(rmdir("/posix-d")), 0);
	bad += !expect(ec(stat("/posix-d", &st)), -ENOENT);

	/* rename: the old name goes, the new one is there. */
	f = open("/posix-r1", O_WRONLY | O_CREAT | O_TRUNC);
	bad += !(f >= 0 && write(f, "renamed", 7) == 7);
	close(f);
	bad += !expect(ec(rename("/posix-r1", "/posix-r2")), 0);
	bad += !expect(ec(stat("/posix-r1", &st)), -ENOENT);
	bad += !expect(ec(stat("/posix-r2", &st)), 0);
	bad += !S_ISREG(st.st_mode);

	/* link: two names, one inode (nlink 2), unlink brings it back. */
	bad += !expect(ec(link("/posix-r2", "/posix-r3")), 0);
	bad += !expect(ec(stat("/posix-r2", &st)), 0) || !expect(st.st_nlink, 2);
	bad += !expect(ec(stat("/posix-r3", &st)), 0) || !expect(st.st_ino > 0, 1);
	bad += !expect(ec(unlink("/posix-r3")), 0);
	bad += !expect(ec(stat("/posix-r2", &st)), 0) || !expect(st.st_nlink, 1);
	bad += !expect(ec(unlink("/posix-r2")), 0);

	/* Error paths. */
	bad += !expect(ec(mkdir("/posix-dd", 0755)), 0);
	bad += !expect(ec(unlink("/posix-dd")), -EISDIR);
	bad += !expect(ec(rmdir("/posix-dd")), 0);
	bad += !expect(ec(unlink("/no/such/file")), -ENOENT);
	bad += !expect(ec(rmdir("/no/such/dir")), -ENOENT);

	fmt(d, sizeof d, "mkdir/unlink/rmdir/rename/link + errno, %d checks", 23 - bad);
	check("fsops", bad == 0, d);
	return bad;
}

/* ---- uname / termios ------------------------------------------------------ */

static int sc_uname(void)
{
	struct utsname u;
	char d[96];
	int bad = 0;
	struct termios t;
	struct winsize ws;

	bad += !expect(ec(uname(&u)), 0);
	bad += !expect(strcmp(u.sysname, "PrismOS"), 0);
	bad += !expect(strcmp(u.machine, "x86_64"), 0);
	/* The release moved to "3.2" with this phase (the kernel's own
	 * sys_uname answers it); the mini-libc program checked "3.1". */
	bad += !expect(strcmp(u.release, "3.2"), 0);
	bad += !expect(u.nodename[0] != 0, 1);

	/* The tty pair: the console answers, a file does not -- and the
	 * 80 columns come back through tcgetwinsize. */
	bad += !expect(ec(tcgetattr(1, &t)), 0);
	bad += !expect(ec(tcgetwinsize(1, &ws)), 0);
	/* The real geometry, whatever the console is: 80 columns in VGA
	 * text mode, 160 on the QEMU framebuffer's text grid. */
	bad += !expect(ws.ws_col != 0, 1);
	long f = open("/bin/hello", O_RDONLY);
	bad += !expect(ec(tcgetattr(f, &t)), -ENOTTY);
	close(f);

	fmt(d, sizeof d, "PrismOS x86_64 3.2, TCGETS/WINSZ, ENOTTY, %d checks", 9 - bad);
	check("uname", bad == 0, d);
	return bad;
}

/* ---- main ----------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
	(void)envp;
	setvbuf(stdout, 0, _IONBF, 0);

	if (argc == 2 && strcmp(argv[1], "postexec") == 0) {
		postexec_mode();
		return 0;
	}

	printf("posix (pid %ld): the C library's syscall set\n", (long)getpid());
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
	sc_session();
	if (fails == 0)
		printf("posix: all 11 passed\n");
	else
		printf("posix: %d FAILURE(S)\n", fails);
	return fails == 0 ? 0 : 1;
}
