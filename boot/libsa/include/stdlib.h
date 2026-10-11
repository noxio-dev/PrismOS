/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_STDLIB_H
#define _LIBSA_STDLIB_H
#include <stddef.h>
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
void *malloc(size_t);
void *calloc(size_t, size_t);
void *realloc(void *, size_t);
void free(void *);
void abort(void) __attribute__((noreturn));
void exit(int) __attribute__((noreturn));
int abs(int);
long labs(long);
long long llabs(long long);
long strtol(const char *, char **, int);
unsigned long strtoul(const char *, char **, int);
long long strtoll(const char *, char **, int);
unsigned long long strtoull(const char *, char **, int);
double strtod(const char *, char **);
int atoi(const char *);
char *getenv(const char *);
/* loader specific */
void heap_init(void *base, size_t size);
size_t heap_free_bytes(void);
#endif
