/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include <ctype.h>
#include <stdio.h>
#include <locale.h>
#include <time.h>
#include <math.h>

int errno;

extern void console_panic(const char *msg) __attribute__((noreturn));
extern unsigned long long plat_ms(void);

void abort(void) { console_panic("abort()"); }
void exit(int c) { (void)c; console_panic("exit()"); }
void libsa_assert_fail(const char *e, const char *f, int l)
{
    printf("assertion failed: %s (%s:%d)\n", e, f, l);
    abort();
}

int abs(int x) { return x < 0 ? -x : x; }
long labs(long x) { return x < 0 ? -x : x; }
long long llabs(long long x) { return x < 0 ? -x : x; }
char *getenv(const char *n) { (void)n; return NULL; }
time_t time(time_t *t) { time_t v = plat_ms() / 1000; if (t) *t = v; return v; }
clock_t clock(void) { return (clock_t)plat_ms(); }

static struct lconv lconv_c = { "." };
struct lconv *localeconv(void) { return &lconv_c; }

/* ---- allocator: address ordered blocks, first fit (rover), coalescing ---- */
struct blk {
    size_t size;          /* total size including header, multiple of 16 */
    size_t prev;          /* size of previous block (0 for first) */
    unsigned used;
    unsigned magic;
    char pad[16];         /* keeps payload 32-byte offset: header = 32 bytes */
};
#define HDR sizeof(struct blk)
#define BMAGIC 0xB10CB10Cu

static char *heap_lo, *heap_hi;
static struct blk *rover;
static size_t free_bytes;

static struct blk *nextb(struct blk *b)
{
    char *n = (char *)b + b->size;
    return n >= heap_hi ? NULL : (struct blk *)n;
}

void heap_init(void *base, size_t size)
{
    uintptr_t a = ((uintptr_t)base + 15) & ~(uintptr_t)15;
    size -= a - (uintptr_t)base;
    size &= ~(size_t)15;
    heap_lo = (char *)a;
    heap_hi = heap_lo + size;
    struct blk *b = (struct blk *)heap_lo;
    b->size = size; b->prev = 0; b->used = 0; b->magic = BMAGIC;
    rover = b;
    free_bytes = size;
}

size_t heap_free_bytes(void) { return free_bytes; }

void *malloc(size_t n)
{
    if (!n) n = 1;
    n = (n + HDR + 15) & ~(size_t)15;
    if (!rover) return NULL;
    struct blk *start = rover, *b = rover;
    do {
        if (!b->used && b->size >= n) {
            if (b->size >= n + HDR + 16) {
                struct blk *r = (struct blk *)((char *)b + n);
                r->size = b->size - n; r->prev = n; r->used = 0; r->magic = BMAGIC;
                struct blk *rn = nextb(r);
                if (rn) rn->prev = r->size;
                b->size = n;
            }
            b->used = 1;
            free_bytes -= b->size;
            rover = nextb(b) ? nextb(b) : (struct blk *)heap_lo;
            return (char *)b + HDR;
        }
        b = nextb(b);
        if (!b) b = (struct blk *)heap_lo;
    } while (b != start);
    return NULL;
}

void free(void *p)
{
    if (!p) return;
    struct blk *b = (struct blk *)((char *)p - HDR);
    if (b->magic != BMAGIC || !b->used)
        console_panic("free(): corrupt heap");
    b->used = 0;
    free_bytes += b->size;
    struct blk *n = nextb(b);
    if (n && !n->used) {
        if (rover == n) rover = b;
        b->size += n->size;
        n = nextb(b);
        if (n) n->prev = b->size;
    }
    if (b->prev) {
        struct blk *pr = (struct blk *)((char *)b - b->prev);
        if (!pr->used) {
            if (rover == b) rover = pr;
            pr->size += b->size;
            n = nextb(pr);
            if (n) n->prev = pr->size;
        }
    }
}

void *realloc(void *p, size_t n)
{
    if (!p) return malloc(n);
    if (!n) { free(p); return NULL; }
    struct blk *b = (struct blk *)((char *)p - HDR);
    size_t have = b->size - HDR;
    if (n <= have) return p;
    void *q = malloc(n);
    if (!q) return NULL;
    memcpy(q, p, have);
    free(p);
    return q;
}

void *calloc(size_t a, size_t b)
{
    size_t n = a * b;
    if (a && n / a != b) return NULL;
    void *p = malloc(n);
    if (p) memset(p, 0, n);
    return p;
}

