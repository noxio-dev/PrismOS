/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * Freestanding mini libc for PrismOS user programs.
 */
#include "prism.h"

char **environ;
static unsigned long *auxv;

extern int main(int argc, char **argv, char **envp);

/* Called by _start with a pointer to argc on the initial stack. */
__attribute__((noreturn, used)) void __prism_start(long *sp)
{
	int argc = (int)sp[0];
	char **argv = (char **)(sp + 1);
	char **envp = argv + argc + 1;
	char **e = envp;

	while (*e)
		e++;
	auxv = (unsigned long *)(e + 1);
	environ = envp;
	sys_exit(main(argc, argv, envp));
}

unsigned long getauxval(unsigned long type)
{
	for (unsigned long *a = auxv; a && a[0] != AT_NULL; a += 2)
		if (a[0] == type)
			return a[1];
	return 0;
}

/* The compiler may emit calls to these even in freestanding code. */
void *memcpy(void *d, const void *s, size_t n)
{
	unsigned char *dp = d;
	const unsigned char *sp = s;

	while (n--)
		*dp++ = *sp++;
	return d;
}

void *memmove(void *d, const void *s, size_t n)
{
	unsigned char *dp = d;
	const unsigned char *sp = s;

	if (dp < sp) {
		while (n--)
			*dp++ = *sp++;
	} else {
		dp += n;
		sp += n;
		while (n--)
			*--dp = *--sp;
	}
	return d;
}

void *memset(void *d, int c, size_t n)
{
	unsigned char *dp = d;

	while (n--)
		*dp++ = (unsigned char)c;
	return d;
}

int memcmp(const void *a, const void *b, size_t n)
{
	const unsigned char *x = a, *y = b;

	for (; n; n--, x++, y++)
		if (*x != *y)
			return *x - *y;
	return 0;
}

size_t strlen(const char *s)
{
	size_t n = 0;

	while (s[n])
		n++;
	return n;
}

int strcmp(const char *a, const char *b)
{
	while (*a && *a == *b)
		a++, b++;
	return (unsigned char)*a - (unsigned char)*b;
}

unsigned long strtoul(const char *s, char **end, int base)
{
	unsigned long v = 0;

	if (base == 0)
		base = (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) ? 16 : 10;
	if (base == 16 && s[0] == '0' && (s[1] == 'x' || s[1] == 'X'))
		s += 2;
	for (;; s++) {
		int d;

		if (*s >= '0' && *s <= '9')
			d = *s - '0';
		else if (*s >= 'a' && *s <= 'f')
			d = *s - 'a' + 10;
		else if (*s >= 'A' && *s <= 'F')
			d = *s - 'A' + 10;
		else
			break;
		if (d >= base)
			break;
		v = v * base + d;
	}
	if (end)
		*end = (char *)s;
	return v;
}

/* ---- printf ----------------------------------------------------------- */

struct out {
	char *buf;
	size_t size, len;
};

static void put(struct out *o, char c)
{
	if (o->len + 1 < o->size)
		o->buf[o->len] = c;
	o->len++;
}

static void put_num(struct out *o, unsigned long long v, unsigned base, int upper,
    int neg, int width, int zero, int left)
{
	char tmp[24];
	int n = 0;
	const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";

	do {
		tmp[n++] = digits[v % base];
		v /= base;
	} while (v);
	int len = n + (neg ? 1 : 0);

	if (left)
		zero = 0;
	if (neg && zero)
		put(o, '-');
	for (int i = len; !left && i < width; i++)
		put(o, zero ? '0' : ' ');
	if (neg && !zero)
		put(o, '-');
	while (n)
		put(o, tmp[--n]);
	for (int i = len; left && i < width; i++)
		put(o, ' ');
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap)
{
	struct out o = { buf, size, 0 };

	for (; *fmt; fmt++) {
		int width = 0, zero = 0, lng = 0, left = 0;

		if (*fmt != '%') {
			put(&o, *fmt);
			continue;
		}
		fmt++;
		for (;; fmt++) {		/* flags */
			if (*fmt == '0')
				zero = 1;
			else if (*fmt == '-')
				left = 1;
			else
				break;
		}
		while (*fmt >= '0' && *fmt <= '9')
			width = width * 10 + (*fmt++ - '0');
		while (*fmt == 'l')
			lng++, fmt++;
		switch (*fmt) {
		case 's': {
			const char *s = va_arg(ap, const char *);

			if (!s)
				s = "(null)";
			size_t l = strlen(s);

			for (size_t i = l; !left && i < (size_t)width; i++)
				put(&o, ' ');
			while (*s)
				put(&o, *s++);
			for (size_t i = l; left && i < (size_t)width; i++)
				put(&o, ' ');
			break;
		}
		case 'c':
			put(&o, (char)va_arg(ap, int));
			break;
		case 'd':
		case 'i': {
			long long v = lng ? va_arg(ap, long long) : va_arg(ap, int);

			put_num(&o, v < 0 ? -(unsigned long long)v : (unsigned long long)v,
			    10, 0, v < 0, width, zero, left);
			break;
		}
		case 'u':
			put_num(&o, lng ? va_arg(ap, unsigned long long) : va_arg(ap, unsigned),
			    10, 0, 0, width, zero, left);
			break;
		case 'x':
		case 'X':
			put_num(&o, lng ? va_arg(ap, unsigned long long) : va_arg(ap, unsigned),
			    16, *fmt == 'X', 0, width, zero, left);
			break;
		case 'p':
			put(&o, '0');
			put(&o, 'x');
			put_num(&o, (unsigned long)va_arg(ap, void *), 16, 0, 0, 0, 0, 0);
			break;
		case '%':
			put(&o, '%');
			break;
		case 0:
			fmt--;
			break;
		default:
			put(&o, '%');
			put(&o, *fmt);
		}
	}
	if (size)
		buf[o.len < size ? o.len : size - 1] = 0;
	return (int)o.len;
}

int printf(const char *fmt, ...)
{
	char buf[512];
	va_list ap;
	int n;

	va_start(ap, fmt);
	n = vsnprintf(buf, sizeof buf, fmt, ap);
	va_end(ap);
	if (n >= (int)sizeof buf)
		n = sizeof buf - 1;
	sys_write(1, buf, n);
	return n;
}
