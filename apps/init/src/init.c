/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * init -- the session keeper (phase 3.3.7).
 *
 * The kernel starts this as pid 1 instead of its own shell
 * (sys/kern/src/main.rs): load /sbin/init, hand over, and the userland
 * is on.  What a session keeper owes:
 *
 *   * the mounts -- /proc and /tmp are pid 1's to set up, like on every
 *     real unix (the kernel keeps / and /dev, the devtmpfs split;
 *     mount(2) through prism_raw.h, phase 3.3.7);
 *   * a shell per terminal, always -- one on the console (/dev/tty0)
 *     and, when the boot probe found a UART, a getty on /dev/ttyS0
 *     (phase 3.3.5: the serial line is a terminal of its own now, the
 *     one the headless machines and the boot tests talk to).  Both
 *     come back when they exit, crash, or take a signal they cannot
 *     refuse (a ^D at the prompt, the respawn loop is blind to why);
 *   * the reaping -- waitpid(-1) collects the shells, their orphaned
 *     grandchildren (the kernel re-parents them here, like every
 *     unix) and anything else that dies alone;
 *   * composure -- the terminal signals bounce off (a ^C is for the
 *     foreground child, not for the session), and nothing here reads
 *     the terminals: the shells own them.
 *
 * Deliberately not here yet (the roadmap's 3.4 step): /etc/inittab
 * parsing, runlevels, a shutdown protocol -- busybox init brings all
 * of that.  This is the honest minimum a pid 1 must be.
 */

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "prism_raw.h"

#define SHELL "/usr/bin/sh"

/* The environment every session program starts with -- the same words
 * the kernel shell's `exec` hands its programs (PATH walks the
 * usr-merge layout first).  The kernel adds PRISM_TTYS0=1 when the
 * boot probe found a serial UART: the getty's cue. */
static char *const envp[] = {
        "PATH=/usr/bin:/bin",
        "HOME=/",
        "TERM=prism",
        NULL,
};

/* One mount pid 1 makes at a time.  EBUSY (already mounted) is fine --
 * the rescue path or a restart may have been there first. */
static void mount_one(const char *path, const char *type, int rdonly)
{
        long r = prism_mount("none", path, type, rdonly ? 1UL : 0UL);
        if (r == 0)
                printf("init: %s mounted (%s%s)\n", path, type,
                        rdonly ? ", read-only" : "");
        else if (r == -16) /* EBUSY */
                printf("init: %s already mounted\n", path);
        else
                printf("init: cannot mount %s: %s\n", path, strerror((int)-r));
}

/* fork + exec the shell with its standard trio on `tty` -- the getty
 * shape: the child owns the terminal from its very first printf. */
static pid_t spawn_shell(const char *tty)
{
        pid_t pid = fork();

        if (pid < 0) {
                printf("init: fork failed (%s)\n", strerror(errno));
                return -1;
        }
        if (pid == 0) {
                /* The child: back to signal defaults, then the terminal,
                 * then the shell.  execve would reset the caught ones
                 * anyway; this keeps the failed-exec path honest too. */
                signal(SIGINT, SIG_DFL);
                signal(SIGQUIT, SIG_DFL);
                signal(SIGTERM, SIG_DFL);
                signal(SIGHUP, SIG_DFL);
                int fd = open(tty, O_RDWR);
                if (fd < 0)
                        _exit(126); /* no terminal, no session */
                dup2(fd, 0);
                dup2(fd, 1);
                dup2(fd, 2);
                if (fd > 2)
                        close(fd);
                char *argv[] = { (char *)SHELL, NULL };
                execve(SHELL, argv, envp);
                _exit(127); /* not reached on success */
        }
        return pid;
}

/* The death report, the shell's name spelled out (two terminals, two
 * answers). */
static void report(const char *which, int st)
{
        if (WIFSIGNALED(st))
                printf("init: %s shell killed by signal %d, respawning\n",
                        which, WTERMSIG(st));
        else
                printf("init: %s shell exited (status %d), respawning\n",
                        which, WIFEXITED(st) ? WEXITSTATUS(st) : 0);
}

int main(void)
{
        /* A ^C is for the foreground command, not for the session. */
        signal(SIGINT, SIG_IGN);
        signal(SIGQUIT, SIG_IGN);
        signal(SIGTERM, SIG_IGN);
        signal(SIGHUP, SIG_IGN);

        setvbuf(stdout, 0, _IONBF, 0);

        /* The mounts (phase 3.3.7): /proc is the process state the
         * shells' ps / df read, /tmp the RAM scratch pad -- the same two
         * the kernel used to mount itself, now through mount(2) like on
         * a real unix.  A failure is a notice, not a stop: the shells
         * can still run, and the programs will say what is missing. */
        mount_one("/proc", "procfs", 1);
        mount_one("/tmp", "tmpfs", 0);

        /* The console session first -- the machine's own screen and
         * keyboard, /dev/tty0 (writes stay on the text console; the
         * serial line below is nobody's mirror anymore). */
        printf("\nPrismOS init: starting the console shell\n");
        pid_t console_pid = spawn_shell("/dev/tty0");

        /* The serial session (phase 3.3.5): /dev/ttyS0, a line
         * discipline of its own on the UART's queue.  The kernel says
         * PRISM_TTYS0 when it found one; without a UART there is no
         * point parking a shell on a dead line. */
        pid_t serial_pid = -1;
        if (getenv("PRISM_TTYS0")) {
                printf("\nPrismOS init: starting the serial shell (ttyS0)\n");
                serial_pid = spawn_shell("/dev/ttyS0");
        }

        for (;;) {
                int st;
                pid_t pid = waitpid(-1, &st, 0);

                if (pid < 0) {
                        sleep(1); /* EINTR or a hiccup: breathe, go on */
                        continue;
                }
                if (pid == serial_pid) {
                        report("serial", st);
                        /* Respawn pacing: a shell that dies instantly must
                         * not turn the session into a fork bomb. */
                        sleep(1);
                        printf("\nPrismOS init: starting the serial shell (ttyS0)\n");
                        serial_pid = spawn_shell("/dev/ttyS0");
                } else if (pid == console_pid) {
                        report("console", st);
                        sleep(1);
                        printf("\nPrismOS init: starting the console shell\n");
                        console_pid = spawn_shell("/dev/tty0");
                }
                /* Anything else that died here is an orphan the kernel
                 * re-parented: reaped, and no ceremony for it. */
        }
}
