/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_STDIO_H
#define _LIBSA_STDIO_H
#include <stddef.h>
#include <stdarg.h>
#define EOF (-1)
#define BUFSIZ 512
typedef struct libsa_file FILE;
extern FILE *stdin, *stdout, *stderr;
FILE *fopen(const char *, const char *);
FILE *freopen(const char *, const char *, FILE *);
int fclose(FILE *);
size_t fread(void *, size_t, size_t, FILE *);
size_t fwrite(const void *, size_t, size_t, FILE *);
int fflush(FILE *);
int getc(FILE *);
int fgetc(FILE *);
int ungetc(int, FILE *);
int feof(FILE *);
int ferror(FILE *);
void clearerr(FILE *);
int fputs(const char *, FILE *);
int fputc(int, FILE *);
int putchar(int);
int puts(const char *);
int printf(const char *, ...) __attribute__((format(printf, 1, 2)));
int fprintf(FILE *, const char *, ...) __attribute__((format(printf, 2, 3)));
int vprintf(const char *, va_list);
int vfprintf(FILE *, const char *, va_list);
int sprintf(char *, const char *, ...) __attribute__((format(printf, 2, 3)));
int snprintf(char *, size_t, const char *, ...) __attribute__((format(printf, 3, 4)));
int vsnprintf(char *, size_t, const char *, va_list);
#endif
