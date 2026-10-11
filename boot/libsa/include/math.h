/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_MATH_H
#define _LIBSA_MATH_H
#define HUGE_VAL (__builtin_huge_val())
#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define M_PI 3.14159265358979323846
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
double floor(double); double ceil(double); double fabs(double);
double fmod(double, double); double pow(double, double);
double frexp(double, int *); double ldexp(double, int);
double sqrt(double); double exp(double); double log(double);
double log2(double); double log10(double);
double sin(double); double cos(double); double tan(double);
double asin(double); double acos(double); double atan(double);
double atan2(double, double);
#endif
