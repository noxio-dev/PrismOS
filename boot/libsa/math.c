/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <math.h>
#include <stdint.h>

double fabs(double x) { return __builtin_fabs(x); }
double sqrt(double x) { return __builtin_sqrt(x); }

double floor(double x)
{
    if (!(fabs(x) < 4503599627370496.0)) return x;
    double t = (double)(long long)x;
    return t > x ? t - 1.0 : t;
}

double ceil(double x)
{
    if (!(fabs(x) < 4503599627370496.0)) return x;
    double t = (double)(long long)x;
    return t < x ? t + 1.0 : t;
}

double fmod(double x, double y)
{
    if (y == 0.0 || isinf(x) || isnan(x) || isnan(y)) return NAN;
    if (isinf(y)) return x;
    double r;
    __asm__ volatile("1: fprem\n fnstsw %%ax\n testb $4, %%ah\n jnz 1b"
                     : "=t"(r) : "0"(x), "u"(y) : "ax", "cc");
    return r;
}

double frexp(double x, int *e)
{
    union { double d; uint64_t u; } v = { x };
    int ex = (int)((v.u >> 52) & 0x7FF);
    if (ex == 0x7FF || x == 0.0) { *e = 0; return x; }
    if (ex == 0) {                       /* subnormal */
        v.d = x * 18014398509481984.0;   /* 2^54 */
        ex = (int)((v.u >> 52) & 0x7FF) - 54;
    }
    *e = ex - 1022;
    v.u = (v.u & 0x800FFFFFFFFFFFFFULL) | (1022ULL << 52);
    return v.d;
}

double ldexp(double x, int n)
{
    if (x == 0.0 || isinf(x) || isnan(x)) return x;
    while (n > 1000) { x *= 0x1p1000; n -= 1000; if (isinf(x)) return x; }
    while (n < -1000) { x *= 0x1p-1000; n += 1000; if (x == 0.0) return x; }
    union { double d; uint64_t u; } s = { 1.0 };
    s.u = (uint64_t)(1023 + n) << 52;
    return x * s.d;
}

static double fyl2x(double x, double y)
{
    double r;
    __asm__("fyl2x" : "=t"(r) : "0"(x), "u"(y) : "st(1)");
    return r;
}

double log2(double x)
{
    if (isnan(x) || x < 0.0) return NAN;
    if (x == 0.0) return -HUGE_VAL;
    if (isinf(x)) return x;
    return fyl2x(x, 1.0);
}
double log(double x)
{
    if (isnan(x) || x < 0.0) return NAN;
    if (x == 0.0) return -HUGE_VAL;
    if (isinf(x)) return x;
    return fyl2x(x, 0.693147180559945309417);
}
double log10(double x)
{
    if (isnan(x) || x < 0.0) return NAN;
    if (x == 0.0) return -HUGE_VAL;
    if (isinf(x)) return x;
    return fyl2x(x, 0.301029995663981195214);
}

double exp(double x)
{
    if (isnan(x)) return x;
    if (x > 709.78) return HUGE_VAL;
    if (x < -745.2) return 0.0;
    double k = floor(x * 1.4426950408889634 + 0.5);
    double r = x - k * 6.93147180369123816490e-01 - k * 1.90821492927058770002e-10;
    double t = 1.0, s = 1.0;
    for (int i = 1; i <= 20; i++) { t *= r / i; s += t; }
    return ldexp(s, (int)k);
}

double pow(double x, double y)
{
    if (y == 0.0 || x == 1.0) return 1.0;
    if (isnan(x) || isnan(y)) return NAN;
    if (isinf(y)) {
        double a = fabs(x);
        if (a == 1.0) return 1.0;
        return ((a < 1.0) == (y > 0.0)) ? 0.0 : HUGE_VAL;
    }
    if (floor(y) == y && fabs(y) < 2147483648.0) {
        long long n = (long long)y;
        unsigned long long u = n < 0 ? -(unsigned long long)n : (unsigned long long)n;
        double r = 1.0, b = x;
        while (u) { if (u & 1) r *= b; b *= b; u >>= 1; }
        return n < 0 ? 1.0 / r : r;
    }
    if (x == 0.0) return y > 0.0 ? 0.0 : HUGE_VAL;
    if (x < 0.0) return NAN;
    return exp(y * log(x));
}

double sin(double x) { double r; __asm__("fsin" : "=t"(r) : "0"(x)); return r; }
double cos(double x) { double r; __asm__("fcos" : "=t"(r) : "0"(x)); return r; }
double tan(double x) { double r; __asm__("fptan\n fstp %%st(0)" : "=t"(r) : "0"(x)); return r; }
double atan2(double y, double x)
{
    double r;
    __asm__("fpatan" : "=t"(r) : "0"(x), "u"(y) : "st(1)");
    return r;
}
double atan(double x) { return atan2(x, 1.0); }
double asin(double x) { return atan2(x, sqrt((1.0 - x) * (1.0 + x))); }
double acos(double x) { return atan2(sqrt((1.0 - x) * (1.0 + x)), x); }
