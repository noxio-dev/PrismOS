/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * tmpfs -- the /tmp tmpfs exercised end to end from ring 3 (phase 3.3).
 *
 * The kernel mounts a RAM-backed file system on /tmp at boot
 * (sys/kern/src/fs/tmpfs.rs): everything the VFS can do to a file
 * system, bounded by a byte and an inode quota.  This program is the
 * ring-3 half of the proof -- the same scenarios the host-side fstest
 * runs (tests/fs_test.py, "tmpfs on /tmp"), spoken through mlibc's
 * POSIX surface instead: the file calls, lseek, stat / fstat, hard
 * links, the symlink pair (the new syscalls 46 symlink / 47 readlink),
 * rename, mkdir / rmdir with ENOTEMPTY, and getdents through opendir.
 *
 * The C library for the portable surface; nothing here needs the raw
 * shim of apps/include/prism_raw.h -- even the framebuffer pair stays
 * out (44 / 45 remain cube's).  Deterministic names, explicit cleanup
 * first (a previous run may have crashed), explicit cleanup last: /tmp
 * is RAM, but a good citizen leaves it as they found it.
 *
 *   exec tmpfs
 *   exec -t 100000 tmpfs     (the watchdog default is 3 s of CPU)
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define DIRPATH "/tmp/prism-tmpfs"

/* The check counter: every scenario is one check, the summary line is
 * the answer (and the exit status: 0 when everything passed). */
static int ok, total;

static void check(const char *what, int cond)
{
        total++;
        ok += cond ? 1 : 0;
        printf("tmpfs: %-38s %s\n", what, cond ? "ok" : "FAIL");
}

/* Best-effort removal of a stale tree from a previous crashed run. */
static void cleanup(void)
{
        static const char *files[] = { "data.txt", "hard", "renamed.txt",
                                       "soft", "inside.txt", NULL };
        char p[256];

        for (int i = 0; files[i]; i++) {
                snprintf(p, sizeof(p), "%s/%s", DIRPATH, files[i]);
                unlink(p);
        }
        snprintf(p, sizeof(p), "%s/sub", DIRPATH);
        rmdir(p);
        rmdir(DIRPATH);
}

int main(void)
{
        char p[256];
        char buf[256];

        cleanup();

        /* -- the tree ---------------------------------------------- */
        snprintf(p, sizeof(p), "%s/data.txt", DIRPATH);
        if (mkdir(DIRPATH, 0755) == 0)
                check("mkdir " DIRPATH, 1);
        else
                check("mkdir " DIRPATH, 0);

        /* -- create / write / read back ----------------------------- */
        int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        check("open data.txt O_CREAT", fd >= 0);
        const char *first = "hello tmpfs\n";
        ssize_t n = write(fd, first, strlen(first));
        check("write data.txt", n == (ssize_t)strlen(first));
        check("close data.txt (write side)", close(fd) == 0);

        fd = open(p, O_RDONLY);
        check("open data.txt O_RDONLY", fd >= 0);
        struct stat st;
        check("fstat size == 12", fstat(fd, &st) == 0 && st.st_size == 12);
        memset(buf, 0, sizeof(buf));
        n = read(fd, buf, sizeof(buf));
        check("read back data.txt", n == 12 && memcmp(buf, first, 12) == 0);

        /* -- append + lseek ---------------------------------------- */
        const char *second = "second line\n";
        int wfd = open(p, O_WRONLY | O_APPEND);
        check("open data.txt O_APPEND", wfd >= 0);
        check("append second line", write(wfd, second, strlen(second)) == 12);
        close(wfd);
        check("lseek back to 0", lseek(fd, 0, SEEK_SET) == 0);
        memset(buf, 0, sizeof(buf));
        n = read(fd, buf, sizeof(buf));
        check("24 bytes, hello..second", n == 24 && memcmp(buf, first, 12) == 0
              && memcmp(buf + 12, second, 12) == 0);
        check("lseek +6, read \"tmpfs\"", lseek(fd, 6, SEEK_SET) == 6
              && read(fd, buf, 5) == 5 && memcmp(buf, "tmpfs", 5) == 0);
        close(fd);

        /* -- stat --------------------------------------------------- */
        check("stat data.txt: regular, nlink 1",
              stat(p, &st) == 0 && S_ISREG(st.st_mode) && st.st_nlink == 1);

        /* -- hard links --------------------------------------------- */
        snprintf(p, sizeof(p), "%s/hard", DIRPATH);
        check("link hard", link(DIRPATH "/data.txt", p) == 0);
        check("nlink 2 after link",
              stat(DIRPATH "/data.txt", &st) == 0 && st.st_nlink == 2);
        check("unlink data.txt", unlink(DIRPATH "/data.txt") == 0);
        check("nlink 1 after unlink", stat(p, &st) == 0 && st.st_nlink == 1);

        /* -- rename ------------------------------------------------- */
        snprintf(buf, sizeof(buf), "%s/renamed.txt", DIRPATH);
        check("rename hard -> renamed.txt", rename(p, buf) == 0);
        check("old name gone (ENOENT)",
              stat(p, &st) == -1 && errno == ENOENT);

        /* -- symlinks: syscalls 46 / 47 ------------------------------ */
        snprintf(p, sizeof(p), "%s/soft", DIRPATH);
        check("symlink soft -> renamed.txt",
              symlink("renamed.txt", p) == 0);
        memset(buf, 0, sizeof(buf));
        n = readlink(p, buf, sizeof(buf) - 1);
        check("readlink says renamed.txt (no NUL)",
              n == 11 && memcmp(buf, "renamed.txt", 11) == 0);
        check("stat through soft follows", stat(p, &st) == 0
              && S_ISREG(st.st_mode) && st.st_size == 24);
        fd = open(p, O_RDONLY);
        check("open through soft", fd >= 0);
        memset(buf, 0, sizeof(buf));
        n = read(fd, buf, sizeof(buf));
        check("read through soft: the 24 bytes", n == 24);
        close(fd);

        /* -- mkdir / rmdir / ENOTEMPTY ------------------------------ */
        snprintf(p, sizeof(p), "%s/sub", DIRPATH);
        check("mkdir sub", mkdir(p, 0755) == 0);
        check("mkdir sub again: EEXIST",
              mkdir(p, 0755) == -1 && errno == EEXIST);
        snprintf(buf, sizeof(buf), "%s/sub/inside.txt", DIRPATH);
        fd = open(buf, O_CREAT | O_WRONLY, 0644);
        check("create sub/inside.txt", fd >= 0);
        close(fd);
        check("rmdir sub: ENOTEMPTY",
              rmdir(p) == -1 && errno == ENOTEMPTY);
        check("unlink inside.txt", unlink(buf) == 0);
        check("rmdir sub now", rmdir(p) == 0);

        /* -- getdents through opendir ------------------------------- */
        DIR *d = opendir(DIRPATH);
        check("opendir " DIRPATH, d != NULL);
        int seen = 0;
        struct dirent *e;
        while (d && (e = readdir(d)) != NULL)
                if (e->d_name[0] != '.')
                        seen++;
        if (d)
                closedir(d);
        check("readdir: renamed.txt + soft", seen == 2);

        /* -- /tmp itself -------------------------------------------- */
        check("stat /tmp: a directory", stat("/tmp", &st) == 0
              && S_ISDIR(st.st_mode));

        /* -- leave /tmp as we found it ------------------------------ */
        check("unlink renamed.txt", unlink(DIRPATH "/renamed.txt") == 0);
        check("unlink soft", unlink(DIRPATH "/soft") == 0);
        check("rmdir " DIRPATH, rmdir(DIRPATH) == 0);
        check("tree gone (ENOENT)",
              stat(DIRPATH, &st) == -1 && errno == ENOENT);

        printf("tmpfs: %d/%d checks passed\n", ok, total);
        return ok == total ? 0 : 1;
}
