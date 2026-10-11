/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/* Force-included when building Lua: route output to the loader console. */
#ifndef LUA_PRISM_H
#define LUA_PRISM_H
#include <stddef.h>
void console_write(const char *s, size_t n);
int printf(const char *, ...);
#define lua_writestring(s, l)        console_write((s), (l))
#define lua_writeline()              console_write("\n", 1)
#define lua_writestringerror(s, p)   printf((s), (p))
#endif
