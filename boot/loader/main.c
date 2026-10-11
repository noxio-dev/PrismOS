/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include "platform.h"
#include "fs.h"

/*
 * Common entry, called by the platform backend once the console, the heap
 * and the boot filesystem are up.  The Lua scripts do the real work; if
 * they cannot run, fall back to booting the default kernel directly.
 */
void loader_main(void)
{
    printf("PrismOS loader (%s)\n", plat_name());

    if (loader_run() == 0) {
        printf("loader.lua returned without booting\n");
    } else {
        const char *err = "unknown error";
        printf("\nLua loader failed, trying the default kernel...\n");
        if (kernel_load("/boot/kernel/kernel", &err) == 0)
            boot_kernel("", &err);
        printf("cannot boot: %s\n", err);
    }
    printf("Press any key to reboot.\n");
    console_getkey(-1);
    plat_reboot();
}
