/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/hello -- ported to mlibc (phase 3.2, step 4).  The original
 * (usr/hello/hello.c, on the mini libc of usr/lib) spoke the raw syscall
 * interface and the kernel's negative errnos; this one speaks the C
 * library: stdio for the output, unistd for getpid / execve, <sys/auxv.h>
 * for getauxval, errno for the failures -- and keeps only the three
 * PrismOS-only calls raw (prism_raw.h): the uptime clock, yield, and the
 * millisecond sleep.  Same scenarios, same checks, same output.
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
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <unistd.h>

#include "../prism_raw.h"

int main(int argc, char **argv, char **envp);

static const char banner[] = "hello from an ELF program";

/* Pointers to data: they need R_X86_64_RELATIVE relocations in a PIE. */
static const char *const modes[] = { "info", "fpu", "chain", "errors", "exit", "fault" };

static int fail(const char *what)
{
        printf("  FAIL: %s\n", what);
        return 1;
}

/*
 * A library call's answer, as the kernel's negative errno -- the shape the
 * error table below compares.  mlibc reports -1 + errno (the POSIX rule);
 * this puts the two conventions next to each other.
 */
static long ec(long r)
{
        return r >= 0 ? r : -errno;
}

/* ---- info ------------------------------------------------------------- */

static int info(int argc, char **argv, char **envp)
{
        int bad = 0;

        printf("%s (pid %ld)\n", banner, (long)getpid());
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
        printf("  uptime = %ld ms\n", prism_uptime_ms());

        /* The relocation of a pointer to a string in .rodata. */
        if (strcmp(modes[0], "info") != 0)
                bad += fail("relocated pointer table");
        if (getauxval(AT_PAGESZ) != 4096)
                bad += fail("AT_PAGESZ");
        if (getauxval(AT_ENTRY) == 0 || getauxval(AT_PHDR) == 0)
                bad += fail("AT_ENTRY / AT_PHDR");
        if (argc < 1 || strcmp(argv[0], (const char *)getauxval(AT_EXECFN)) != 0 ) {
                /* argv[0] is whatever the shell typed: not required to match */
        }

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
                        prism_yield();
                else
                        prism_sleep_ms(1);
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
        bad += expect("execve(/nonexistent)", ec(execve("/nonexistent", av, envp)), -ENOENT);
        bad += expect("execve(/boot/loader.conf)", ec(execve("/boot/loader.conf", av, envp)), -EACCES);
        bad += expect("execve(/bin/notelf)", ec(execve("/bin/notelf", av, envp)), -ENOEXEC);
        bad += expect("execve(/bin) directory", ec(execve("/bin", av, envp)), -EISDIR);
        bad += expect("execve(bad pointer)", ec(execve((const char *)0x1234, av, envp)), -EFAULT);
        bad += expect("execve(path too long)", ec(execve(longpath, av, envp)), -ENAMETOOLONG);
        {
                /* a deliberately invalid argv, routed through a volatile so
                 * gcc cannot reason about what it points at (clang, which the
                 * mini-libc original builds with, never looks) */
                char *const *volatile badav = (char *const *)(uintptr_t)0x10;
                bad += expect("execve(bad argv pointer)", ec(execve("/bin/hello", badav, envp)), -EFAULT);
        }
        /* argument list too long: 70 arguments */
        {
                char *many[71];

                for (int i = 0; i < 70; i++)
                        many[i] = "a";
                many[70] = 0;
                bad += expect("execve(70 arguments)", ec(execve("/bin/hello", many, envp)), -E2BIG);
        }
        /* a failed execve leaves the process alone */
        printf("  still alive after %d failures check: pid %ld\n", bad, (long)getpid());
        return bad;
}

/* ---- main ------------------------------------------------------------- */

int main(int argc, char **argv, char **envp)
{
        /* The mini libc's printf wrote each line straight through; the prompts
         * and the interleaved raw write()s of the test programs need the same
         * immediacy (and a forked child inheriting pending buffered output
         * would print it twice).  No buffering, then. */
        setvbuf(stdout, 0, _IONBF, 0);

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
                printf("hello chain: pid %ld calls execve()\n", (long)getpid());
                execve("/bin/hello", nargv, envp);

                printf("hello chain: execve failed: %s\n", strerror(errno)); /* only on failure */
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
