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
    char *b = malloc(size + 1);
    if (!b) { fs_close(f); return -12; }
    long got = fs_read(f, b, size);
    fs_close(f);
    if (got != (long)size) { free(b); return -5; }
    b[size] = 0;
    *buf = b;
    *len = size;
    return 0;
}
