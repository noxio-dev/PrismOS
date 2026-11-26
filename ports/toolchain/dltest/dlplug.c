/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * libdlplug.so -- the plug-in dltest dlopen()s (ports/toolchain/dltest).
 *
 * It is shaped like an Xorg module on purpose: a DSO nobody names at link
 * time, loaded by path, whose constructor runs at dlopen, that exports
 * functions, initialised data and a table of function pointers, and that
 * imports from BOTH sides -- the C library (strlen) and the host program
 * (dltest_host_hook, which dltest exports with --export-dynamic, the way
 * Xorg's executable exports the API its modules call).  One thread-local
 * variable covers the general-dynamic TLS of a dlopen()ed object.
 */

#include <stddef.h>
#include <string.h>

/* Defined by the host program (dltest.c), resolved at dlopen time. */
extern int dltest_host_hook(int x);

int dlplug_ctor_ran;       /* 0 until the constructor has run, then 1 */
int dlplug_answer = 42;    /* initialised data: a RELATIVE / absolute reloc */

static __thread int tls_val = 7;

__attribute__((constructor))
static void
dlplug_init(void)
{
	dlplug_ctor_ran = 1;
}

int
dlplug_add(int a, int b)
{
	return a + b;
}

/* An import from the C library (PLT / GOT slot resolved by the rtld). */
unsigned long
dlplug_strlen(const char *s)
{
	return strlen(s);
}

/* An import from the host executable. */
int
dlplug_call_host(int x)
{
	return dltest_host_hook(x);
}

/* A thread-local: 8 on the first call, 9 on the second. */
int
dlplug_tls_bump(void)
{
	return ++tls_val;
}

struct dlplug_ops {
	int (*add)(int, int);
	const char *name;
};

/* Function pointers and a string pointer in data: relocated at load. */
const struct dlplug_ops dlplug_ops = { dlplug_add, "dlplug" };
