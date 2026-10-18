/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include "efi.h"
#include "../loader/fs.h"
#include "../loader/fsback.h"

/*
 * UEFI boot volume discovery.  The boot tree lives on the ext4 root
 * partition (P3): every whole disk that exposes a block I/O protocol is
 * scanned, its GPT parsed, and each Linux filesystem partition is offered
 * to the ext4 driver until one carries the boot tree (it has /boot/
 * loader.conf or /boot/kernel/kernel -- plain ext4 data disks do not).
 *
 * When nothing matches (legacy single-volume images, the ISO with its el
 * Torito FAT), fall back to the simple file system of the volume this
 * application was started from.
 */

extern EFI_HANDLE efi_image;
extern EFI_SYSTEM_TABLE *efi_st;
extern EFI_BOOT_SERVICES *efi_bs;

/* ---- ext4 path: whole-disk block I/O ----------------------------------- */

static EFI_BLOCK_IO *bio;           /* the disk the ext4 volume was found on */
static uint8_t *bounce;              /* 64 KiB page-aligned ReadBlocks buffer */
#define BOUNCE_SECTORS (64 * 1024 / 512)

int blk_read(uint64_t lba, uint32_t count, void *buf)
{
    uint8_t *dst = buf;
    if (!bio || !bounce || bio->media->io_align > 4096) return -1;
    while (count) {
        uint32_t n = count > BOUNCE_SECTORS ? BOUNCE_SECTORS : count;
        if (bio->read_blocks(bio, bio->media->media_id, lba,
                             (UINTN)n * 512, bounce) != EFI_SUCCESS)
            return -1;
        memcpy(dst, bounce, (size_t)n * 512);
        dst += (size_t)n * 512;
        lba += n;
        count -= n;
    }
    return 0;
}

static const uint8_t linux_guid[16] = {        /* Linux filesystem / data */
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
};

/* does the mounted ext4 volume carry the loader boot tree? */
static int has_boot_tree(void)
{
    int isdir;
    uint64_t size;
    if (ext4_stat("/boot/loader.conf", &isdir, &size) == 0) return 1;
    if (ext4_stat("/boot/kernel/kernel", &isdir, &size) == 0) return 1;
    return 0;
}

/* try one whole disk: GPT Linux partitions, then the bare volume */
static int try_disk(EFI_BLOCK_IO *b)
{
    uint8_t s[512];
    bio = b;
    if (blk_read(1, 1, s) == 0 && !memcmp(s, "EFI PART", 8)) {
        uint64_t elba = *(uint64_t *)(s + 72);
        uint32_t n = *(uint32_t *)(s + 80), esz = *(uint32_t *)(s + 84);
        if (esz >= 128 && esz <= 512) {
            for (uint32_t i = 0; i < n && i < 128; i++) {
                uint64_t off = (uint64_t)i * esz;
                uint8_t e[512];
                const uint8_t *ent;
                if (blk_read(elba + off / 512, 1, e) < 0) break;
                ent = e + off % 512;
                if (memcmp(ent, linux_guid, 16)) continue;
                if (ext4_mount(*(uint64_t *)(ent + 32)) == 0 && has_boot_tree())
                    return 1;
            }
            return 0;               /* valid GPT, no boot tree on it */
        }
    }
    /* no usable GPT: try the bare volume */
    return ext4_mount(0) == 0 && has_boot_tree();
}

static int ext4_fs_init(void)
{
    static const EFI_GUID bio_guid = GUID_BLOCK_IO;
    EFI_HANDLE *handles = NULL;
    UINTN count = 0;
    EFI_PHYSICAL_ADDRESS addr = 0;

    if (efi_bs->allocate_pages(AllocateAnyPages, EfiLoaderData,
                               64 * 1024 / 4096, &addr) != EFI_SUCCESS)
        return -1;
    bounce = (uint8_t *)(uintptr_t)addr;

    if (efi_bs->locate_handle_buffer(ByProtocol, (EFI_GUID *)&bio_guid, NULL,
                                     &count, &handles) != EFI_SUCCESS)
        return -1;
    for (UINTN i = 0; i < count; i++) {
        EFI_BLOCK_IO *b;
        if (efi_bs->handle_protocol(handles[i], (EFI_GUID *)&bio_guid,
                                    (void **)&b) != EFI_SUCCESS || !b || !b->media)
            continue;
        if (b->media->logical_partition || b->media->block_size != 512)
            continue;              /* partition-level handle, or a CD */
        if (try_disk(b)) {
            efi_bs->free_pool(handles);
            return 0;
        }
    }
    efi_bs->free_pool(handles);
    return -1;
}

