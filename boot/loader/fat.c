/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/*
 * Read-only FAT32 driver with long file name support, used by the BIOS
 * loader over the blk_read() block layer (see bios/disk.c).  The ext4
 * backend (ext4.c) sits next to it; the UEFI loader uses the firmware's
 * file system protocol instead.
 */
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include "fs.h"
#include "fsback.h"

static struct {
    int mounted;
    uint64_t base;
    uint32_t spc, fat_lba, data_lba, root, clusters, bpc;
    uint32_t fatsec;
    uint8_t fatbuf[512];
} fat;

struct fs_file {
    uint32_t first, size, pos, idx, cur;     /* idx: cluster index of cur */
};

static uint16_t rd16(const uint8_t *p) { return p[0] | (p[1] << 8); }
static uint32_t rd32(const uint8_t *p) { return p[0] | (p[1] << 8) | (p[2] << 16) | ((uint32_t)p[3] << 24); }

int fat_mount(uint64_t part_lba)
{
    uint8_t s[512];
    if (blk_read(part_lba, 1, s) < 0) return -EIO;
    if (s[510] != 0x55 || s[511] != 0xAA || rd16(s + 11) != 512 || rd16(s + 17) != 0 ||
        rd16(s + 22) != 0 || !rd32(s + 36))
        return -EINVAL;
    fat.base = part_lba;
    fat.spc = s[13];
    fat.bpc = fat.spc * 512;
    fat.fat_lba = rd16(s + 14);
    fat.data_lba = fat.fat_lba + s[16] * rd32(s + 36);
    fat.root = rd32(s + 44);
    uint32_t total = rd32(s + 32);
    if (!fat.spc || !total || total < fat.data_lba) return -EINVAL;
    fat.clusters = (total - fat.data_lba) / fat.spc;
    if (fat.clusters < 65525) return -EINVAL;        /* FAT12/16 */
    fat.fatsec = ~0u;
    fat.mounted = 1;
    return 0;
}

static uint32_t next_cluster(uint32_t c)
{
    uint32_t sec = (c * 4) / 512;
    if (sec != fat.fatsec) {
        if (blk_read(fat.base + fat.fat_lba + sec, 1, fat.fatbuf) < 0) return 0x0FFFFFFF;
        fat.fatsec = sec;
    }
    return rd32(fat.fatbuf + (c * 4) % 512) & 0x0FFFFFFF;
}

static int valid(uint32_t c) { return c >= 2 && c < 0x0FFFFFF8; }
static uint64_t clba(uint32_t c) { return fat.base + fat.data_lba + (uint64_t)(c - 2) * fat.spc; }

/* ---- directories ------------------------------------------------------ */

struct dent { char name[256]; uint32_t cluster, size; int isdir; };

static uint8_t sfn_sum(const uint8_t *n)
{
    uint8_t s = 0;
    for (int i = 0; i < 11; i++) s = ((s & 1) << 7) + (s >> 1) + n[i];
    return s;
}

/* cb returns non-zero to stop; dir_walk returns that value, or 0 at the end */
static int dir_walk(uint32_t cluster, int (*cb)(struct dent *, void *), void *arg)
{
    static const uint8_t lfn_off[13] = { 1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30 };
    uint16_t lfn[20 * 13 + 1];
    int lfn_ok = 0;
    uint8_t cks = 0;
    uint8_t *buf = malloc(fat.bpc);
    struct dent *d = malloc(sizeof *d);
    int ret = 0;

    if (!buf || !d) { free(buf); free(d); return -ENOMEM; }
    for (; valid(cluster); cluster = next_cluster(cluster)) {
        if (blk_read(clba(cluster), fat.spc, buf) < 0) { ret = -EIO; goto out; }
        for (uint32_t o = 0; o < fat.bpc; o += 32) {
            const uint8_t *e = buf + o;
            if (e[0] == 0) goto out;
            if (e[0] == 0xE5) { lfn_ok = 0; continue; }
            if (e[11] == 0x0F) {
                int ord = e[0] & 0x1F;
                if (e[0] & 0x40) { memset(lfn, 0xFF, sizeof lfn); lfn_ok = 1; cks = e[13]; }
                if (!lfn_ok || ord < 1 || ord > 20 || e[13] != cks) { lfn_ok = 0; continue; }
                for (int i = 0; i < 13; i++) lfn[(ord - 1) * 13 + i] = rd16(e + lfn_off[i]);
                continue;
            }
            if (e[11] & 0x08) { lfn_ok = 0; continue; }       /* volume label */
            int n = 0;
            if (lfn_ok && sfn_sum(e) == cks) {
                for (int i = 0; i < 20 * 13 && lfn[i] != 0 && lfn[i] != 0xFFFF && n < 255; i++)
                    d->name[n++] = lfn[i] < 128 ? (char)lfn[i] : '?';
            } else {
                int lb = e[12] & 0x08, le = e[12] & 0x10;
                for (int i = 0; i < 8 && e[i] != ' '; i++) {
                    char c = e[i];
                    d->name[n++] = (lb && c >= 'A' && c <= 'Z') ? c + 32 : c;
                }
                if (e[8] != ' ') {
                    d->name[n++] = '.';
                    for (int i = 8; i < 11 && e[i] != ' '; i++) {
                        char c = e[i];
                        d->name[n++] = (le && c >= 'A' && c <= 'Z') ? c + 32 : c;
                    }
                }
            }
            d->name[n] = 0;
            lfn_ok = 0;
            if (d->name[0] == '.' && (n == 1 || (n == 2 && d->name[1] == '.'))) continue;
            d->cluster = (rd16(e + 20) << 16) | rd16(e + 26);
            d->size = rd32(e + 28);
            d->isdir = (e[11] & 0x10) != 0;
            if ((ret = cb(d, arg)) != 0) goto out;
        }
    }
out:
    free(buf); free(d);
    return ret;
}

