/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_ASSERT_H
#define _LIBSA_ASSERT_H
#ifdef NDEBUG
#define assert(x) ((void)0)
#else
void libsa_assert_fail(const char *, const char *, int);
#define assert(x) ((x) ? (void)0 : libsa_assert_fail(#x, __FILE__, __LINE__))
#endif
#endif
