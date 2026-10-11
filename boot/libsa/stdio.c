/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include "../loader/fs.h"

extern void console_write(const char *s, size_t n);

/*
 * FILE emulation: files are read-only and loaded whole into memory on open
 * (they are small boot scripts).  stdout/stderr go to the loader console.
 */
struct libsa_file {
    char *buf;
    size_t len, pos;
    int eof, err, ungot, kind;     /* kind: 0 file, 1 stdout, 2 stderr, 3 stdin */
    char *path;
};

static struct libsa_file f_in  = { 0, 0, 0, 0, 0, -1, 3, 0 };
static struct libsa_file f_out = { 0, 0, 0, 0, 0, -1, 1, 0 };
static struct libsa_file f_err = { 0, 0, 0, 0, 0, -1, 2, 0 };
FILE *stdin = &f_in, *stdout = &f_out, *stderr = &f_err;

static int load(FILE *f, const char *path)
{
    char *b; size_t n;
    int r = fs_readall(path, &b, &n);
    if (r < 0) { errno = -r; return -1; }
    free(f->buf);
    f->buf = b; f->len = n; f->pos = 0; f->eof = f->err = 0; f->ungot = -1;
    return 0;
}

FILE *fopen(const char *path, const char *mode)
{
    if (mode[0] != 'r' || strchr(mode, '+')) { errno = EROFS; return NULL; }
    FILE *f = calloc(1, sizeof *f);
    if (!f) { errno = ENOMEM; return NULL; }
    f->ungot = -1;
    if (load(f, path)) { free(f); return NULL; }
    f->path = strdup(path);
    return f;
}

FILE *freopen(const char *path, const char *mode, FILE *f)
{
    (void)mode;
    if (!f || f->kind || !path) return NULL;
    if (load(f, path)) { fclose(f); return NULL; }
    return f;
}

int fclose(FILE *f)
{
    if (!f || f->kind) return 0;
    free(f->buf); free(f->path); free(f);
    return 0;
}

size_t fread(void *p, size_t sz, size_t n, FILE *f)
{
    if (f->kind || !sz || !n) return 0;
    size_t want = sz * n, have = f->len - f->pos, got = 0;
    if (f->ungot >= 0 && want) {
        *(char *)p = (char)f->ungot; f->ungot = -1; got = 1; p = (char *)p + 1; want--;
    }
    if (want > have) { want = have; f->eof = 1; }
    memcpy(p, f->buf + f->pos, want);
    f->pos += want;
    return (got + want) / sz;
}

size_t fwrite(const void *p, size_t sz, size_t n, FILE *f)
{
    if (f->kind == 1 || f->kind == 2) { console_write(p, sz * n); return n; }
    f->err = 1;
    return 0;
}

int fflush(FILE *f) { (void)f; return 0; }

int getc(FILE *f)
{
    if (f->kind) return EOF;
    if (f->ungot >= 0) { int c = f->ungot; f->ungot = -1; return c; }
    if (f->pos >= f->len) { f->eof = 1; return EOF; }
    return (unsigned char)f->buf[f->pos++];
}
int fgetc(FILE *f) { return getc(f); }

int ungetc(int c, FILE *f)
{
    if (c == EOF || f->ungot >= 0) return EOF;
    f->ungot = c; f->eof = 0;
    return c;
}

int feof(FILE *f) { return f->eof; }
int ferror(FILE *f) { return f->err; }
void clearerr(FILE *f) { f->eof = f->err = 0; }

int fputc(int c, FILE *f) { char ch = (char)c; return fwrite(&ch, 1, 1, f) ? c : EOF; }
int putchar(int c) { return fputc(c, stdout); }
int fputs(const char *s, FILE *f) { return fwrite(s, 1, strlen(s), f) ? 0 : EOF; }
int puts(const char *s) { fputs(s, stdout); fputc('\n', stdout); return 0; }

int vfprintf(FILE *f, const char *fmt, va_list ap)
{
    char small[512], *b = small;
    va_list ap2;
    va_copy(ap2, ap);
    int n = vsnprintf(small, sizeof small, fmt, ap);
    if (n >= (int)sizeof small) {
        b = malloc(n + 1);
        if (!b) { va_end(ap2); return -1; }
        vsnprintf(b, n + 1, fmt, ap2);
    }
    va_end(ap2);
    fwrite(b, 1, n, f);
    if (b != small) free(b);
    return n;
}

int vprintf(const char *fmt, va_list ap) { return vfprintf(stdout, fmt, ap); }

int printf(const char *fmt, ...)
{
    va_list ap; va_start(ap, fmt);
    int r = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return r;
}

int fprintf(FILE *f, const char *fmt, ...)
{
    va_list ap; va_start(ap, fmt);
    int r = vfprintf(f, fmt, ap);
    va_end(ap);
    return r;
}