struct find { const char *name; struct dent hit; int found; };

static int find_cb(struct dent *d, void *arg)
{
    struct find *f = arg;
    if (strcasecmp(d->name, f->name)) return 0;
    f->hit = *d; f->found = 1;
    return 1;
}

/* resolve path to a directory entry; the root is reported as cluster root */
static int lookup(const char *path, struct dent *out)
{
    char comp[256];
    struct dent cur = { "", fat.root, 0, 1 };

    if (!fat.mounted) return -EIO;
    while (*path) {
        while (*path == '/') path++;
        if (!*path) break;
        size_t n = 0;
        while (path[n] && path[n] != '/') n++;
        if (n >= sizeof comp) return -ENOENT;
        memcpy(comp, path, n); comp[n] = 0;
        path += n;
        if (!strcmp(comp, ".")) continue;
        if (!cur.isdir) return -ENOTDIR;
        struct find f = { comp, { "" }, 0 };
        int r = dir_walk(cur.cluster, find_cb, &f);
        if (r < 0) return r;
        if (!f.found) return -ENOENT;
        cur = f.hit;
    }
    *out = cur;
    return 0;
}

/* ---- fs.h (dispatched by bios/disk.c) -------------------------------- */

int fat_stat(const char *path, int *isdir, uint64_t *size)
{
    struct dent d;
    int r = lookup(path, &d);
    if (r < 0) return r;
    *isdir = d.isdir;
    *size = d.isdir ? 0 : d.size;
    return 0;
}

int fat_open(const char *path, fs_file **out)
{
    struct dent d;
    int r = lookup(path, &d);
    if (r < 0) return r;
    if (d.isdir) return -EISDIR;
    fs_file *f = calloc(1, sizeof *f);
    if (!f) return -ENOMEM;
    f->first = f->cur = d.cluster;
    f->size = d.size;
    *out = f;
    return 0;
}

void fat_close(fs_file *f) { free(f); }
uint64_t fat_size(fs_file *f) { return f->size; }

int fat_seek(fs_file *f, uint64_t off)
{
    if (off > f->size) return -EINVAL;
    f->pos = (uint32_t)off;
    return 0;
}

/* position f->cur on the cluster holding f->pos */
static int goto_cluster(fs_file *f)
{
    uint32_t want = f->pos / fat.bpc;
    if (want < f->idx || !valid(f->cur)) { f->idx = 0; f->cur = f->first; }
    while (f->idx < want) {
        f->cur = next_cluster(f->cur);
        f->idx++;
        if (!valid(f->cur)) return -EIO;
    }
    return valid(f->cur) ? 0 : -EIO;
}

long fat_read(fs_file *f, void *buf, size_t n)
{
    uint8_t *dst = buf, sec[512];
    size_t total = 0;

    if (f->pos + (uint64_t)n > f->size) n = f->size - f->pos;
    while (n) {
        if (goto_cluster(f) < 0) return total ? (long)total : -EIO;
        uint32_t off = f->pos % fat.bpc;
        if (off == 0 && n >= fat.bpc) {
            /* whole clusters: read the longest physically contiguous run */
            uint32_t k = 1, c = f->cur;
            while ((k + 1) * (size_t)fat.bpc <= n && next_cluster(c) == c + 1) { c++; k++; }
            if (blk_read(clba(f->cur), k * fat.spc, dst) < 0) return total ? (long)total : -EIO;
            f->cur = c; f->idx += k - 1;
            size_t bytes = (size_t)k * fat.bpc;
            dst += bytes; total += bytes; n -= bytes; f->pos += bytes;
        } else {
            if (blk_read(clba(f->cur) + off / 512, 1, sec) < 0) return total ? (long)total : -EIO;
            size_t chunk = 512 - off % 512;
            if (chunk > n) chunk = n;
            memcpy(dst, sec + off % 512, chunk);
            dst += chunk; total += chunk; n -= chunk; f->pos += chunk;
        }
    }
    return (long)total;
}

struct rd { fs_dir_cb cb; void *arg; };

static int rd_cb(struct dent *d, void *arg)
{
    struct rd *r = arg;
    return r->cb(d->name, d->isdir, d->isdir ? 0 : d->size, r->arg);
}

int fat_readdir(const char *path, fs_dir_cb cb, void *arg)
{
    struct dent d;
    int r = lookup(path, &d);
    if (r < 0) return r;
    if (!d.isdir) return -ENOTDIR;
    struct rd rd = { cb, arg };
    r = dir_walk(d.cluster, rd_cb, &rd);
    return r < 0 ? r : 0;
}
