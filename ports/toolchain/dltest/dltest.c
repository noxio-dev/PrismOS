/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /usr/bin/dltest -- dlopen / dlsym / dlclose / dlerror / dladdr from ring 3.
 *
 * Xorg loads its drivers and extensions as shared objects (docs/XORG.md,
 * 3.2): this is the proof that the PrismOS C library can.  mlibc's rtld
 * provides the machinery (__dlapi_open, __dlapi_resolve, ...) in ld.so and
 * dlfcn.cpp wraps it in libc.so; this program checks that it WORKS on the
 * kernel, with the plug-in shaped like a module (see dlplug.c).
 *
 * Linked dynamically with --export-dynamic (dltest_host_hook below must be
 * visible to the plug-in).  Usage:  dltest [path-to-libdlplug.so]
 * (default /usr/lib/libdlplug.so).  Prints "dltest: N/N checks passed";
 * exit status 0 only when every check passed.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE	/* dladdr / Dl_info live behind it (glibc and mlibc) */
#endif

#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

#define DEFAULT_PLUG "/usr/lib/libdlplug.so"

static int total, passed;

static void
check(int ok, const char *what)
{
	total++;
	if (ok)
		passed++;
	printf("dltest: %s %s\n", ok ? "ok  " : "FAIL", what);
}

/* Called from the plug-in: the host-side half of the module API. */
int
dltest_host_hook(int x)
{
	return x * 2;
}

struct dlplug_ops {
	int (*add)(int, int);
	const char *name;
};

int
main(int argc, char **argv)
{
	const char *path = argc > 1 ? argv[1] : DEFAULT_PLUG;
	const char *base = strrchr(path, '/');
	void *h, *h2, *sym;
	Dl_info info;
	int rc;

	base = base ? base + 1 : path;

	/* ---- open by path, RTLD_NOW ---- */
	h = dlopen(path, RTLD_NOW);
	check(h != NULL, "dlopen(path, RTLD_NOW)");
	if (h == NULL) {
		const char *e = dlerror();
		printf("dltest: dlerror: %s\n", e ? e : "(null)");
		printf("dltest: %d/%d checks passed\n", passed, total);
		return 1;
	}

	/* ---- the constructor ran during dlopen ---- */
	{
		int *ran = dlsym(h, "dlplug_ctor_ran");
		check(ran != NULL && *ran == 1, "constructor ran at dlopen");
	}

	/* ---- a function ---- */
	{
		int (*add)(int, int) = (int (*)(int, int))dlsym(h, "dlplug_add");
		check(add != NULL && add(40, 2) == 42, "dlsym function: add(40, 2) == 42");
	}

	/* ---- initialised data ---- */
	{
		int *answer = dlsym(h, "dlplug_answer");
		check(answer != NULL && *answer == 42, "dlsym data: answer == 42");
	}

	/* ---- plug-in -> C library import ---- */
	{
		unsigned long (*sl)(const char *) =
		    (unsigned long (*)(const char *))dlsym(h, "dlplug_strlen");
		check(sl != NULL && sl("prismos") == 7, "plug-in calls libc strlen");
	}

	/* ---- plug-in -> host executable import (the Xorg module model) ---- */
	{
		int (*ch)(int) = (int (*)(int))dlsym(h, "dlplug_call_host");
		check(ch != NULL && ch(5) == 10, "plug-in calls the host's exported symbol");
	}

	/* ---- function pointers / string pointer relocated in data ---- */
	{
		const struct dlplug_ops *ops = dlsym(h, "dlplug_ops");
		check(ops != NULL && ops->add != NULL && ops->add(20, 22) == 42
		    && ops->name != NULL && strcmp(ops->name, "dlplug") == 0,
		    "relocated function-pointer table");
	}

	/* ---- a missing symbol: NULL + an error string ---- */
	sym = dlsym(h, "dlplug_no_such_symbol");
	check(sym == NULL && dlerror() != NULL, "dlsym(missing) fails with dlerror");

	/* ---- a missing library: NULL + an error string ---- */
	check(dlopen("/usr/lib/libdlplug-missing.so", RTLD_NOW) == NULL
	    && dlerror() != NULL, "dlopen(missing) fails with dlerror");

	/* ---- the same library twice: same handle ---- */
	h2 = dlopen(path, RTLD_NOW);
	check(h2 == h, "dlopen twice returns the same handle");

	/* ---- by bare name, through the rtld's search path ---- */
	{
		void *hb = dlopen(base, RTLD_NOW);
		check(hb != NULL, "dlopen(bare name) via the search path");
		if (hb != NULL)
			dlclose(hb);
	}

	/* ---- RTLD_DEFAULT: the global scope (host exports + libc) ---- */
	check(dlsym(RTLD_DEFAULT, "dltest_host_hook") != NULL,
	    "dlsym(RTLD_DEFAULT) finds the host's exported symbol");
	check(dlsym(RTLD_DEFAULT, "printf") != NULL,
	    "dlsym(RTLD_DEFAULT) finds libc's printf");

	/* ---- dladdr on a plug-in address ---- */
	sym = dlsym(h, "dlplug_add");
	memset(&info, 0, sizeof info);
	check(sym != NULL && dladdr(sym, &info) != 0 && info.dli_fname != NULL
	    && strstr(info.dli_fname, "dlplug") != NULL,
	    "dladdr names the plug-in");

	/* ---- TLS of a dlopen()ed object (last of the risky ones) ---- */
	{
		int (*bump)(void) = (int (*)(void))dlsym(h, "dlplug_tls_bump");
		int a = bump ? bump() : -1;
		int b = bump ? bump() : -1;
		check(a == 8 && b == 9, "plug-in thread-local (general dynamic)");
	}

	/* ---- close (once per successful open: path, path, +bare above) ---- */
	rc = dlclose(h2);
	rc |= dlclose(h);
	check(rc == 0, "dlclose returns 0");

	printf("dltest: %d/%d checks passed\n", passed, total);
	return passed == total ? 0 : 1;
}
