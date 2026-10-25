/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/*
 * Read-only ext2/3/4 driver for the BIOS loader: the same on-disk format
 * support as the kernel driver (sys/kern/src/fs/ext4), trimmed to what a
 * boot loader needs -- no checksums, no journal, no writes.
 *
 *   * reading: extents or classic block maps, 32/64-bit group descriptors,
 *     flex_bg, uninit_bg / metadata_csum volumes (checksums are not
 *     verified on read), htree directories (scanned linearly), fast and
 *     slow symlinks, sparse files (holes read as zeros).
 *   * refused at mount: inline_data, bigalloc, meta_bg, encryption,
 *     casefold, MMP, EA inodes -- the same set as the kernel driver.
 *
 * Path lookup matches names exactly first, then falls back to a
 * case-insensitive comparison (the boot tree is lowercase anyway; this
 * just makes uppercase typos survive).
 */
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <errno.h>
#include "fs.h"
#include "fsback.h"

#define EXT4_MAGIC      0xEF53
#define EXT4_ROOT_INO   2
#define EXT_MAGIC       0xF30A
#define EXT_INIT_MAX    32768          /* ee_len high bit marks uninit */
#define MAX_LINKS       8              /* symlink chains */
#define MAX_COMPONENTS  32             /* path depth */

/* s_feature_incompat */
#define INCOMPAT_FILETYPE   0x0002
#define INCOMPAT_RECOVER    0x0004
#define INCOMPAT_EXTENTS    0x0040
#define INCOMPAT_64BIT      0x0080
#define INCOMPAT_FLEX_BG    0x0200
#define INCOMPAT_CSUM_SEED  0x2000
#define INCOMPAT_LARGEDIR   0x4000
#define INCOMPAT_SUPPORTED  (INCOMPAT_FILETYPE | INCOMPAT_RECOVER | INCOMPAT_EXTENTS | \
                             INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED | \
                             INCOMPAT_LARGEDIR)

/* s_feature_ro_compat */
#define RO_BIGALLOC         0x0200

/* inode flags */
#define EXT4_HUGE_FILE_FL   0x00040000
#define EXT4_EXTENTS_FL     0x00080000

/* i_mode */
#define S_IFMT          0xF000
#define S_IFREG         0x8000
#define S_IFDIR         0x4000
#define S_IFLNK         0xA000

/* directory entry file types */
#define FT_REG          1
#define FT_DIR          2
#define FT_SYMLINK      7

static struct {
    int mounted;
    uint64_t base;              /* LBA of the partition start */
    uint32_t bs;                /* filesystem block size */
    uint32_t spb;               /* 512-byte sectors per block */
    uint64_t blocks;            /* filesystem blocks */
    uint32_t inodes, ipg;
    uint32_t inode_size;
    uint16_t desc_size;
    uint32_t groups;
    uint32_t first_data_block;
    int filetype;               /* INCOMPAT_FILETYPE */
    int wide;                   /* INCOMPAT_64BIT: high halves in the descriptors */
    uint64_t gdt_blk;           /* first group descriptor block */
    /* one-block caches; boot reads are mostly sequential */
    uint8_t *gbuf;              /* group descriptor table block */
    uint64_t gbuf_blk;
    uint8_t *ibuf;              /* inode table block */
    uint64_t ibuf_blk;
    uint8_t *tmp;               /* scratch block for unaligned reads */
} e4;

struct inode {
    uint32_t ino;
    uint16_t mode;
    uint32_t flags;
    uint64_t size;              /* directories keep the low 32 bits only */
    uint64_t blocks512;         /* i_blocks, 512-byte units */
    uint64_t file_acl;
    uint8_t block[60];          /* i_block[]: pointers, extents, fast symlink */
};

/* mapping of a file: sorted, non-overlapping extents */
struct ext { uint32_t lblk, len; uint64_t pblk; int uninit; };

struct fs_file {
    uint64_t size, pos;
    struct ext *exts;
    int nexts;
};

static uint16_t rd16(const uint8_t *p) { return p[0] | (p[1] << 8); }
static uint32_t rd32(const uint8_t *p) { return p[0] | (p[1] << 8) | (p[2] << 16) | ((uint32_t)p[3] << 24); }

static int bread(uint64_t blk, void *buf)
{
    if (blk >= e4.blocks) return -EIO;
    return blk_read(e4.base + blk * e4.spb, e4.spb, buf) < 0 ? -EIO : 0;
}

