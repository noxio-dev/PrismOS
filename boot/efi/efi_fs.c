/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include "efi.h"
#include "../loader/fs.h"

extern EFI_HANDLE efi_image;
extern EFI_SYSTEM_TABLE *efi_st;
extern EFI_BOOT_SERVICES *efi_bs;

static EFI_FILE_PROTOCOL *root;

struct fs_file {
    EFI_FILE_PROTOCOL *h;
    uint64_t size;
};

static int status_errno(EFI_STATUS s)
{
    if (s == EFI_NOT_FOUND) return ENOENT;
    return EIO;
}

int fs_init(void)
{
    static const EFI_GUID li_guid = GUID_LOADED_IMAGE, fs_guid = GUID_SIMPLE_FS;
    EFI_LOADED_IMAGE_PROTOCOL *li;
    EFI_SIMPLE_FILE_SYSTEM_PROTOCOL *sfs;

    if (efi_bs->handle_protocol(efi_image, (EFI_GUID *)&li_guid, (void **)&li) != EFI_SUCCESS) return -1;
    if (efi_bs->handle_protocol(li->device_handle, (EFI_GUID *)&fs_guid, (void **)&sfs) != EFI_SUCCESS) return -1;
    return sfs->open_volume(sfs, &root) == EFI_SUCCESS ? 0 : -1;
}

/* '/' separated ASCII path -> backslash separated UCS-2, no leading slash */
static CHAR16 *to_wide(const char *path)
{
    size_t n = strlen(path);
    CHAR16 *w = malloc((n + 2) * sizeof *w);
    size_t o = 0;
    if (!w) return NULL;
    while (*path == '/') path++;
    for (; *path; path++) {
        if (*path == '/') {
            if (o && w[o - 1] != '\\') w[o++] = '\\';
        } else w[o++] = (unsigned char)*path;
    }
    while (o && w[o - 1] == '\\') o--;
    if (!o) w[o++] = '.';                 /* the root itself */
    w[o] = 0;
    return w;
}

static EFI_FILE_INFO *get_info(EFI_FILE_PROTOCOL *f)
{
    static const EFI_GUID guid = GUID_FILE_INFO;
    UINTN size = 512;
    for (int i = 0; i < 4; i++) {
        EFI_FILE_INFO *info = malloc(size);
        if (!info) return NULL;
        EFI_STATUS s = f->get_info(f, (EFI_GUID *)&guid, &size, info);
        if (s == EFI_SUCCESS) return info;
        free(info);
        if (s != EFI_BUFFER_TOO_SMALL) return NULL;
    }
    return NULL;
}

static int open_path(const char *path, EFI_FILE_PROTOCOL **out)
{
    CHAR16 *w = to_wide(path);
    if (!w) return -ENOMEM;
    EFI_STATUS s = root->open(root, out, w, EFI_FILE_MODE_READ, 0);
    free(w);
    return s == EFI_SUCCESS ? 0 : -status_errno(s);
}

int fs_stat(const char *path, int *isdir, uint64_t *size)
{
    EFI_FILE_PROTOCOL *f;
    int r = open_path(path, &f);
    if (r < 0) return r;
    EFI_FILE_INFO *i = get_info(f);
    f->close(f);
    if (!i) return -EIO;
    *isdir = (i->attribute & EFI_FILE_DIRECTORY) != 0;
    *size = *isdir ? 0 : i->file_size;
    free(i);
    return 0;
}

int fs_open(const char *path, fs_file **out)
{
    EFI_FILE_PROTOCOL *h;
    int r = open_path(path, &h);
    if (r < 0) return r;
    EFI_FILE_INFO *i = get_info(h);
    if (!i) { h->close(h); return -EIO; }
    if (i->attribute & EFI_FILE_DIRECTORY) { free(i); h->close(h); return -EISDIR; }
    fs_file *f = malloc(sizeof *f);
    if (!f) { free(i); h->close(h); return -ENOMEM; }
    f->h = h;
    f->size = i->file_size;
    free(i);
    *out = f;
    return 0;
}

void fs_close(fs_file *f)
{
    if (!f) return;
    f->h->close(f->h);
    free(f);
}

uint64_t fs_size(fs_file *f) { return f->size; }

int fs_seek(fs_file *f, uint64_t off)
{
    if (off > f->size) return -EINVAL;
    return f->h->set_position(f->h, off) == EFI_SUCCESS ? 0 : -EIO;
}

long fs_read(fs_file *f, void *buf, size_t n)
{
    uint8_t *p = buf;
    size_t total = 0;
    while (n) {
        UINTN chunk = n > (1u << 20) ? (1u << 20) : n;
        if (f->h->read(f->h, &chunk, p) != EFI_SUCCESS) return total ? (long)total : -EIO;
        if (!chunk) break;
        p += chunk; total += chunk; n -= chunk;
    }
    return (long)total;
}

int fs_readdir(const char *path, fs_dir_cb cb, void *arg)
{
    EFI_FILE_PROTOCOL *d;
    int r = open_path(path, &d);
    if (r < 0) return r;
    EFI_FILE_INFO *probe = get_info(d);
    if (!probe || !(probe->attribute & EFI_FILE_DIRECTORY)) { free(probe); d->close(d); return -ENOTDIR; }
    free(probe);

    UINTN cap = 1024;
    uint8_t *buf = malloc(cap);
    if (!buf) { d->close(d); return -ENOMEM; }
    for (;;) {
        UINTN size = cap;
        EFI_STATUS s = d->read(d, &size, buf);
        if (s == EFI_BUFFER_TOO_SMALL) {
            free(buf);
            cap = size;
            buf = malloc(cap);
            if (!buf) { d->close(d); return -ENOMEM; }
            continue;
        }
        if (s != EFI_SUCCESS) { r = -EIO; break; }
        if (!size) break;
        EFI_FILE_INFO *i = (EFI_FILE_INFO *)buf;
        char name[256];
        size_t k = 0;
        for (; i->file_name[k] && k < sizeof name - 1; k++)
            name[k] = i->file_name[k] < 128 ? (char)i->file_name[k] : '?';
        name[k] = 0;
        if (!strcmp(name, ".") || !strcmp(name, "..")) continue;
        int isdir = (i->attribute & EFI_FILE_DIRECTORY) != 0;
        if (cb(name, isdir, isdir ? 0 : i->file_size, arg)) break;
    }
    free(buf);
    d->close(d);
    return r;
}
