/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <math.h>

struct out { char *buf; size_t size, pos; };
static int signbit_d(double v) { union { double d; uint64_t u; } x = { v }; return (int)(x.u >> 63); }

static void put(struct out *o, char c)
{
    if (o->pos + 1 < o->size)
        o->buf[o->pos] = c;
    o->pos++;
}

static const double ptab[] = { 1e1, 1e2, 1e4, 1e8, 1e16, 1e32, 1e64, 1e128, 1e256 };

/*
 * Generate n (1..17) significant decimal digits of v > 0 into d[] and return
 * the decimal exponent of the first digit.  Scaling uses binary powers of ten
 * so results can be off by one unit in the last place for values far from 1:
 * good enough for a boot loader (Lua prints with %.14g).
 */
static int gen_digits(double v, int n, char *d)
{
    int e = 0, i;
    uint64_t p = 1, m;

    if (v >= 10.0) {
        for (i = 8; i >= 0; i--)
            if (v >= ptab[i]) { v /= ptab[i]; e += 1 << i; }
    } else if (v < 1.0) {
        for (i = 8; i >= 0; i--)
            if (v * ptab[i] < 10.0) { v *= ptab[i]; e -= 1 << i; }
    }
    if (v < 1.0) { v *= 10.0; e--; }
    if (v >= 10.0) { v /= 10.0; e++; }
    for (i = 1; i < n; i++) p *= 10;
    m = (uint64_t)(v * (double)p + 0.5);
    if (m >= p * 10) { m = p; e++; }
    for (i = n - 1; i >= 0; i--) { d[i] = '0' + (char)(m % 10); m /= 10; }
    return e;
}

static int emit_fixed(char *o, const char *d, int n, int e, int prec, int alt)
{
    char *s = o;
    int k, j, idx;
    if (e < 0) *s++ = '0';
    else for (k = 0; k <= e; k++) *s++ = k < n ? d[k] : '0';
    if (prec > 0 || alt) *s++ = '.';
    for (j = 1; j <= prec; j++) {
        idx = e + j;
        *s++ = (idx >= 0 && idx < n) ? d[idx] : '0';
    }
    return (int)(s - o);
}

static int emit_exp(char *o, const char *d, int n, int e, int prec, int alt, int upper)
{
    char *s = o;
    int j, ae = e < 0 ? -e : e;
    *s++ = d[0];
    if (prec > 0 || alt) *s++ = '.';
    for (j = 1; j <= prec; j++) *s++ = j < n ? d[j] : '0';
    *s++ = upper ? 'E' : 'e';
    *s++ = e < 0 ? '-' : '+';
    if (ae >= 100) *s++ = '0' + ae / 100;
    *s++ = '0' + (ae / 10) % 10;
    *s++ = '0' + ae % 10;
    return (int)(s - o);
}

static int strip_zeros(char *o, int len)
{
    char *dot = memchr(o, '.', len);
    char *ex = memchr(o, 'e', len);
    if (!ex) ex = memchr(o, 'E', len);
    if (!dot) return len;
    int end = ex ? (int)(ex - o) : len;
    int i = end;
    while (i > (int)(dot - o) + 1 && o[i - 1] == '0') i--;
    if (i == (int)(dot - o) + 1) i--;
    if (ex) { memmove(o + i, ex, len - end); return i + len - end; }
    return i;
}

