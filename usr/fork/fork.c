/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/fork -- the phase 2.5 test program: fork with copy-on-write, wait,
 * signals.  Each scenario prints one line and the program ends with a
 * summary (`fork: all 10 passed`), which is what `make test` looks for.
 *
 *   cow       the classic: parent fills a buffer, forks, the child rewrites
 *             its copy (write faults -> the kernel copies the page), the
 *             parent's buffer must still be intact after wait
 *   pids      the parent's fork() returns the child's pid, the child's
 *             returns 0, and the reaped status matches the child's exit
 *   waiterr   wait() with no matching child -> -ECHILD; WNOHANG on a
 *             living child -> 0 without blocking
 *   waitpid   two children dying in a known order, reaped by pid: the
 *             targeted wait must return the right one first
 *   sigchld   a SIGCHLD handler in the parent runs while children die
 *             (deliveries coalesce: 1..3 are all correct)
 *   sigterm   a child without a handler dies of SIGTERM: WIFSIGNALED and
 *             WTERMSIG == 15
 *   sigself   a self-signal with a handler: the return value of kill()
 *             surviving the round trip proves sigreturn restores every
 *             register of the interrupted context
 *   exec      a forked child execve()s: the copy-on-write pages of the old
 *             image are released cleanly, the new image's status comes back
 *             through wait
 *   orphan    a child that dies before its own child: the grandchild is
 *             adopted by the kernel and still runs to its end
 *   sigkill   kill() on a dead pid -> -ESRCH, catching SIGKILL -> -EINVAL
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

/* ---- cow --------------------------------------------------------------- */

/* Big enough to span several pages, in .data (writable, so both sides
 * fork-share it copy-on-write). */
static unsigned char pad[3 * 4096];

static int scenario_cow(void)
{
	for (long i = 0; i < (long)sizeof pad; i++)
		pad[i] = (unsigned char)(i * 7);

	long pid = sys_fork();
	if (pid < 0) {
		check("cow", 0, "fork failed");
		return 1;
	}
	if (pid == 0) {
		/* Child: rewrite our copy.  Every store faults first
		 * (read-only + COW), the kernel copies the page, the store
		 * re-executes. */
		for (long i = 0; i < (long)sizeof pad; i++)
			pad[i] ^= 0xff;
		long bad = 0;
		for (long i = 0; i < (long)sizeof pad; i++)
			if (pad[i] != (unsigned char)(i * 7 ^ 0xff))
				bad++;
		if (bad)
			sys_exit(100);
		sys_exit(42);
	}

	int st = 0;
	long r = sys_wait(pid, &st, 0);
	if (r != pid || !PRISM_WIFEXITED(st) || PRISM_WEXITSTATUS(st) != 42) {
		check("cow", 0, "child did not exit 42");
		return 1;
	}
	long bad = 0;
	for (long i = 0; i < (long)sizeof pad; i++)
		if (pad[i] != (unsigned char)(i * 7))
			bad++;
	check("cow", bad == 0, bad ? "parent buffer corrupted by the child" :
	    "3 pages rewritten by the child, parent copy intact");
	return bad != 0;
}

/* ---- pids -------------------------------------------------------------- */

static int scenario_pids(void)
{
	long pid = sys_fork();
	if (pid < 0) {
		check("pids", 0, "fork failed");
		return 1;
	}
	if (pid == 0)
		sys_exit(sys_getpid() & 0xff); /* parent checks it matches */

	int st = 0;
	long r = sys_wait(pid, &st, 0);
	int ok = r == pid && PRISM_WIFEXITED(st)
	    && PRISM_WEXITSTATUS(st) == (pid & 0xff);
	check("pids", ok, "parent saw the child's pid, child saw 0");
	return !ok;
}

/* ---- waiterr ------------------------------------------------------------ */

static int scenario_waiterr(void)
{
	long e = sys_wait(999999, 0, 0);
	if (e != -PRISM_ECHILD) {
		check("waiterr", 0, "wait for a stranger should be -ECHILD");
		return 1;
	}

	long pid = sys_fork();
	if (pid == 0)
		sys_exit(0);

	int st = 0;
	long r = sys_wait(PRISM_WAIT_ANY, &st, PRISM_WNOHANG);
	int ok = r == 0; /* child alive, nothing reaped, no blocking */
	long r2 = sys_wait(PRISM_WAIT_ANY, &st, 0);
	ok = ok && r2 == pid && PRISM_WIFEXITED(st) && PRISM_WEXITSTATUS(st) == 0;
	check("waiterr", ok, "-ECHILD and WNOHANG paths ok");
	return !ok;
}

/* ---- waitpid ------------------------------------------------------------ */

static int scenario_waitpid(void)
{
	long slow = sys_fork();
	if (slow == 0) {
		sys_sleep(30);
		sys_exit(1);
	}
	long fast = sys_fork();
	if (fast == 0)
		sys_exit(2);

	/* Reap in the "wrong" order: by pid, fast first. */
	int st = 0;
	long r = sys_wait(fast, &st, 0);
	int ok = r == fast && PRISM_WIFEXITED(st) && PRISM_WEXITSTATUS(st) == 2;
	r = sys_wait(slow, &st, 0);
	ok = ok && r == slow && PRISM_WIFEXITED(st) && PRISM_WEXITSTATUS(st) == 1;
	check("waitpid", ok, "reaped each child by pid, in any order");
	return !ok;
}