/* ---- group descriptors ------------------------------------------------ */

static const uint8_t *gd_get(uint32_t g)
{
    uint64_t byte = (uint64_t)g * e4.desc_size;
    uint64_t blk = e4.gdt_blk + byte / e4.bs;
    uint32_t off = byte % e4.bs;
    if (blk >= e4.blocks || off + e4.desc_size > e4.bs) return NULL;
    if (blk != e4.gbuf_blk) {
        if (bread(blk, e4.gbuf) < 0) return NULL;
        e4.gbuf_blk = blk;
    }
    return e4.gbuf + off;
}

static uint64_t gd_inode_table(uint32_t g)
{
    const uint8_t *d = gd_get(g);
    if (!d) return 0;
    return rd32(d + 8) | (e4.wide ? (uint64_t)rd16(d + 40) << 32 : 0);   /* 64-bit high */
}

/* ---- inodes ------------------------------------------------------------- */

static int read_inode(uint32_t ino, struct inode *out)
{
    if (!ino || ino > e4.inodes) return -EINVAL;
    uint32_t g = (ino - 1) / e4.ipg;
    uint64_t idx = (ino - 1) % e4.ipg;
    uint64_t itable = gd_inode_table(g);
    uint64_t byte = idx * e4.inode_size;
    uint64_t blk = itable + byte / e4.bs;
    uint32_t off = byte % e4.bs;
    if (!itable || off + e4.inode_size > e4.bs || blk >= e4.blocks) return -EIO;
    if (blk != e4.ibuf_blk) {
        if (bread(blk, e4.ibuf) < 0) return -EIO;
        e4.ibuf_blk = blk;
    }
    const uint8_t *p = e4.ibuf + off;
    memset(out, 0, sizeof *out);
    out->ino = ino;
    out->mode = rd16(p);
    out->flags = rd32(p + 0x20);
    out->size = rd32(p + 4);
    if ((out->mode & S_IFMT) == S_IFREG)                    /* i_size_high */
        out->size |= (uint64_t)rd32(p + 0x6C) << 32;
    out->blocks512 = rd32(p + 0x1C);
    if (out->flags & EXT4_HUGE_FILE_FL)
        out->blocks512 |= (uint64_t)rd16(p + 0x74) << 32;
    out->file_acl = rd32(p + 0x68);
    memcpy(out->block, p + 0x28, 60);
    return 0;
}

/* the target lives in i_block[] when no data block backs it */
static int symlink_is_fast(const struct inode *i)
{
    if ((i->mode & S_IFMT) != S_IFLNK || (i->flags & EXT4_EXTENTS_FL) || i->size >= 60)
        return 0;
    return i->blocks512 == (i->file_acl ? e4.spb : 0);
}

/* ---- extent lists (extent trees and classic block maps) ----------------- */

static int ext_push(struct ext **exts, int *n, int *cap,
                    uint32_t lblk, uint32_t len, uint64_t pblk, int uninit)
{
    struct ext *last = *n ? &(*exts)[*n - 1] : NULL;
    if (last && (uint64_t)last->lblk + last->len > lblk) return -EIO;   /* out of order */
    if (last && last->uninit == uninit && (uint64_t)last->lblk + last->len == lblk &&
        last->pblk + last->len == pblk && (uint64_t)last->len + len <= EXT_INIT_MAX) {
        last->len += len;
        return 0;
    }
    if (*n == *cap) {
        int nc = *cap ? *cap * 2 : 16;
        struct ext *t = realloc(*exts, (size_t)nc * sizeof *t);
        if (!t) return -ENOMEM;
        *exts = t;
        *cap = nc;
    }
    (*exts)[*n] = (struct ext){ lblk, len, pblk, uninit };
    (*n)++;
    return 0;
}

