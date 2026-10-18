/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/hello -- the first real PrismOS program: a static PIE ELF written in
 * C, loaded from the ext4 root by the kernel (phase 2.4).  It exercises the
 * loader and the process layer:
 *
 *   hello [info]          banner, argv / envp / auxv, relocations, a float check
 *   hello fpu <tag> [n]   x87 + SSE arithmetic with yields in between; run two
 *                         at once ('exec hello fpu 1 -- hello fpu 2') to check
 *                         that the FPU state is kept per process
 *   hello chain [args...] execve("/bin/hello", ...) -- replaces this process
 *   hello errors          execve error paths (ENOENT, EACCES, ENOEXEC ...)
 *   hello exit <n>        exits with status n
 *   hello fault           dereferences NULL: the kernel must contain it
 */
#include "prism.h"

int main(int argc, char **argv, char **envp);

static const char banner[] = "hello from an ELF program";

/* Pointers to data: they need R_X86_64_RELATIVE relocations in a PIE. */
static const char *const modes[] = { "info", "fpu", "chain", "errors", "exit", "fault" };

static int fail(const char *what)
{
	printf("  FAIL: %s\n", what);
	return 1;
}

/* ---- info ------------------------------------------------------------- */

static int info(int argc, char **argv, char **envp)
{
	int bad = 0;

	printf("%s (pid %ld)\n", banner, sys_getpid());
	printf("  argc = %d\n", argc);
	for (int i = 0; i < argc; i++)
		printf("  argv[%d] = \"%s\"\n", i, argv[i]);
	for (char **e = envp; *e; e++)
		printf("  env: %s\n", *e);
	printf("  main() runs at %p (static PIE, relocated by the loader)\n", (void *)&main);
	printf("  AT_PAGESZ = %lu, AT_PHENT = %lu, AT_PHNUM = %lu\n",
	    getauxval(AT_PAGESZ), getauxval(AT_PHENT), getauxval(AT_PHNUM));
	printf("  AT_ENTRY  = %lx, AT_EXECFN = \"%s\"\n", getauxval(AT_ENTRY),
	    (const char *)getauxval(AT_EXECFN));
	printf("  uptime = %ld ms\n", sys_uptime_ms());

	/* The relocation of a pointer to a string in .rodata. */
	if (strcmp(modes[0], "info") != 0)
		bad += fail("relocated pointer table");
	if (getauxval(AT_PAGESZ) != 4096)
		bad += fail("AT_PAGESZ");
	if (getauxval(AT_ENTRY) == 0 || getauxval(AT_PHDR) == 0)
		bad += fail("AT_ENTRY / AT_PHDR");
	if (argc < 1 || strcmp(argv[0], (const char *)getauxval(AT_EXECFN)) != 0 )
		; /* argv[0] is whatever the shell typed: not required to match */

	/* Floating point, with exactly representable values. */
	volatile double d = 1.5;
	d = d * d + 0.25;                       /* 2.5 */
	long double ld = 3.0L;
	ld = ld / 4.0L + 0.25L;                 /* 1.0 */
	if ((long)(d * 4.0) != 10 || (long)(ld * 8.0L) != 8)
		bad += fail("float arithmetic");
	printf("  float check: %s\n", bad ? "FAILED" : "ok");
	return bad;
}

/* ---- fpu -------------------------------------------------------------- */

/* Sums i/2 + tag for i = 1..n, `rounds` times, yielding in between: with the
 * FPU state shared between processes the other copy would corrupt it. */
static int fpu(long tag, long rounds)
{
	const long n = 2000;
	double acc = 0.0;
	long double lacc = 0.0L;

	for (long r = 0; r < rounds; r++) {
		for (long i = 1; i <= n; i++) {
			acc += (double)i * 0.5 + (double)tag;
			lacc += (long double)i / 4.0L;
		}
		/* sqrt is an SSE instruction (sqrtsd): perfect squares are exact. */
		volatile double sq = (double)((r + 7) * (r + 7));

		if ((long)__builtin_sqrt(sq) != r + 7)
			return fail("sqrt");
		if (r & 1)
			sys_yield();
		else
			sys_sleep(1);
	}
	double want = (double)rounds * ((double)n * (double)(n + 1) / 4.0 + (double)tag * (double)n);
	long double lwant = (long double)rounds * ((long double)n * (long double)(n + 1) / 8.0L);

	if (acc != want)
		return fail("SSE accumulator corrupted");
	if (lacc != lwant)
		return fail("x87 accumulator corrupted");
	printf("  fpu %ld: %ld rounds ok (sse %ld, x87 %ld)\n", tag, rounds, (long)acc, (long)lacc);
	return 0;
}