/* ---- number parsing ---- */
unsigned long long strtoull(const char *s, char **end, int base)
{
    const char *p = s;
    unsigned long long v = 0;
    int neg = 0, any = 0;
    while (isspace((unsigned char)*p)) p++;
    if (*p == '-') { neg = 1; p++; } else if (*p == '+') p++;
    if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X') && isxdigit((unsigned char)p[2])) {
        p += 2; base = 16;
    } else if (base == 0) {
        base = (p[0] == '0') ? 8 : 10;
    }
    for (;; p++) {
        int d;
        if (isdigit((unsigned char)*p)) d = *p - '0';
        else if (isalpha((unsigned char)*p)) d = tolower((unsigned char)*p) - 'a' + 10;
        else break;
        if (d >= base) break;
        v = v * base + d;
        any = 1;
    }
    if (end) *end = (char *)(any ? p : s);
    return neg ? -v : v;
}

long long strtoll(const char *s, char **e, int b) { return (long long)strtoull(s, e, b); }
unsigned long strtoul(const char *s, char **e, int b) { return (unsigned long)strtoull(s, e, b); }
long strtol(const char *s, char **e, int b) { return (long)strtoull(s, e, b); }
int atoi(const char *s) { return (int)strtol(s, NULL, 10); }

static double p10(int e)
{
    double r = 1.0, b = 10.0;
    unsigned n = e < 0 ? -e : e;
    while (n) { if (n & 1) r *= b; b *= b; n >>= 1; }
    return r;
}

double strtod(const char *s, char **end)
{
    const char *p = s;
    int neg = 0, any = 0, dexp = 0;
    unsigned long long m = 0;
    int digits = 0;
    double v;

    while (isspace((unsigned char)*p)) p++;
    if (*p == '-') { neg = 1; p++; } else if (*p == '+') p++;

    if (p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) {     /* hex float */
        const char *q = p + 2;
        double h = 0; int seen = 0, fexp = 0, dot = 0;
        for (;; q++) {
            if (*q == '.' && !dot) { dot = 1; continue; }
            if (!isxdigit((unsigned char)*q)) break;
            h = h * 16 + (isdigit((unsigned char)*q) ? *q - '0' : tolower((unsigned char)*q) - 'a' + 10);
            if (dot) fexp -= 4;
            seen = 1;
        }
        if (seen) {
            if (*q == 'p' || *q == 'P') {
                const char *r = q + 1; int en = 0, ng = 0;
                if (*r == '-') { ng = 1; r++; } else if (*r == '+') r++;
                if (isdigit((unsigned char)*r)) {
                    while (isdigit((unsigned char)*r)) en = en * 10 + (*r++ - '0');
                    fexp += ng ? -en : en; q = r;
                }
            }
            if (end) *end = (char *)q;
            h = ldexp(h, fexp);
            return neg ? -h : h;
        }
    }

    for (;; p++) {
        if (isdigit((unsigned char)*p)) {
            any = 1;
            if (digits < 19) { m = m * 10 + (*p - '0'); if (m) digits++; }
            else dexp++;
        } else break;
    }
    if (*p == '.') {
        p++;
        for (;; p++) {
            if (isdigit((unsigned char)*p)) {
                any = 1;
                if (digits < 19) { m = m * 10 + (*p - '0'); if (m) digits++; dexp--; }
            } else break;
        }
    }
    if (!any) { if (end) *end = (char *)s; return 0.0; }
    if (*p == 'e' || *p == 'E') {
        const char *r = p + 1; int en = 0, ng = 0;
        if (*r == '-') { ng = 1; r++; } else if (*r == '+') r++;
        if (isdigit((unsigned char)*r)) {
            while (isdigit((unsigned char)*r)) { if (en < 10000) en = en * 10 + (*r - '0'); r++; }
            dexp += ng ? -en : en;
            p = r;
        }
    }
    v = (double)m;
    if (m) {
        if (dexp > 308) v = HUGE_VAL;
        else if (dexp > 0) v *= p10(dexp);
        else if (dexp < -330) v = 0.0;
        else if (dexp < -300) { v /= p10(300); v /= p10(-dexp - 300); }
        else if (dexp < 0) v /= p10(-dexp);
    }
    if (end) *end = (char *)p;
    return neg ? -v : v;
}