static int walk_node(const uint8_t *node, size_t len, struct ext **exts, int *n, int *cap,
                     int depth_left)
{
    if (len < 12 || rd16(node) != EXT_MAGIC) return -EIO;
    uint32_t entries = rd16(node + 2), max = rd16(node + 4), depth = rd16(node + 6);
    if (entries > max || 12 + 12ull * entries > len || depth > (uint32_t)depth_left)
        return -EIO;
    for (uint32_t i = 0; i < entries; i++) {
        const uint8_t *x = node + 12 + 12 * i;
        int r;
        if (depth == 0) {
            uint32_t lblk = rd32(x);
            uint32_t raw = rd16(x + 4);
            uint64_t pblk = rd32(x + 8) | ((uint64_t)rd16(x + 6) << 32);
            int uninit = raw > EXT_INIT_MAX;
            uint32_t elen = uninit ? raw - EXT_INIT_MAX : raw;
            if (!elen || pblk + elen > e4.blocks) return -EIO;
            r = ext_push(exts, n, cap, lblk, elen, pblk, uninit);
        } else {
            uint64_t child = rd32(x + 4) | ((uint64_t)rd16(x + 8) << 32);
            if (child >= e4.blocks) return -EIO;
            uint8_t *b = malloc(e4.bs);
            if (!b) return -ENOMEM;
            r = bread(child, b) < 0 ? -EIO
                : walk_node(b, e4.bs, exts, n, cap, depth_left - 1);
            free(b);
        }
        if (r < 0) return r;
    }
    return 0;
}

static int walk_indirect(uint64_t blk, int level, uint64_t base, uint64_t nblk,
                         struct ext **exts, int *n, int *cap)
{
    if (blk >= e4.blocks) return -EIO;
    uint8_t *b = malloc(e4.bs);
    if (!b) return -ENOMEM;
    int r = bread(blk, b);
    if (r == 0) {
        uint32_t ptrs = e4.bs / 4;
        uint64_t child_span = level == 1 ? 1 : (level == 2 ? ptrs : (uint64_t)ptrs * ptrs);
        for (uint32_t i = 0; i < ptrs; i++) {
            uint64_t lb = base + (uint64_t)i * child_span;
            if (lb >= nblk) break;
            uint64_t p = rd32(b + 4 * i);
            if (!p) continue;
            if (level == 1)
                r = ext_push(exts, n, cap, (uint32_t)lb, 1, p, 0);
            else
                r = walk_indirect(p, level - 1, lb, nblk, exts, n, cap);
            if (r < 0) break;
        }
    }
    free(b);
    return r;
}

/* build the whole mapping of an inode; the caller frees *exts */
static int load_tree(const struct inode *i, struct ext **exts, int *n)
{
    int cap = 0, r = 0;
    *exts = NULL;
    *n = 0;
    if (i->flags & EXT4_EXTENTS_FL) {
        r = walk_node(i->block, sizeof i->block, exts, n, &cap, 5);
    } else {
        uint64_t nblk = (i->size + e4.bs - 1) / e4.bs;
        uint64_t ptrs = e4.bs / 4, lblk = 0;
        for (int k = 0; k < 12 && lblk < nblk; k++, lblk++) {
            uint64_t p = rd32(i->block + 4 * k);
            if (p && p < e4.blocks)
                r = ext_push(exts, n, &cap, (uint32_t)lblk, 1, p, 0);
            if (r < 0) break;
        }
        uint64_t spans[3] = { ptrs, ptrs * ptrs, ptrs * ptrs * ptrs };
        for (int level = 1; level <= 3 && lblk < nblk && r == 0; level++) {
            uint64_t p = rd32(i->block + 48 + (level - 1) * 4);
            if (p)
                r = walk_indirect(p, level, lblk, nblk, exts, n, &cap);
            lblk += spans[level - 1];
        }
    }
    if (r < 0) {
        free(*exts);
        *exts = NULL;
        *n = 0;
    }
    return r;
}

/* index of the extent covering lblk, or -1; *next: first extent after it */
static int ext_at(const struct ext *exts, int n, uint32_t lblk, int *next)
{
    int lo = 0, hi = n;
    while (lo < hi) {
        int mid = (lo + hi) / 2;
        if (exts[mid].lblk <= lblk) lo = mid + 1;
        else hi = mid;
    }
    if (next) *next = lo;
    if (!lo) return -1;
    const struct ext *e = &exts[lo - 1];
    return (uint64_t)lblk < (uint64_t)e->lblk + e->len ? lo - 1 : -1;
}

/* ---- file data ------------------------------------------------------------ */