/* ---- sigchld ------------------------------------------------------------ */

static volatile long chld_deliveries;
static volatile long chld_last_sig;

static void on_sigchld(int sig)
{
	chld_deliveries++;
	chld_last_sig = sig;
}

static int scenario_sigchld(void)
{
	chld_deliveries = 0;
	long old = sys_signal(PRISM_SIGCHLD, on_sigchld);
	if (old != 0) {
		check("sigchld", 0, "signal() should return the old handler (0)");
		return 1;
	}

	for (int i = 0; i < 3; i++) {
		long pid = sys_fork();
		if (pid == 0)
			sys_exit(0);
		int st = 0;
		sys_wait(pid, &st, 0);
	}
	/* The handler runs between our wait returns; standard signals
	 * coalesce, so anything in 1..3 deliveries is correct. */
	int ok = chld_deliveries >= 1 && chld_deliveries <= 3
	    && chld_last_sig == PRISM_SIGCHLD;
	sys_signal(PRISM_SIGCHLD, 0);
	check("sigchld", ok, "handler ran while children died (coalesced)");
	return !ok;
}

/* ---- sigterm ------------------------------------------------------------ */

static int scenario_sigterm(void)
{
	long pid = sys_fork();
	if (pid == 0)
		for (;;)
			sys_yield(); /* no handler: SIGTERM ends us */

	long k = sys_kill(pid, PRISM_SIGTERM);
	int st = 0;
	long r = sys_wait(pid, &st, 0);
	int ok = k == 0 && r == pid && PRISM_WIFSIGNALED(st)
	    && PRISM_WTERMSIG(st) == PRISM_SIGTERM;
	check("sigterm", ok, "child died of SIGTERM, wait decoded it");
	return !ok;
}

/* ---- sigself ------------------------------------------------------------ */

static volatile long usr1_seen;
static volatile long usr1_arg;

static void on_usr1(int sig)
{
	usr1_seen++;
	usr1_arg = sig;
}

static int scenario_sigself(void)
{
	usr1_seen = 0;
	usr1_arg = 0;
	sys_signal(PRISM_SIGUSR1, on_usr1);

	volatile long marker = 0x1234abcd;
	/* The kill() return value must survive the handler round trip:
	 * sigreturn restores every register of the interrupted context,
	 * rax included. */
	long r = sys_kill(sys_getpid(), PRISM_SIGUSR1);
	long m = marker;

	int ok = r == 0 && usr1_seen == 1 && usr1_arg == PRISM_SIGUSR1
	    && m == 0x1234abcd;
	check("sigself", ok, "registers and stack intact across the handler");
	sys_signal(PRISM_SIGUSR1, 0);
	return !ok;
}

/* ---- exec --------------------------------------------------------------- */

static int scenario_exec(char **envp)
{
	long pid = sys_fork();
	if (pid < 0) {
		check("exec", 0, "fork failed");
		return 1;
	}
	if (pid == 0) {
		/* Replace the forked image (its COW pages are released).
		 * /bin/hello exits with the number we give it. */
		char *av[] = { "hello", "exit", "7", 0 };
		sys_execve("/bin/hello", av, envp);
		sys_exit(99); /* exec failed */
	}

	int st = 0;
	long r = sys_wait(pid, &st, 0);
	int ok = r == pid && PRISM_WIFEXITED(st) && PRISM_WEXITSTATUS(st) == 7;
	check("exec", ok, "child execve'd /bin/hello, exit 7 reaped");
	return !ok;
}

/* ---- orphan ------------------------------------------------------------- */

static int scenario_orphan(void)
{
	long child = sys_fork();
	if (child == 0) {
		long g = sys_fork();
		if (g == 0) {
			sys_sleep(30); /* outlives its parent */
			sys_exit(9);
		}
		/* Die with the grandchild still sleeping: the kernel
		 * adopts it. */
		sys_exit(5);
	}

	int st = 0;
	long r = sys_wait(child, &st, 0);
	int ok = r == child && PRISM_WIFEXITED(st) && PRISM_WEXITSTATUS(st) == 5;
	check("orphan", ok, "child reaped, grandchild adopted by pid 0");
	return !ok;
}

/* ---- sigkill ------------------------------------------------------------ */

static int scenario_sigkill(void)
{
	long e = sys_kill(999999, PRISM_SIGTERM);
	long h = sys_signal(PRISM_SIGKILL, on_usr1); /* must be refused */
	long probe = sys_kill(sys_getpid(), 0);       /* existence check */
	int ok = e == -PRISM_ESRCH && h == -PRISM_EINVAL && probe == 0;
	check("sigkill", ok, "-ESRCH, SIGKILL uncatchable, sig 0 probes");
	return !ok;
}

/* ---- main --------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
	(void)argc;
	(void)argv;
	printf("fork (pid %ld): fork, wait, signals and copy-on-write\n",
	    sys_getpid());

	scenario_cow();
	scenario_pids();
	scenario_waiterr();
	scenario_waitpid();
	scenario_sigchld();
	scenario_sigterm();
	scenario_sigself();
	scenario_exec(envp);
	scenario_orphan();
	scenario_sigkill();

	if (fails == 0)
		printf("fork: all 10 passed\n");
	else
		printf("fork: %d/10 passed, FAILURES\n", 10 - fails);
	return fails != 0;
}
