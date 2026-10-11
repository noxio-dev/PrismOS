/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdlib.h>
#include <string.h>
#include "platform.h"

struct var { struct var *next; char *name, *value; };
static struct var *vars;

static struct var *find(const char *n)
{
    for (struct var *v = vars; v; v = v->next)
        if (!strcmp(v->name, n)) return v;
    return NULL;
}

const char *env_get(const char *n) { struct var *v = find(n); return v ? v->value : NULL; }

int env_set(const char *n, const char *val, int overwrite)
{
    struct var *v = find(n);
    char *nv = strdup(val);
    if (!nv) return -1;
    if (v) {
        if (!overwrite) { free(nv); return 0; }
        free(v->value); v->value = nv;
        return 0;
    }
    v = malloc(sizeof *v);
    if (!v) { free(nv); return -1; }
    v->name = strdup(n); v->value = nv; v->next = NULL;
    if (!v->name) { free(nv); free(v); return -1; }
    struct var **pp = &vars;
    while (*pp) pp = &(*pp)->next;
    *pp = v;
    return 0;
}

void env_unset(const char *n)
{
    for (struct var **pp = &vars; *pp; pp = &(*pp)->next)
        if (!strcmp((*pp)->name, n)) {
            struct var *v = *pp;
            *pp = v->next;
            free(v->name); free(v->value); free(v);
            return;
        }
}

void env_each(void (*cb)(const char *, const char *, void *), void *arg)
{
    for (struct var *v = vars; v; v = v->next) cb(v->name, v->value, arg);
}
