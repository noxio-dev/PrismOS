/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/*
 * Lua integration: opens a restricted set of standard libraries, exposes
 * the "loader" module (console, environment, filesystem, kernel loading,
 * boot) and runs /boot/lua/loader.lua, which drives the whole boot.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lua.h"
#include "lauxlib.h"
#include "lualib.h"
#include "platform.h"
#include "fs.h"

#define LOADER_VERSION "PrismOS loader 0.1.0"
#define LOADER_SCRIPT  "/boot/lua/loader.lua"

/* ---- console ------------------------------------------------------- */

static int l_write(lua_State *L)
{
    size_t n;
    const char *s = luaL_checklstring(L, 1, &n);
    console_write(s, n);
    return 0;
}

static int l_size(lua_State *L)
{
    int rows, cols;
    console_size(&rows, &cols);
    lua_pushinteger(L, rows);
    lua_pushinteger(L, cols);
    return 2;
}

static int l_getkey(lua_State *L)
{
    char buf[8];
    int k = console_getkey((int)luaL_optinteger(L, 1, -1));
    if (k < 0) lua_pushnil(L);
    else lua_pushstring(L, key_name(k, buf));
    return 1;
}

static int l_time(lua_State *L) { lua_pushinteger(L, (lua_Integer)plat_ms()); return 1; }
static int l_delay(lua_State *L) { plat_sleep((unsigned)luaL_checkinteger(L, 1)); return 0; }
static int l_reboot(lua_State *L) { (void)L; plat_reboot(); }
static int l_shutdown(lua_State *L) { (void)L; plat_shutdown(); }

/* ---- environment ---------------------------------------------------- */

static int l_getenv(lua_State *L)
{
    const char *v = env_get(luaL_checkstring(L, 1));
    if (v) lua_pushstring(L, v); else lua_pushnil(L);
    return 1;
}

static int l_setenv(lua_State *L)
{
    const char *n = luaL_checkstring(L, 1), *v = luaL_checkstring(L, 2);
    int ow = lua_isnone(L, 3) ? 1 : lua_toboolean(L, 3);
    lua_pushboolean(L, env_set(n, v, ow) == 0);
    return 1;
}

static int l_unsetenv(lua_State *L) { env_unset(luaL_checkstring(L, 1)); return 0; }

static void environ_cb(const char *n, const char *v, void *arg)
{
    lua_State *L = arg;
    lua_pushstring(L, v);
    lua_setfield(L, -2, n);
}

static int l_environ(lua_State *L)
{
    lua_newtable(L);
    env_each(environ_cb, L);
    return 1;
}

/* ---- filesystem ------------------------------------------------------ */

static int fail(lua_State *L, int err)
{
    lua_pushnil(L);
    lua_pushstring(L, strerror(-err));
    return 2;
}

static int l_readfile(lua_State *L)
{
    char *b; size_t n;
    int r = fs_readall(luaL_checkstring(L, 1), &b, &n);
    if (r < 0) return fail(L, r);
    lua_pushlstring(L, b, n);
    free(b);
    return 1;
}

static int l_stat(lua_State *L)
{
    int isdir; uint64_t size;
    int r = fs_stat(luaL_checkstring(L, 1), &isdir, &size);
    if (r < 0) return fail(L, r);
    lua_newtable(L);
    lua_pushboolean(L, isdir); lua_setfield(L, -2, "dir");
    lua_pushinteger(L, (lua_Integer)size); lua_setfield(L, -2, "size");
    return 1;
}

static int l_exists(lua_State *L)
{
    int isdir; uint64_t size;
    lua_pushboolean(L, fs_stat(luaL_checkstring(L, 1), &isdir, &size) >= 0);
    return 1;
}

static int dir_cb(const char *name, int isdir, uint64_t size, void *arg)
{
    lua_State *L = arg;
    lua_Integer i = (lua_Integer)lua_rawlen(L, -1) + 1;
    lua_newtable(L);
    lua_pushstring(L, name); lua_setfield(L, -2, "name");
    lua_pushboolean(L, isdir); lua_setfield(L, -2, "dir");
    lua_pushinteger(L, (lua_Integer)size); lua_setfield(L, -2, "size");
    lua_rawseti(L, -2, i);
    return 0;
}

static int l_readdir(lua_State *L)
{
    const char *path = luaL_checkstring(L, 1);
    lua_newtable(L);
    int r = fs_readdir(path, dir_cb, L);
    if (r < 0) { lua_pop(L, 1); return fail(L, r); }
    return 1;
}

/* ---- kernel ---------------------------------------------------------- */

static int l_load_kernel(lua_State *L)
{
    const char *err;
    if (kernel_load(luaL_checkstring(L, 1), &err) < 0) {
        lua_pushnil(L); lua_pushstring(L, err);
        return 2;
    }
    lua_pushboolean(L, 1);
    return 1;
}

static int l_load_module(lua_State *L)
{
    const char *err, *path = luaL_checkstring(L, 1);
    const char *type = luaL_optstring(L, 2, "raw");
    const char *name = luaL_optstring(L, 3, path);
    if (module_load(path, type, name, &err) < 0) {
        lua_pushnil(L); lua_pushstring(L, err);
        return 2;
    }
    lua_pushboolean(L, 1);
    return 1;
}

