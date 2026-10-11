/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <string.h>
#include <stdlib.h>
#include <errno.h>

void *memcpy(void *d, const void *s, size_t n)
{
    void *r = d;
    __asm__ volatile("rep movsb" : "+D"(d), "+S"(s), "+c"(n) : : "memory");
    return r;
}

void *memmove(void *d, const void *s, size_t n)
{
    if ((unsigned char *)d <= (unsigned char *)s ||
        (unsigned char *)d >= (unsigned char *)s + n)
        return memcpy(d, s, n);
    unsigned char *dd = (unsigned char *)d + n - 1;
    const unsigned char *ss = (const unsigned char *)s + n - 1;
    while (n--)
        *dd-- = *ss--;
    return d;
}

void *memset(void *d, int c, size_t n)
{
    void *r = d;
    __asm__ volatile("rep stosb" : "+D"(d), "+c"(n) : "a"(c) : "memory");
    return r;
}

int memcmp(const void *a, const void *b, size_t n)
{
    const unsigned char *x = a, *y = b;
    for (; n; n--, x++, y++)
        if (*x != *y)
            return *x - *y;
    return 0;
}

void *memchr(const void *s, int c, size_t n)
{
    const unsigned char *p = s;
    for (; n; n--, p++)
        if (*p == (unsigned char)c)
            return (void *)p;
    return NULL;
}

size_t strlen(const char *s)
{
    const char *p = s;
    while (*p) p++;
    return p - s;
}

size_t strnlen(const char *s, size_t n)
{
    size_t i = 0;
    while (i < n && s[i]) i++;
    return i;
}

int strcmp(const char *a, const char *b)
{
    while (*a && *a == *b) { a++; b++; }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n)
{
    for (; n; n--, a++, b++) {
        if (*a != *b) return (unsigned char)*a - (unsigned char)*b;
        if (!*a) return 0;
    }
    return 0;
}

static int lc(int c) { return (c >= 'A' && c <= 'Z') ? c + 32 : c; }

int strcasecmp(const char *a, const char *b)
{
    while (*a && lc(*a) == lc(*b)) { a++; b++; }
    return lc((unsigned char)*a) - lc((unsigned char)*b);
}

int strncasecmp(const char *a, const char *b, size_t n)
{
    for (; n; n--, a++, b++) {
        if (lc(*a) != lc(*b)) return lc((unsigned char)*a) - lc((unsigned char)*b);
        if (!*a) return 0;
    }
    return 0;
}

int strcoll(const char *a, const char *b) { return strcmp(a, b); }

char *strcpy(char *d, const char *s)
{
    char *r = d;
    while ((*d++ = *s++)) ;
    return r;
}

char *strncpy(char *d, const char *s, size_t n)
{
    size_t i = 0;
    for (; i < n && s[i]; i++) d[i] = s[i];
    for (; i < n; i++) d[i] = 0;
    return d;
}

char *strcat(char *d, const char *s)
{
    strcpy(d + strlen(d), s);
    return d;
}

char *strchr(const char *s, int c)
{
    for (;; s++) {
        if (*s == (char)c) return (char *)s;
        if (!*s) return NULL;
    }
}

char *strrchr(const char *s, int c)
{
    const char *r = NULL;
    for (;; s++) {
        if (*s == (char)c) r = s;
        if (!*s) return (char *)r;
    }
}

char *strstr(const char *h, const char *n)
{
    size_t l = strlen(n);
    if (!l) return (char *)h;
    for (; *h; h++)
        if (*h == *n && !strncmp(h, n, l))
            return (char *)h;
    return NULL;
}

size_t strspn(const char *s, const char *a)
{
    size_t n = 0;
    while (s[n] && strchr(a, s[n])) n++;
    return n;
}

size_t strcspn(const char *s, const char *r)
{
    size_t n = 0;
    while (s[n] && !strchr(r, s[n])) n++;
    return n;
}

char *strpbrk(const char *s, const char *a)
{
    s += strcspn(s, a);
    return *s ? (char *)s : NULL;
}

char *strdup(const char *s)
{
    size_t n = strlen(s) + 1;
    char *p = malloc(n);
    if (p) memcpy(p, s, n);
    return p;
}

char *strerror(int e)
{
    switch (e) {
    case 0: return "no error";
    case ENOENT: return "no such file or directory";
    case EIO: return "input/output error";
    case ENOMEM: return "out of memory";
    case EISDIR: return "is a directory";
    case ENOTDIR: return "not a directory";
    case EINVAL: return "invalid argument";
    case EROFS: return "read-only file system";
    case ERANGE: return "result too large";
    default: return "error";
    }
}