/* ---- errors ----------------------------------------------------------- */

static int expect(const char *what, long got, long want)
{
	if (got == want) {
		printf("  ok   %-28s -> %ld\n", what, got);
		return 0;
	}
	printf("  FAIL %-28s -> %ld (wanted %ld)\n", what, got, want);
	return 1;
}

static int errors(char **envp)
{
	char *av[] = { "x", 0 };
	char longpath[400];
	int bad = 0;

	memset(longpath, 'a', sizeof longpath - 1);
	longpath[0] = '/';
	longpath[sizeof longpath - 1] = 0;
	bad += expect("execve(/nonexistent)", sys_execve("/nonexistent", av, envp), -PRISM_ENOENT);
	bad += expect("execve(/boot/loader.conf)", sys_execve("/boot/loader.conf", av, envp), -PRISM_EACCES);
	bad += expect("execve(/bin/notelf)", sys_execve("/bin/notelf", av, envp), -PRISM_ENOEXEC);
	bad += expect("execve(/bin) directory", sys_execve("/bin", av, envp), -PRISM_EISDIR);
	bad += expect("execve(bad pointer)", sys_execve((const char *)0x1234, av, envp), -PRISM_EFAULT);
	bad += expect("execve(path too long)", sys_execve(longpath, av, envp), -PRISM_ENAMETOOLONG);
	bad += expect("execve(bad argv pointer)", sys_execve("/bin/hello", (char **)0x10, envp), -PRISM_EFAULT);
	/* argument list too long: 70 arguments */
	{
		char *many[71];

		for (int i = 0; i < 70; i++)
			many[i] = "a";
		many[70] = 0;
		bad += expect("execve(70 arguments)", sys_execve("/bin/hello", many, envp), -PRISM_E2BIG);
	}
	/* a failed execve leaves the process alone */
	printf("  still alive after %d failures check: pid %ld\n", bad, sys_getpid());
	return bad;
}

/* ---- main ------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
	const char *mode = argc > 1 ? argv[1] : modes[0];

	if (strcmp(mode, "info") == 0)
		return info(argc, argv, envp);
	if (strcmp(mode, "fpu") == 0) {
		long tag = argc > 2 ? (long)strtoul(argv[2], 0, 10) : 1;
		long rounds = argc > 3 ? (long)strtoul(argv[3], 0, 10) : 40;

		return fpu(tag, rounds);
	}
	if (strcmp(mode, "chain") == 0) {
		/* Replace this process by a fresh copy of ourselves, passing the
		 * remaining arguments on: the new image must see them in argv. */
		char *nargv[16];
		int n = 0;

		nargv[n++] = "hello";
		nargv[n++] = "info";
		nargv[n++] = "(after execve)";
		for (int i = 2; i < argc && n < 15; i++)
			nargv[n++] = argv[i];
		nargv[n] = 0;
		printf("hello chain: pid %ld calls execve()\n", sys_getpid());
		long r = sys_execve("/bin/hello", nargv, envp);

		printf("hello chain: execve returned %ld\n", r); /* only on failure */
		return 1;
	}
	if (strcmp(mode, "errors") == 0)
		return errors(envp);
	if (strcmp(mode, "exit") == 0)
		return argc > 2 ? (int)strtoul(argv[2], 0, 10) : 0;
	if (strcmp(mode, "fault") == 0) {
		printf("hello fault: about to dereference NULL\n");
		*(volatile int *)0 = 1;
		return 0;
	}
	printf("usage: hello [info | fpu <tag> [rounds] | chain [args] | errors | exit <n> | fault]\n");
	return 2;
}