/* read n bytes at pos through an extent list; holes and uninit read as zeros */
static long read_exts(struct ext *exts, int nexts, uint64_t size, uint64_t pos,
                      void *buf, size_t n)
{
    uint8_t *dst = buf;
    size_t total = 0;

    if (pos >= size) return 0;
    if (n > size - pos) n = size - pos;
    while (n) {
        uint32_t lblk = pos / e4.bs;
        uint32_t inblk = pos % e4.bs;
        int nxt, idx = ext_at(exts, nexts, lblk, &nxt);
        if (idx < 0) {                                   /* hole */
            uint64_t hole_end = nxt < nexts ? (uint64_t)exts[nxt].lblk * e4.bs : size;
            size_t take = hole_end - pos < n ? (size_t)(hole_end - pos) : n;
            memset(dst, 0, take);
            dst += take; total += take; n -= take; pos += take;
        } else {
            struct ext e = exts[idx];
            uint64_t avail = ((uint64_t)e.lblk + e.len - lblk) * e4.bs - inblk;
            size_t take = (uint64_t)n < avail ? n : (size_t)avail;
            uint64_t pblk = e.pblk + (lblk - e.lblk);
            if (e.uninit) {
                memset(dst, 0, take);
            } else if (inblk % 512 == 0 && take % 512 == 0) {
                if (blk_read(e4.base + pblk * e4.spb + inblk / 512, take / 512, dst) < 0)
                    return total ? (long)total : -EIO;
            } else {                                     /* unaligned: by block */
                while (take) {
                    if (bread(pblk, e4.tmp) < 0) return total ? (long)total : -EIO;
                    size_t chunk = e4.bs - inblk;
                    if (chunk > take) chunk = take;
                    memcpy(dst, e4.tmp + inblk, chunk);
                    dst += chunk; total += chunk; n -= chunk; pos += chunk; take -= chunk;
                    pblk++;
                    inblk = 0;
                }
                continue;
            }
            dst += take; total += take; n -= take; pos += take;
        }
    }
    return (long)total;
}

/* one-shot read of an inode (symlink targets, small metadata) */
static long data_read(const struct inode *i, uint64_t off, void *buf, size_t n)
{
    struct ext *exts;
    int nexts;
    if (load_tree(i, &exts, &nexts) < 0) return -EIO;
    long r = read_exts(exts, nexts, i->size, off, buf, n);
    free(exts);
    return r;
}

static int read_symlink(const struct inode *i, char *buf, size_t bufsz)
{
    long n;
    if (i->size >= bufsz) return -ENOENT;
    if (symlink_is_fast(i)) {
        memcpy(buf, i->block, i->size);
        n = (long)i->size;
    } else {
        n = data_read(i, 0, buf, i->size);
    }
    if (n < 0) return (int)n;
    buf[n] = 0;
    return 0;
}

/* ---- directories ---------------------------------------------------------- */

struct dent { char name[256]; uint32_t ino; int isdir; };     /* isdir: -1 unknown */

typedef int (*dent_cb)(struct dent *d, void *arg);

/* cb returns non-zero to stop (propagated); 0 at the end of the scan */
static int dir_walk(const struct inode *dir, dent_cb cb, void *arg)
{
    struct ext *exts;
    int nexts, r = load_tree(dir, &exts, &nexts);
    if (r < 0) return r;

    uint64_t nblk = dir->size / e4.bs;          /* dirs are whole blocks */
    uint8_t *buf = malloc(e4.bs);
    struct dent *d = malloc(sizeof *d);
    int ret = 0;
    if (!buf || !d) { ret = -ENOMEM; goto out; }

    for (int i = 0; i < nexts; i++) {
        const struct ext *e = &exts[i];
        if (e->uninit) continue;
        for (uint32_t b = 0; b < e->len; b++) {
            if ((uint64_t)e->lblk + b >= nblk) goto out;
            if (bread(e->pblk + b, buf) < 0) { ret = -EIO; goto out; }
            for (uint32_t off = 0; off + 8 <= e4.bs; ) {
                const uint8_t *de = buf + off;
                uint16_t rlen = rd16(de + 4);
                uint8_t nlen = de[6], ftype = de[7];
                if (rlen < 8 || (rlen & 3) || 8 + (uint32_t)nlen > rlen ||
                    rlen > e4.bs - off)         /* entry must stay inside the block */
                    goto out;                   /* corrupt entry: stop here */
                off += rlen;
                uint32_t ino = rd32(de);
                if (!ino || !nlen) continue;     /* gap or metadata_csum tail */
                if (nlen <= 2 && de[8] == '.') continue;      /* "." and ".." */
                memcpy(d->name, de + 8, nlen);
                d->name[nlen] = 0;
                d->ino = ino;
                d->isdir = e4.filetype ? ftype == FT_DIR : -1;
                if ((ret = cb(d, arg)) != 0) goto out;
            }
        }
    }
out:
    free(buf); free(d); free(exts);
    return ret;
}

