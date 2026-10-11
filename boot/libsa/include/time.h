/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_TIME_H
#define _LIBSA_TIME_H
typedef long long time_t;
typedef long long clock_t;
#define CLOCKS_PER_SEC 1000
time_t time(time_t *);
clock_t clock(void);
#endif
