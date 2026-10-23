/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/hello rebuilt against mlibc (phase 3.2, step 3) -- the program that
 * proves the toolchain: compiled by the x86_64-prismos cross compiler against
 * the mlibc sysroot, linked as the static PIE the kernel's loader expects.
 *
 * It exercises, through mlibc only, everything the first two steps built:
 *   - write()     -- the raw syscall 1 path through the port's Write tag
 *   - printf      -- stdio on top of the kernel's descriptor table
 *   - getpid()    -- the GetPid tag
 *   - auxv        -- AT_PAGESZ / AT_ENTRY / AT_EXECFN, the System V initial
 *                    stack the kernel builds (phase 2.4)
 *   - a pointer table in .rodata -- the R_X86_64_RELATIVE relocations the
 *                    loader applies at USER_BASE
 *   - kill() to itself with a caught handler -- the SA_RESTORER contract:
 *                    mlibc's restorer must make syscall 12 (sigreturn) and the
 *                    kernel must restore the interrupted context
 *   - clock_gettime(CLOCK_MONOTONIC) -- syscall 43, the first step of the
 *                    phase, through mlibc's ClockGet tag
 *
 * Step 4 of the phase will move the real /bin/hello (fpu / chain / errors /
 * fault modes) from the mini libc onto mlibc; this one stays the toolchain's
 * smoke test.
 */
#include <stdio.h>
#include <unistd.h>
#include <string.h>
#include <signal.h>
#include <time.h>
#include <sys/auxv.h>

static const char banner[] = "hello from an mlibc program";

/* Pointers to .rodata data: a static PIE needs its R_X86_64_RELATIVE
 * relocations applied by the kernel loader before these compare equal. */
static const char *const table[] = { banner, "write", "printf", "signal", "clock" };

static volatile sig_atomic_t got_signal;

static void handler(int sig)
{
	(void)sig;
	got_signal = 1;
}

int main(int argc, char **argv, char **envp)
{
	int bad = 0;
	struct timespec ts;

	/* write(): the raw path -- mlibc's Write tag straight to syscall 1. */
	write(1, banner, sizeof banner - 1);
	write(1, "\n", 1);

	/* stdio through the descriptor table the kernel gave the process. */
	printf("%s (pid %ld)\n", banner, (long)getpid());
	printf("  argc = %d, argv[0] = \"%s\"\n", argc, argc > 0 ? argv[0] : "(null)");
	for (char **e = envp; *e; e++)
		printf("  env: %s\n", *e);

	/* The System V initial stack the kernel built (phase 2.4). */
	printf("  AT_PAGESZ = %ld, AT_ENTRY = %#lx, AT_EXECFN = \"%s\"\n",
	    getauxval(AT_PAGESZ), getauxval(AT_ENTRY),
	    (const char *)getauxval(AT_EXECFN));
	if (getauxval(AT_PAGESZ) != 4096)
		bad += 1;

	/* Relocations: the loader must have applied the table's entries. */
	for (unsigned i = 0; i < sizeof table / sizeof *table; i++)
		if (table[i] == 0)
			bad += 1;
	if (strcmp(table[0], banner) != 0 || strcmp(table[2], "printf") != 0)
		bad += 1;
	printf("  relocations: %s\n", bad ? "FAILED" : "ok");

	/* The SA_RESTORER contract: a caught signal whose handler returns
	 * through mlibc's restorer (syscall 12), the kernel restoring the
	 * interrupted context register for register. */
	struct sigaction sa;
	memset(&sa, 0, sizeof sa);
	sa.sa_handler = handler;
	sigemptyset(&sa.sa_mask);
	if (sigaction(SIGUSR1, &sa, 0) != 0) {
		printf("  sigaction: FAILED\n");
		bad += 1;
	} else if (kill(getpid(), SIGUSR1) != 0 || !got_signal) {
		printf("  signal round trip: FAILED\n");
		bad += 1;
	} else {
		printf("  signal round trip: ok (restorer reached sigreturn)\n");
	}

	/* clock_gettime (syscall 43) through mlibc's ClockGet tag. */
	if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0 || ts.tv_sec == 0) {
		printf("  clock_gettime: FAILED\n");
		bad += 1;
	} else {
		printf("  CLOCK_MONOTONIC = %lld.%09ld\n",
		    (long long)ts.tv_sec, ts.tv_nsec);
	}

	printf("hello: %s\n", bad ? "FAILED" : "all checks passed");
	return bad;
}