struct find { const char *name; uint32_t ino; int found; };

static int find_cb(struct dent *d, void *arg)
{
    struct find *f = arg;
    if (!strcmp(d->name, f->name)) {
        f->ino = d->ino; f->found = 1;
        return 1;                               /* exact match: stop the scan */
    }
    if (!f->found && !strcasecmp(d->name, f->name)) {
        f->ino = d->ino; f->found = 1;          /* remember, keep looking for exact */
    }
    return 0;
}

/* ---- path resolution -------------------------------------------------------- */

/*
 * Resolve `path` starting at the directory stack[depth-1] (the root for
 * absolute paths).  stack[0..depth-1] holds the chain of directories from
 * the root down to the starting one, so ".." climbs back through it ("."
 * is skipped, ".." above the root stays there).  Symlinks are followed: an
 * absolute target restarts at the root, a relative one at the directory
 * holding the link, in both cases with the rest of the path appended.
 */
static int walk_from(struct inode *stack, int depth, const char *path,
                      struct inode *out, int links)
{
    struct inode cur = stack[depth - 1];
    char comp[256], target[512], joined[1536];
    int r;

    if (links > MAX_LINKS) return -EIO;
    while (*path) {
        while (*path == '/') path++;
        if (!*path) break;
        size_t n = 0;
        while (path[n] && path[n] != '/') n++;
        if (n >= sizeof comp) return -ENOENT;
        memcpy(comp, path, n); comp[n] = 0;
        path += n;
        if (!strcmp(comp, ".")) continue;
        if (!strcmp(comp, "..")) {
            if (depth > 1) depth--;            /* one step towards the root */
            cur = stack[depth - 1];
            continue;
        }
        if ((cur.mode & S_IFMT) != S_IFDIR) return -ENOTDIR;

        struct find f = { comp, 0, 0 };
        if ((r = dir_walk(&cur, find_cb, &f)) < 0) return r;
        if (!f.found) return -ENOENT;

        struct inode nxt;
        if ((r = read_inode(f.ino, &nxt)) < 0) return r;

        if ((nxt.mode & S_IFMT) == S_IFLNK) {
            if ((r = read_symlink(&nxt, target, sizeof target)) < 0) return r;
            snprintf(joined, sizeof joined, "%s/%s", target, path);
            if (target[0] == '/') {
                if ((r = read_inode(EXT4_ROOT_INO, &stack[0])) < 0) return r;
                return walk_from(stack, 1, joined, out, links + 1);
            }
            return walk_from(stack, depth, joined, out, links + 1);
        }
        if (depth >= MAX_COMPONENTS) return -EIO;
        cur = stack[depth++] = nxt;              /* descend one directory */
    }
    *out = cur;
    return 0;
}

static int lookup(const char *path, struct inode *out)
{
    struct inode stack[MAX_COMPONENTS];
    int r;
    if (*path != '/') return -EINVAL;
    if ((r = read_inode(EXT4_ROOT_INO, &stack[0])) < 0) return r;
    return walk_from(stack, 1, path, out, 0);
}

/* ---- mount ------------------------------------------------------------------- */