/* ---- fallback: the simple file system we were started from --------------- */

static EFI_FILE_PROTOCOL *root;

struct legacy_file {
    EFI_FILE_PROTOCOL *h;
    uint64_t size;
};

static int legacy_init(void)
{
    static const EFI_GUID li_guid = GUID_LOADED_IMAGE, fs_guid = GUID_SIMPLE_FS;
    EFI_LOADED_IMAGE_PROTOCOL *li;
    EFI_SIMPLE_FILE_SYSTEM_PROTOCOL *sfs;

    if (efi_bs->handle_protocol(efi_image, (EFI_GUID *)&li_guid, (void **)&li) != EFI_SUCCESS) return -1;
    if (efi_bs->handle_protocol(li->device_handle, (EFI_GUID *)&fs_guid, (void **)&sfs) != EFI_SUCCESS) return -1;
    return sfs->open_volume(sfs, &root) == EFI_SUCCESS ? 0 : -1;
}

static int status_errno(EFI_STATUS s)
{
    if (s == EFI_NOT_FOUND) return ENOENT;
    return EIO;
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

static int legacy_stat(const char *path, int *isdir, uint64_t *size)
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

static int legacy_open(const char *path, fs_file **out)
{
    EFI_FILE_PROTOCOL *h;
    int r = open_path(path, &h);
    if (r < 0) return r;
    EFI_FILE_INFO *i = get_info(h);
    if (!i) { h->close(h); return -EIO; }
    if (i->attribute & EFI_FILE_DIRECTORY) { free(i); h->close(h); return -EISDIR; }
    struct legacy_file *f = malloc(sizeof *f);
    if (!f) { free(i); h->close(h); return -ENOMEM; }
    f->h = h;
    f->size = i->file_size;
    free(i);
    *out = (fs_file *)f;
    return 0;
}

static void legacy_close(fs_file *ff)
{
    struct legacy_file *f = (struct legacy_file *)ff;
    if (!f) return;
    f->h->close(f->h);
    free(f);
}

static uint64_t legacy_size(fs_file *ff)
{
    return ((struct legacy_file *)ff)->size;
}

static int legacy_seek(fs_file *ff, uint64_t off)
{
    struct legacy_file *f = (struct legacy_file *)ff;
    if (off > f->size) return -EINVAL;
    return f->h->set_position(f->h, off) == EFI_SUCCESS ? 0 : -EIO;
}

static long legacy_read(fs_file *ff, void *buf, size_t n)
{
    struct legacy_file *f = (struct legacy_file *)ff;
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

static int legacy_readdir(const char *path, fs_dir_cb cb, void *arg)
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

/* ---- dispatch -------------------------------------------------------------- */

static int use_ext4;

int fs_init(void)
{
    if (ext4_fs_init() == 0) {
        use_ext4 = 1;
        return 0;
    }
    use_ext4 = 0;
    return legacy_init();
}

int fs_stat(const char *path, int *isdir, uint64_t *size)
{
    return use_ext4 ? ext4_stat(path, isdir, size)
                    : legacy_stat(path, isdir, size);
}

int fs_open(const char *path, fs_file **out)
{
    return use_ext4 ? ext4_open(path, out) : legacy_open(path, out);
}

long fs_read(fs_file *f, void *buf, size_t n)
{
    return use_ext4 ? ext4_read(f, buf, n) : legacy_read(f, buf, n);
}

int fs_seek(fs_file *f, uint64_t off)
{
    return use_ext4 ? ext4_seek(f, off) : legacy_seek(f, off);
}

uint64_t fs_size(fs_file *f)
{
    return use_ext4 ? ext4_size(f) : legacy_size(f);
}

void fs_close(fs_file *f)
{
    if (use_ext4) ext4_close(f);
    else legacy_close(f);
}

int fs_readdir(const char *path, fs_dir_cb cb, void *arg)
{
    return use_ext4 ? ext4_readdir(path, cb, arg) : legacy_readdir(path, cb, arg);
}
