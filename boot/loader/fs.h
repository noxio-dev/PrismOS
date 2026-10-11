/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#ifndef LOADER_FS_H
#define LOADER_FS_H

#include <stddef.h>
#include <stdint.h>

/*
 * Minimal read-only filesystem API implemented by each platform backend
 * (FAT32 over BIOS int 13h, or the EFI simple file system protocol).
 * Paths are absolute, '/' separated and case-insensitive (FAT).
 * Functions return 0 or a positive count on success, -errno on failure.
 */
typedef struct fs_file fs_file;

typedef int (*fs_dir_cb)(const char *name, int isdir, uint64_t size, void *arg);

int  fs_init(void);                                /* mount the boot volume */
int  fs_open(const char *path, fs_file **out);
long fs_read(fs_file *f, void *buf, size_t n);
int  fs_seek(fs_file *f, uint64_t off);
uint64_t fs_size(fs_file *f);
void fs_close(fs_file *f);
int  fs_stat(const char *path, int *isdir, uint64_t *size);
int  fs_readdir(const char *path, fs_dir_cb cb, void *arg);

/* read a whole file into a malloc'ed, NUL terminated buffer */
int  fs_readall(const char *path, char **buf, size_t *len);

#endif
