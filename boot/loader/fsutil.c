/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdlib.h>
#include "fs.h"

int fs_readall(const char *path, char **buf, size_t *len)
{
    fs_file *f;
    int r = fs_open(path, &f);
    if (r < 0) return r;
    uint64_t size = fs_size(f);
    /* The size comes straight from the on-disk inode (64 bits, attacker
     * controlled).  Refuse anything that cannot fit in the loader heap:
     * this also rules out the size + 1 wrap-around (size == UINT64_MAX)
     * that used to turn into malloc(0) followed by an unbounded read. */
    if (size >= (uint64_t)heap_free_bytes()) { fs_close(f); return -12; }
    char *b = malloc((size_t)size + 1);
    if (!b) { fs_close(f); return -12; }
    long got = fs_read(f, b, size);
    fs_close(f);
    if (got != (long)size) { free(b); return -5; }
    b[size] = 0;
    *buf = b;
    *len = size;
    return 0;
}
