/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_LOCALE_H
#define _LIBSA_LOCALE_H
struct lconv { char *decimal_point; };
struct lconv *localeconv(void);
#endif