static int l_kernel(lua_State *L)
{
    const struct loaded_kernel *k = kernel_info();
    if (!k->valid) { lua_pushnil(L); return 1; }
    lua_newtable(L);
    lua_pushstring(L, k->path); lua_setfield(L, -2, "path");
    lua_pushinteger(L, (lua_Integer)k->phys); lua_setfield(L, -2, "phys");
    lua_pushinteger(L, (lua_Integer)k->virt); lua_setfield(L, -2, "virt");
    lua_pushinteger(L, (lua_Integer)k->size); lua_setfield(L, -2, "size");
    lua_pushinteger(L, (lua_Integer)k->entry); lua_setfield(L, -2, "entry");
    return 1;
}

static int l_boot(lua_State *L)
{
    const char *err;
    boot_kernel(luaL_optstring(L, 1, ""), &err);      /* returns only on error */
    lua_pushnil(L);
    lua_pushstring(L, err);
    return 2;
}

static int l_memmap(lua_State *L)
{
    static const char *const names[] = { "?", "usable", "reserved", "acpi-reclaim", "acpi-nvs", "bad", "loader", "kernel" };
    struct prism_mmap_entry *m = malloc(256 * sizeof *m);
    if (!m) return luaL_error(L, "out of memory");
    int n = plat_memmap(m, 256);
    lua_createtable(L, n, 0);
    for (int i = 0; i < n; i++) {
        lua_newtable(L);
        lua_pushinteger(L, (lua_Integer)m[i].base); lua_setfield(L, -2, "base");
        lua_pushinteger(L, (lua_Integer)m[i].length); lua_setfield(L, -2, "length");
        lua_pushstring(L, m[i].type < 8 ? names[m[i].type] : "?"); lua_setfield(L, -2, "type");
        lua_rawseti(L, -2, i + 1);
    }
    free(m);
    return 1;
}

static const luaL_Reg loader_funcs[] = {
    { "write", l_write }, { "size", l_size }, { "getkey", l_getkey },
    { "time", l_time }, { "delay", l_delay },{ "reboot", l_reboot },
    { "shutdown", l_shutdown }, { "getenv", l_getenv }, { "setenv", l_setenv },
    { "unsetenv", l_unsetenv },{ "environ", l_environ },
    { "readfile", l_readfile }, { "stat", l_stat }, { "exists", l_exists },
    { "readdir", l_readdir },{ "load_kernel", l_load_kernel }, { "load_module", l_load_module },
    { "kernel", l_kernel }, { "boot", l_boot }, { "memmap", l_memmap },
    { NULL, NULL }
};

/* ---- minimal 'require' ------------------------------------------------ */

static const char prelude[] =
    "package = { loaded = {}, path = '/boot/lua/?.lua' }\n"
    "function require(name)\n"
    "  local m = package.loaded[name]\n"
    "  if m ~= nil then return m end\n"
    "  local path = package.path:gsub('%?', (name:gsub('%.', '/')))\n"
    "  local src, err = loader.readfile(path)\n"
    "  if not src then error(\"module '\" .. name .. \"' not found (\" .. path .. \": \" .. tostring(err) .. \")\", 2) end\n"
    "  local fn, e = load(src, '@' .. path)\n"
    "  if not fn then error(e, 0) end\n"
    "  local r = fn(name)\n"
    "  if r == nil then r = true end\n"
    "  package.loaded[name] = r\n"
    "  return r\n"
    "end\n";

static int msgh(lua_State *L)
{
    const char *m = lua_tostring(L, 1);
    luaL_traceback(L, L, m ? m : "(error object)", 1);
    return 1;
}

static void *l_alloc(void *ud, void *ptr, size_t osize, size_t nsize)
{
    (void)ud; (void)osize;
    if (!nsize) { free(ptr); return NULL; }
    return realloc(ptr, nsize);
}

int loader_run(void)
{
    static const luaL_Reg libs[] = {
        { "_G", luaopen_base }, { LUA_TABLIBNAME, luaopen_table },
        { LUA_STRLIBNAME, luaopen_string }, { LUA_MATHLIBNAME, luaopen_math },
        { LUA_UTF8LIBNAME, luaopen_utf8 }, { LUA_COLIBNAME, luaopen_coroutine },
        { NULL, NULL }
    };
    char *src; size_t len;
    int r;

    lua_State *L = lua_newstate(l_alloc, NULL);
    if (!L) { printf("lua: out of memory\n"); return -1; }
    for (const luaL_Reg *l = libs; l->func; l++) {
        luaL_requiref(L, l->name, l->func, 1);
        lua_pop(L, 1);
    }
    luaL_newlib(L, loader_funcs);
    lua_pushstring(L, plat_name()); lua_setfield(L, -2, "platform");
    lua_pushstring(L, LOADER_VERSION); lua_setfield(L, -2, "version");
    lua_pushstring(L, LUA_RELEASE); lua_setfield(L, -2, "lua_version");
    lua_setglobal(L, "loader");

    if (luaL_dostring(L, prelude)) {
        printf("lua: %s\n", lua_tostring(L, -1));
        return -1;
    }
    r = fs_readall(LOADER_SCRIPT, &src, &len);
    if (r < 0) {
        printf("cannot read %s: %s\n", LOADER_SCRIPT, strerror(-r));
        return -1;
    }
    lua_pushcfunction(L, msgh);
    if (luaL_loadbuffer(L, src, len, "@" LOADER_SCRIPT) != LUA_OK || lua_pcall(L, 0, 0, -2) != LUA_OK) {
        printf("\n%s\n", lua_tostring(L, -1));
        free(src);
        return -1;
    }
    free(src);
    lua_close(L);
    return 0;
}
