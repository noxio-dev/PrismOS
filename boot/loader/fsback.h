/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#ifndef LOADER_FSBACK_H
#define LOADER_FSBACK_H

#include "fs.h"

/*
 * Filesystem backends shared by the BIOS and UEFI loaders.  The platform
 * disk layer scans the GPT/MBR, tries to mount each candidate partition
 * (the Linux ext2/3/4 root first, then the FAT32 ESP for legacy layouts)
 * and dispatches the fs.h API to whichever backend mounted.
 *
 * Both backends define their own private `struct fs_file` layout: the tag
 * has no linkage outside its translation unit, so this works with a single
 * opaque fs_file type in fs.h -- the dispatcher only ever passes pointers.
 */

/* Block layer (bios/disk.c over int 13h, efi/efi_fs.c over block I/O):
 * LBAs are absolute disk sectors */
int blk_read(uint64_t lba, uint32_t count, void *buf);

/* fat.c - read-only FAT32 with long file name support */
int  fat_mount(uint64_t part_lba);
int  fat_stat(const char *path, int *isdir, uint64_t *size);
int  fat_open(const char *path, fs_file **out);
long fat_read(fs_file *f, void *buf, size_t n);
int  fat_seek(fs_file *f, uint64_t off);
uint64_t fat_size(fs_file *f);
void fat_close(fs_file *f);
int  fat_readdir(const char *path, fs_dir_cb cb, void *arg);

/* ext4.c - read-only ext2/3/4 */
int  ext4_mount(uint64_t part_lba);
int  ext4_stat(const char *path, int *isdir, uint64_t *size);
int  ext4_open(const char *path, fs_file **out);
long ext4_read(fs_file *f, void *buf, size_t n);
int  ext4_seek(fs_file *f, uint64_t off);
uint64_t ext4_size(fs_file *f);
void ext4_close(fs_file *f);
int  ext4_readdir(const char *path, fs_dir_cb cb, void *arg);

#endif
