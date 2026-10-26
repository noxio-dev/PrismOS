/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * sh -- the userland shell (phase 3.3.7): what init respawns on the
 * console.
 *
 * The split of labour is the unix one: the kernel's tty owns the line
 * (user/tty.rs -- ICANON assembles, ECHO answers, backspace and ^U
 * edit, ^C is ISIG for the foreground), this owns the words.  What is
 * here:
 *
 *   * the builtins of the daily surface: cd / pwd / echo / exit / help,
 *     ls (-l) / cat / touch / mkdir / rm / rmdir, write / append (the
 *     kernel shell's conveniences, same spelling), df (through
 *     /proc/mounts + the statfs of each), ps (through /proc/<pid>/stat)
 *     and env;
 *   * everything else: a PATH walk (/usr/bin, /bin), fork + execve +
 *     waitpid, with the foreground claim (TIOCSPGRP) parked on the
 *     child while it runs -- that is what makes its ^C work;
 *   * `sh -c 'words'` and `sh <script>`: the same loop over a string /
 *     a file instead of the console, the shape an /etc/rc will want.
 *
 * Deliberately not here (the roadmap's 3.4 step): pipes, redirections,
 * job control, history -- busybox ash brings the lot.  The prompt is
 * prism$ (the kernel shell's prism# stays the rescue mode's).
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#include "prism_raw.h"

/* The TIOC* / TCSA* words come from <termios.h> (abi-bits/ioctls.h);
 * the requests themselves go through prism_ioctl -- mlibc's public
 * ioctl() is glibc-option code the port keeps off, while tcgetattr /
 * tcsetattr are plain library calls. */

#define MAXARGS 32
#define MAXLINE 512
#define PROMPT "prism$ "
#define PATH0 "/usr/bin"
#define PATH1 "/bin"

static volatile sig_atomic_t interrupted;

static void on_sigint(int sig)
{
        (void)sig;
        interrupted = 1;
}

/* ---------------------------------------------------------- utilities */

static void err(const char *cmd, const char *what, const char *why)
{
        fprintf(stderr, "sh: %s: %s%s%s\n", cmd, what,
                why ? ": " : "", why ? why : "");
}

/* A byte count as B / KiB / MiB (the kernel shell's df style). */
static void fmt_size(char *out, unsigned long long n)
{
        if (n >= 10UL << 20)
                sprintf(out, "%llu MiB", n >> 20);
        else if (n >= 10UL << 10)
                sprintf(out, "%llu KiB", n >> 10);
        else
                sprintf(out, "%llu B", n);
}

/* ------------------------------------------------------- line reading */

/* One line from the console: the tty's discipline did the editing, so
 * this is just a read -- interrupted (^C: EINTR, answered by a fresh
 * prompt) and un-bounded (EAGAIN past the house limit: re-read).  0 on
 * EOF (^D at an empty prompt). */
static int read_line(char *buf, int cap)
{
        int got = 0;

        while (got < cap - 1) {
                char c;
                ssize_t n = read(0, &c, 1);

                if (n < 0) {
                        if (errno == EINTR)
                                return interrupted ? -2 : -1;
                        if (errno == EAGAIN)
                                continue; /* the bounded blocker: re-read */
                        return -1;
                }
                if (n == 0)
                        break; /* EOF */
                if (c == '\n')
                        break;
                buf[got++] = c;
        }
        buf[got] = 0;
        return got;
}

/* ---------------------------------------------------------- tokenizer */

/* Whitespace words, 'single' and "double" quotes (no $ or escapes --
 * the honest minimum).  Returns argc, 0 on an empty line, -1 on a
 * quote left open. */
static int split(char *line, char **argv, int max)
{
        int argc = 0;
        char *p = line;

        while (*p) {
                while (*p == ' ' || *p == '\t')
                        p++;
                if (!*p)
                        break;
                if (argc == max)
                        return -1;
                char *out = p, quote = 0;

                argv[argc++] = out;
                while (*p && (quote || (*p != ' ' && *p != '\t'))) {
                        if (!quote && (*p == '\'' || *p == '"')) {
                                quote = *p++;
                                continue;
                        }
                        if (quote && *p == quote) {
                                quote = 0;
                                p++;
                                continue;
                        }
                        *out++ = *p++;
                }
                if (quote)
                        return -1;
                /* `out` trails `p` (quotes make it shorter, never longer):
                 * remember where the word ended *before* terminating it --
                 * with no quotes out == p, and writing the NUL there used
                 * to erase the delimiter the loop needs, ending the scan
                 * after the first word. */
                char *end = p;
                int more = *end != 0;

                *out = 0;
                if (more)
                        p = end + 1;
        }
        return argc;
}

/* ----------------------------------------------------------- builtins */

static int bi_cd(int argc, char **argv)
{
        const char *dir = argc > 1 ? argv[1] : "/";

        if (chdir(dir) < 0) {
                err("cd", dir, strerror(errno));
                return 1;
        }
        return 0;
}

static int bi_pwd(void)
{
        char buf[256];

        if (getcwd(buf, sizeof buf))
                printf("%s\n", buf);
        return 0;
}

static int bi_echo(int argc, char **argv)
{
        for (int i = 1; i < argc; i++)
                printf("%s%s", i > 1 ? " " : "", argv[i]);
        printf("\n");
        return 0;
}

static int bi_env(void)
{
        extern char **environ;

        for (char **e = environ; *e; e++)
                printf("%s\n", *e);
        return 0;
}

static int bi_help(void)
{
        printf("builtins: cd pwd echo env help exit version\n");
        printf("          ls [-l] cat touch mkdir rm rmdir write append df ps\n");
        printf("the rest: found through PATH (%s %s)\n", PATH0, PATH1);
        return 0;
}

static int bi_version(void)
{
        printf("sh (PrismOS userland shell), on mlibc\n");
        return 0;
}

/* ls: one directory (or file) per argument, "." by default. */
static void ls_entry(const char *path, int longfmt);

static void ls_dir(const char *path, int longfmt)
{
        DIR *d = opendir(path);

        if (!d) {
                err("ls", path, strerror(errno));
                return;
        }
        struct dirent *e;
        int n = 0;

        while ((e = readdir(d)) != NULL)
                n++;
        rewinddir(d);
        /* readdir order is whatever the file system says; sort it the way
         * an ls reads. */
        char (*names)[256] = calloc(n ? n : 1, 256);
        if (!names) {
                closedir(d);
                err("ls", path, "out of memory");
                return;
        }
        int i = 0;
        while ((e = readdir(d)) != NULL && i < n) {
                snprintf(names[i], 256, "%s", e->d_name);
                i++;
        }
        for (int a = 0; a < i; a++)
                for (int b = a + 1; b < i; b++)
                        if (strcmp(names[a], names[b]) > 0) {
                                char tmp[256];

                                memcpy(tmp, names[a], 256);
                                memcpy(names[a], names[b], 256);
                                memcpy(names[b], tmp, 256);
                        }
        for (int k = 0; k < i; k++) {
                char full[512];

                if (strcmp(path, "/") == 0)
                        snprintf(full, sizeof full, "/%s", names[k]);
                else
                        snprintf(full, sizeof full, "%s/%s", path, names[k]);
                ls_entry(full, longfmt);
        }
        free(names);
        closedir(d);
}

static void ls_entry(const char *path, int longfmt)
{
        struct stat st;

        if (stat(path, &st) < 0) {
                err("ls", path, strerror(errno));
                return;
        }
        const char *base = strrchr(path, '/');

        base = base ? base + 1 : path;
        if (!longfmt) {
                printf("%s\n", base);
                return;
        }
        char mode[11] = "----------";

        if (S_ISDIR(st.st_mode))
                mode[0] = 'd';
        else if (S_ISLNK(st.st_mode))
                mode[0] = 'l';
        for (int b = 0; b < 9; b++)
                if (st.st_mode & (1 << (8 - b)))
                        mode[1 + b] = "rwx"[b % 3];
        char size[24];

        fmt_size(size, (unsigned long long)st.st_size);
        printf("%s %5llu %s %s\n", mode,
                (unsigned long long)st.st_nlink, size, base);
        /* The link's target, spelled out (the usr-merge's are the proof). */
        if (S_ISLNK(st.st_mode)) {
                char target[256];
                ssize_t n = readlink(path, target, sizeof target - 1);

                if (n > 0) {
                        target[n] = 0;
                        printf("    -> %s\n", target);
                }
        }
}

static int bi_ls(int argc, char **argv)
{
        int longfmt = 0, first = 1;

        for (int i = 1; i < argc; i++) {
                if (argv[i][0] == '-') {
                        if (strchr(argv[i], 'l'))
                                longfmt = 1;
                        continue;
                }
                first = 0;
        }
        if (first) {
                ls_dir(".", longfmt);
                return 0;
        }
        for (int i = 1; i < argc; i++) {
                if (argv[i][0] == '-')
                        continue;
                struct stat st;

                if (stat(argv[i], &st) == 0 && S_ISDIR(st.st_mode))
                        ls_dir(argv[i], longfmt);
                else
                        ls_entry(argv[i], longfmt);
        }
        return 0;
}

static int bi_cat(int argc, char **argv)
{
        if (argc < 2) {
                /* cat with nothing: echo the console (until ^D). */
                char buf[512];
                ssize_t n;

                while ((n = read(0, buf, sizeof buf)) > 0)
                        write(1, buf, n);
                return 0;
        }
        for (int i = 1; i < argc; i++) {
                char buf[1024];
                int fd = open(argv[i], O_RDONLY);
                ssize_t n;

                if (fd < 0) {
                        err("cat", argv[i], strerror(errno));
                        return 1;
                }
                while ((n = read(fd, buf, sizeof buf)) > 0)
                        write(1, buf, n);
                close(fd);
        }
        return 0;
}

static int bi_touch(int argc, char **argv)
{
        for (int i = 1; i < argc; i++) {
                int fd = open(argv[i], O_WRONLY | O_CREAT, 0666);

                if (fd < 0) {
                        err("touch", argv[i], strerror(errno));
                        return 1;
                }
                close(fd);
        }
        return 0;
}

static int bi_mkdir(int argc, char **argv)
{
        for (int i = 1; i < argc; i++)
                if (mkdir(argv[i], 0777) < 0) {
                        err("mkdir", argv[i], strerror(errno));
                        return 1;
                }
        return 0;
}

static int bi_rmdir(int argc, char **argv)
{
        for (int i = 1; i < argc; i++)
                if (rmdir(argv[i]) < 0) {
                        err("rmdir", argv[i], strerror(errno));
                        return 1;
                }
        return 0;
}

static int bi_rm(int argc, char **argv)
{
        for (int i = 1; i < argc; i++)
                if (unlink(argv[i]) < 0) {
                        err("rm", argv[i], strerror(errno));
                        return 1;
                }
        return 0;
}

/* write / append FILE TEXT... -- the kernel shell's conveniences, same
 * spelling: the text (joined with single spaces) lands in the file,
 * write truncating, append adding a line. */
static int write_file(int argc, char **argv, int append)
{
        if (argc < 2) {
                printf("usage: %s file text...\n", argv[0]);
                return 1;
        }
        int fd = open(argv[1], O_WRONLY | O_CREAT | (append ? O_APPEND : O_TRUNC), 0666);

        if (fd < 0) {
                err(argv[0], argv[1], strerror(errno));
                return 1;
        }
        int bad = 0;
        for (int i = 2; i < argc; i++)
                bad |= dprintf(fd, "%s%s", i > 2 ? " " : "", argv[i]) < 0;
        bad |= dprintf(fd, "\n") < 0;
        close(fd);
        if (bad)
                err(argv[0], argv[1], strerror(errno));
        return bad ? 1 : 0;
}

/* df: /proc/mounts + the statfs of each (through prism_raw.h -- the
 * kernel's own syscall 48, since mlibc's statfs tag waits for its linux
 * option) -- the same columns the kernel shell's df prints (fs / size /
 * used / avail / inodes / mounted on). */
static int bi_df(void)
{
        FILE *f = fopen("/proc/mounts", "r");

        if (!f) {
                err("df", "/proc/mounts", strerror(errno));
                return 1;
        }
        printf("%-14s %9s %9s %9s %6s  %s\n", "fs", "size", "used", "avail",
                "inodes", "mounted on");
        char line[512];

        while (fgets(line, sizeof line, f)) {
                char src[64], dir[128], type[16];
                struct prism_statfs st;

                if (sscanf(line, "%63s %127s %15s", src, dir, type) != 3)
                        continue;
                if (prism_statfs(dir, &st) < 0) {
                        err("df", dir, strerror(errno));
                        continue;
                }
                unsigned long long bsz = st.f_bsize;
                unsigned long long total = st.f_blocks * bsz;
                unsigned long long freeb = st.f_bfree * bsz;
                char s1[24], s2[24], s3[24];

                fmt_size(s1, total);
                fmt_size(s2, total - freeb);
                fmt_size(s3, freeb);
                printf("%-14s %9s %9s %9s %6llu  %s\n", src, s1, s2, s3,
                        (unsigned long long)st.f_files, dir);
        }
        fclose(f);
        return 0;
}

/* ps: /proc/<pid>/stat, the first four fields (pid, comm, state, ppid). */
static int bi_ps(void)
{
        DIR *d = opendir("/proc");

        if (!d) {
                err("ps", "/proc", strerror(errno));
                return 1;
        }
        printf("%5s %5s %-8s %s\n", "pid", "ppid", "state", "command");
        struct dirent *e;

        while ((e = readdir(d)) != NULL) {
                char *end;
                long pid = strtol(e->d_name, &end, 10);

                if (*end || pid <= 0)
                        continue;
                char path[64], line[512];
                snprintf(path, sizeof path, "/proc/%ld/stat", pid);
                FILE *f = fopen(path, "r");
                if (!f)
                        continue;
                if (fgets(line, sizeof line, f)) {
                        /* "pid (comm) state ppid ..." */
                        char *paren = strchr(line, '(');
                        char *cparen = paren ? strchr(paren, ')') : NULL;
                        if (paren && cparen) {
                                *cparen = 0;
                                char state = cparen[2];
                                long ppid = atol(cparen + 4);
                                printf("%5ld %5ld %-8c %s\n", pid, ppid, state,
                                        paren + 1);
                        }
                }
                fclose(f);
        }
        closedir(d);
        return 0;
}

/* ------------------------------------------------------- the run loop */

/* The foreground claim: parked on the child while it runs, back on this
 * shell at the prompt (TIOCSPGRP -- that is what a ^C targets). */
static void set_fg(pid_t pid)
{
        int pgrp = pid;

        (void)prism_ioctl(0, TIOCSPGRP, &pgrp);
}

/* Run one command: builtins here, the PATH walk in a child.  Returns
 * the exit status the shell should report. */
static int run_line(int argc, char **argv)
{
        if (argc == 0)
                return 0;
        const char *cmd = argv[0];

        if (!strcmp(cmd, "cd"))
                return bi_cd(argc, argv);
        if (!strcmp(cmd, "pwd"))
                return bi_pwd();
        if (!strcmp(cmd, "echo"))
                return bi_echo(argc, argv);
        if (!strcmp(cmd, "env"))
                return bi_env();
        if (!strcmp(cmd, "help"))
                return bi_help();
        if (!strcmp(cmd, "version"))
                return bi_version();
        if (!strcmp(cmd, "ls"))
                return bi_ls(argc, argv);
        if (!strcmp(cmd, "cat"))
                return bi_cat(argc, argv);
        if (!strcmp(cmd, "touch"))
                return bi_touch(argc, argv);
        if (!strcmp(cmd, "mkdir"))
                return bi_mkdir(argc, argv);
        if (!strcmp(cmd, "rmdir"))
                return bi_rmdir(argc, argv);
        if (!strcmp(cmd, "rm"))
                return bi_rm(argc, argv);
        if (!strcmp(cmd, "write"))
                return write_file(argc, argv, 0);
        if (!strcmp(cmd, "append"))
                return write_file(argc, argv, 1);
        if (!strcmp(cmd, "df"))
                return bi_df();
        if (!strcmp(cmd, "ps"))
                return bi_ps();
        if (!strcmp(cmd, "exit"))
                exit(0);

        /* External: fork, claim the foreground for the child, exec, wait. */
        pid_t pid = fork();

        if (pid < 0) {
                err("sh", "fork", strerror(errno));
                return 1;
        }
        if (pid == 0) {
                signal(SIGINT, SIG_DFL);
                extern char **environ;
                char path[512];
                if (strchr(cmd, '/')) {
                        execve(cmd, argv, environ);
                        err("sh", cmd, strerror(errno));
                        _exit(127);
                }
                /* The PATH walk, /usr/bin first (the usr-merge layout). */
                const char *dirs[2] = { PATH0, PATH1 };

                for (int i = 0; i < 2; i++) {
                        snprintf(path, sizeof path, "%s/%s", dirs[i], cmd);
                        execve(path, argv, environ);
                        if (errno != ENOENT) {
                                err("sh", path, strerror(errno));
                                _exit(126);
                        }
                }
                printf("sh: %s: command not found\n", cmd);
                _exit(127);
        }
        set_fg(pid);
        int st;

        while (waitpid(pid, &st, 0) < 0 && errno == EINTR)
                ;
        set_fg(getpid());
        if (WIFSIGNALED(st))
                printf("sh: %s: killed by signal %d\n", cmd, WTERMSIG(st));
        return WIFEXITED(st) ? WEXITSTATUS(st) : 1;
}

/* One input source: the console (interactive) or an open script. */
static int run_stream(FILE *in, int interactive)
{
        char line[MAXLINE];

        for (;;) {
                if (interactive) {
                        printf(PROMPT);
                        interrupted = 0;
                }
                int n = interactive ? read_line(line, sizeof line)
                                    : (fgets(line, sizeof line, in) ? 1 : 0);

                if (interactive && n == -2) {
                        /* A ^C at the prompt: a fresh line, like every shell. */
                        printf("\n");
                        continue;
                }
                if (n <= 0)
                        return 0; /* EOF: the caller decides what that means */
                char *argv[MAXARGS];
                int argc = split(line, argv, MAXARGS);

                if (argc < 0) {
                        printf("sh: too many words (or a quote left open)\n");
                        continue;
                }
                run_line(argc, argv);
        }
}

int main(int argc, char **argv)
{
        setvbuf(stdout, 0, _IONBF, 0);
        signal(SIGINT, on_sigint);
        set_fg(getpid());

        /* sh -c 'words' */
        if (argc == 3 && !strcmp(argv[1], "-c")) {
                char *av[MAXARGS];
                int n = split(argv[2], av, MAXARGS);

                return n > 0 ? run_line(n, av) : 0;
        }
        /* sh <script> */
        if (argc == 2) {
                FILE *f = fopen(argv[1], "r");

                if (!f) {
                        err("sh", argv[1], strerror(errno));
                        return 1;
                }
                int st = run_stream(f, 0);

                fclose(f);
                return st;
        }
        /* The console session (init respawns this). */
        run_stream(stdin, 1);
        return 0;
}