int ext4_mount(uint64_t part_lba)
{
    uint8_t sb[1024];
    struct inode root;
    int r = -EINVAL;

    if (blk_read(part_lba + 2, 2, sb) < 0) return -EIO;
    if (rd16(sb + 0x38) != EXT4_MAGIC) return -EINVAL;
    uint32_t log_bs = rd32(sb + 0x18);
    if (log_bs > 6) return -EINVAL;
    uint32_t bs = 1024u << log_bs;
    uint32_t rev = rd32(sb + 0x4C);
    uint32_t incompat = rev >= 1 ? rd32(sb + 0x60) : 0;
    uint32_t ro_compat = rev >= 1 ? rd32(sb + 0x64) : 0;
    if (incompat & ~INCOMPAT_SUPPORTED) return -EINVAL;         /* we would misread it */
    if (ro_compat & RO_BIGALLOC) return -EINVAL;
    int is64 = (incompat & INCOMPAT_64BIT) != 0;
    uint32_t desc_size = 32;
    if (is64) {
        desc_size = rd16(sb + 0xFE);
        if (desc_size < 64 || desc_size > 1024 || (desc_size & (desc_size - 1)))
            return -EINVAL;
    }

    uint32_t inodes = rd32(sb);
    uint32_t bpg = rd32(sb + 0x20), ipg = rd32(sb + 0x28);
    uint32_t first_data = rd32(sb + 0x14);
    uint32_t inode_size = rev >= 1 ? rd16(sb + 0x58) : 128;
    uint64_t blocks = rd32(sb + 4) | (is64 ? (uint64_t)rd32(sb + 0x150) << 32 : 0);
    if (!bpg || bpg > bs * 8 || !ipg || ipg > bs * 8 || !inode_size ||
        inode_size & (inode_size - 1) || inode_size < 128 || inode_size > bs ||
        blocks <= first_data)
        return -EINVAL;
    uint64_t groups = (blocks - first_data + bpg - 1) / bpg;
    if (!groups || groups > 0xFFFFFFFFu || groups * ipg != inodes) return -EINVAL;
    uint64_t gdt_blocks = (groups * desc_size + bs - 1) / bs;
    if (first_data + 1 + gdt_blocks > blocks) return -EINVAL;

    memset(&e4, 0, sizeof e4);
    e4.gbuf = malloc(bs);
    e4.ibuf = malloc(bs);
    e4.tmp = malloc(bs);
    if (!e4.gbuf || !e4.ibuf || !e4.tmp) goto fail;
    e4.base = part_lba;
    e4.bs = bs;
    e4.spb = bs / 512;
    e4.blocks = blocks;
    e4.inodes = inodes;
    e4.ipg = ipg;
    e4.inode_size = inode_size;
    e4.desc_size = desc_size;
    e4.groups = (uint32_t)groups;
    e4.first_data_block = first_data;
    e4.filetype = (incompat & INCOMPAT_FILETYPE) != 0;
    e4.wide = is64;
    e4.gdt_blk = first_data + 1;
    e4.gbuf_blk = e4.ibuf_blk = ~0ULL;

    e4.mounted = 1;
    if (read_inode(EXT4_ROOT_INO, &root) < 0 || (root.mode & S_IFMT) != S_IFDIR)
        goto fail;                                /* fail clears e4 */
    return 0;
fail:
    free(e4.gbuf); free(e4.ibuf); free(e4.tmp);
    memset(&e4, 0, sizeof e4);
    return r;
}

/* ---- fs.h (dispatched by bios/disk.c) -------------------------------------- */

int ext4_stat(const char *path, int *isdir, uint64_t *size)
{
    struct inode i;
    int r = lookup(path, &i);
    if (r < 0) return r;
    *isdir = (i.mode & S_IFMT) == S_IFDIR;
    *size = *isdir ? 0 : i.size;
    return 0;
}

int ext4_open(const char *path, fs_file **out)
{
    struct inode i;
    int r = lookup(path, &i);
    if (r < 0) return r;
    if ((i.mode & S_IFMT) == S_IFDIR) return -EISDIR;
    fs_file *f = calloc(1, sizeof *f);
    if (!f) return -ENOMEM;
    if (load_tree(&i, &f->exts, &f->nexts) < 0) { free(f); return -EIO; }
    f->size = i.size;
    *out = f;
    return 0;
}

void ext4_close(fs_file *f)
{
    if (f) { free(f->exts); free(f); }
}

uint64_t ext4_size(fs_file *f) { return f->size; }

int ext4_seek(fs_file *f, uint64_t off)
{
    if (off > f->size) return -EINVAL;
    f->pos = off;
    return 0;
}

long ext4_read(fs_file *f, void *buf, size_t n)
{
    long r = read_exts(f->exts, f->nexts, f->size, f->pos, buf, n);
    if (r > 0) f->pos += r;
    return r;
}

struct rd { fs_dir_cb cb; void *arg; };

static int rd_cb(struct dent *d, void *arg)
{
    struct rd *st = arg;
    int isdir = d->isdir;
    uint64_t size = 0;
    if (isdir != 1) {                            /* not known to be a directory */
        struct inode i;
        if (read_inode(d->ino, &i) < 0) return 0;
        isdir = (i.mode & S_IFMT) == S_IFDIR;
        size = isdir ? 0 : i.size;
    }
    return st->cb(d->name, isdir, size, st->arg);
}

int ext4_readdir(const char *path, fs_dir_cb cb, void *arg)
{
    struct inode dir;
    int r = lookup(path, &dir);
    if (r < 0) return r;
    if ((dir.mode & S_IFMT) != S_IFDIR) return -ENOTDIR;
    struct rd st = { cb, arg };
    r = dir_walk(&dir, rd_cb, &st);
    return r < 0 ? r : 0;
}