/* body of a finite non-negative double; returns length */
static int fmt_double(char *o, double v, int conv, int prec, int alt)
{
    char d[24];
    int e, n, upper = conv >= 'A' && conv <= 'Z';
    int lc = upper ? conv + 32 : conv;
    if (prec < 0) prec = 6;

    if (v == 0.0) {
        memset(d, '0', sizeof d); e = 0;
        if (lc == 'g') {
            int len = alt ? emit_fixed(o, d, 1, 0, prec ? prec - 1 : 0, alt)
                          : emit_fixed(o, d, 1, 0, 0, 0);
            return len;
        }
        return lc == 'e' ? emit_exp(o, d, 1, 0, prec, alt, upper)
                         : emit_fixed(o, d, 1, 0, prec, alt);
    }
    if (lc == 'e') {
        n = prec + 1 > 17 ? 17 : prec + 1;
        e = gen_digits(v, n, d);
        return emit_exp(o, d, n, e, prec, alt, upper);
    }
    if (lc == 'f') {
        int nd;
        gen_digits(v, 17, d);
        e = gen_digits(v, 17, d);
        nd = e + 1 + prec;
        if (nd > 17) { n = 17; e = gen_digits(v, n, d); }
        else if (nd >= 1) { n = nd; e = gen_digits(v, n, d); }
        else if (nd == 0) {
            gen_digits(v, 1, d);
            if (d[0] >= '5') { d[0] = '1'; e = -prec; n = 1; }
            else { d[0] = '0'; e = 0; n = 1; }
        } else { d[0] = '0'; e = 0; n = 1; }
        return emit_fixed(o, d, n, e, prec, alt);
    }
    /* %g */
    {
        int P = prec == 0 ? 1 : prec, len;
        n = P > 17 ? 17 : P;
        e = gen_digits(v, n, d);
        if (e < -4 || e >= P) len = emit_exp(o, d, n, e, P - 1, alt, upper);
        else len = emit_fixed(o, d, n, e, P - 1 - e, alt);
        return alt ? len : strip_zeros(o, len);
    }
}

#define F_MINUS 1
#define F_PLUS  2
#define F_SPACE 4
#define F_ALT   8
#define F_ZERO  16

static void pad(struct out *o, char c, int n) { while (n-- > 0) put(o, c); }

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap)
{
    struct out o = { buf, size, 0 };

    for (; *fmt; fmt++) {
        if (*fmt != '%') { put(&o, *fmt); continue; }
        fmt++;
        int flags = 0, width = 0, prec = -1, lmod = 0;
        for (;; fmt++) {
            if (*fmt == '-') flags |= F_MINUS;
            else if (*fmt == '+') flags |= F_PLUS;
            else if (*fmt == ' ') flags |= F_SPACE;
            else if (*fmt == '#') flags |= F_ALT;
            else if (*fmt == '0') flags |= F_ZERO;
            else break;
        }
        if (*fmt == '*') {
            width = va_arg(ap, int); fmt++;
            if (width < 0) { flags |= F_MINUS; width = -width; }
        } else while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
        if (*fmt == '.') {
            fmt++; prec = 0;
            if (*fmt == '*') { prec = va_arg(ap, int); fmt++; }
            else while (*fmt >= '0' && *fmt <= '9') prec = prec * 10 + (*fmt++ - '0');
        }
        for (;; fmt++) {
            if (*fmt == 'l') lmod++;
            else if (*fmt == 'h') lmod--;
            else if (*fmt == 'z' || *fmt == 'j' || *fmt == 't') lmod = 2;
            else if (*fmt == 'L' || *fmt == 'q') lmod = 2;
            else break;
        }
        char tmp[420];
        const char *body = tmp, *prefix = "";
        int blen = 0, zeros = 0, c = *fmt;

        switch (c) {
        case '%': put(&o, '%'); continue;
        case 'c':
            tmp[0] = (char)va_arg(ap, int); blen = 1;
            flags &= ~F_ZERO;
            break;
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            blen = prec >= 0 ? (int)strnlen(s, prec) : (int)strlen(s);
            body = s; flags &= ~F_ZERO;
            break;
        }
        case 'p': {
            uintptr_t v = (uintptr_t)va_arg(ap, void *);
            int i = 0; char t[24];
            do { t[i++] = "0123456789abcdef"[v & 15]; v >>= 4; } while (v);
            while (i) tmp[blen++] = t[--i];
            prefix = "0x"; flags &= ~F_ZERO;
            break;
        }
        case 'd': case 'i': case 'u': case 'x': case 'X': case 'o': {
            unsigned long long v;
            int neg = 0, base = (c == 'x' || c == 'X') ? 16 : c == 'o' ? 8 : 10;
            if (c == 'd' || c == 'i') {
                long long sv = lmod >= 2 ? va_arg(ap, long long)
                             : lmod == 1 ? (long long)va_arg(ap, long)
                             : lmod == -1 ? (short)va_arg(ap, int)
                             : lmod <= -2 ? (signed char)va_arg(ap, int)
                             : (long long)va_arg(ap, int);
                if (sv < 0) { neg = 1; v = -(unsigned long long)sv; } else v = sv;
                prefix = neg ? "-" : (flags & F_PLUS) ? "+" : (flags & F_SPACE) ? " " : "";
            } else {
                v = lmod >= 2 ? va_arg(ap, unsigned long long)
                  : lmod == 1 ? (unsigned long long)va_arg(ap, unsigned long)
                  : lmod == -1 ? (unsigned short)va_arg(ap, unsigned)
                  : lmod <= -2 ? (unsigned char)va_arg(ap, unsigned)
                  : (unsigned long long)va_arg(ap, unsigned);
                if ((flags & F_ALT) && v) prefix = c == 'x' ? "0x" : c == 'X' ? "0X" : c == 'o' ? "0" : "";
            }
            char t[24]; int i = 0;
            const char *dg = c == 'X' ? "0123456789ABCDEF" : "0123456789abcdef";
            do { t[i++] = dg[v % base]; v /= base; } while (v);
            while (i) tmp[blen++] = t[--i];
            if (prec >= 0) { flags &= ~F_ZERO; if (prec == 0 && blen == 1 && tmp[0] == '0') blen = 0; if (prec > blen) zeros = prec - blen; }
            break;
        }
        case 'f': case 'F': case 'e': case 'E': case 'g': case 'G': case 'a': case 'A': {
            double v = va_arg(ap, double);
            int neg = signbit_d(v);
            if (neg) v = -v;
            prefix = neg ? "-" : (flags & F_PLUS) ? "+" : (flags & F_SPACE) ? " " : "";
            if (v != v) { memcpy(tmp, c < 'a' ? "NAN" : "nan", 3); blen = 3; flags &= ~F_ZERO; prefix = ""; }
            else if (isinf(v)) { memcpy(tmp, c < 'a' ? "INF" : "inf", 3); blen = 3; flags &= ~F_ZERO; }
            else {
                int cc = (c == 'a' || c == 'A') ? (c == 'a' ? 'g' : 'G') : c;
                if (cc == 'F') cc = 'f';
                blen = fmt_double(tmp, v, cc, prec, flags & F_ALT);
            }
            break;
        }
        default:
            put(&o, '%'); if (*fmt) put(&o, *fmt); else fmt--;
            continue;
        }
        int plen = (int)strlen(prefix);
        int total = plen + zeros + blen;
        if (!(flags & (F_MINUS | F_ZERO))) pad(&o, ' ', width - total);
        for (int i = 0; i < plen; i++) put(&o, prefix[i]);
        if ((flags & F_ZERO) && !(flags & F_MINUS)) pad(&o, '0', width - total);
        pad(&o, '0', zeros);
        for (int i = 0; i < blen; i++) put(&o, body[i]);
        if (flags & F_MINUS) pad(&o, ' ', width - total);
    }
    if (size) o.buf[o.pos < size ? o.pos : size - 1] = 0;
    return (int)o.pos;
}

int snprintf(char *b, size_t n, const char *f, ...)
{
    va_list ap; va_start(ap, f);
    int r = vsnprintf(b, n, f, ap);
    va_end(ap);
    return r;
}

int sprintf(char *b, const char *f, ...)
{
    va_list ap; va_start(ap, f);
    int r = vsnprintf(b, (size_t)-1 >> 1, f, ap);
    va_end(ap);
    return r;
}
