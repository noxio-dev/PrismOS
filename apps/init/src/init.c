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
 *     mount(2) through the libc since phase 3.4: the mount pair moved
 *     from prism_raw.h into mlibc's linux option with the busybox
 *     step);
 *   * a shell per terminal, always -- one on the console (/dev/tty0)
 *     and, when the boot probe found a UART, a getty on /dev/ttyS0
 *     (phase 3.3.5: the serial line is a terminal of its own now, the
 *     one the headless machines and the boot tests talk to).  Since
 *     3.4.5 the shell is /usr/bin/sh -- the busybox symlink, ash --
 *     started as a *login* shell (argv[0] = "-sh", the classic getty
 *     spelling) so it reads /etc/profile: the PATH and the prompt the
 *     whole session inherits.  Both come back when they exit, crash,
 *     or take a signal they cannot refuse (a ^D at the prompt, the
 *     respawn loop is blind to why);
 *   * the reaping -- waitpid(-1) collects the shells, their orphaned
 *     grandchildren (the kernel re-parents them here, like every
 *     unix) and anything else that dies alone;
 *   * composure -- the terminal signals bounce off (a ^C is for the
 *     foreground child, not for the session), and nothing here reads
 *     the terminals: the shells own them.
 *
 * Deliberately not here yet: /etc/inittab parsing, runlevels, a
 * shutdown protocol -- busybox init brings all of that, and this file
 * is not it.  This is the honest minimum a pid 1 must be.
 */

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/wait.h>
#include <unistd.h>
#include <sys/ioctl.h>

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
 * the rescue path or a restart may have been there first.  Since phase
 * 3.4 this is mlibc's mount(2) -- the linux option's own call, the pair
 * the busybox `mount` applet speaks too; the raw prism_raw.h spelling
 * is /sbin/init's history. */
static void mount_one(const char *path, const char *type, int rdonly)
{
        unsigned long flags = rdonly ? MS_RDONLY : 0;
        if (mount("none", path, type, flags, NULL) == 0)
                printf("init: %s mounted (%s%s)\n", path, type,
                        rdonly ? ", read-only" : "");
        else if (errno == EBUSY)
                printf("init: %s already mounted\n", path);
        else
                printf("init: cannot mount %s: %s\n", path, strerror(errno));
}

/* fork + exec the shell with its standard trio on `tty` -- the getty
 * shape: the child owns the terminal from its very first printf.  The
 * argv[0] spelling is the login-shell one ("-sh"): busybox ash reads
 * /etc/profile for it -- the PATH and the PS1 the session lives with
 * (phase 3.4.5; apps/sh stays installed as /usr/bin/minish, the rig
 * the 3.3 scenarios ran on). */
static pid_t spawn_shell(const char *tty)
{
        pid_t pid = fork();

        if (pid < 0) {
                printf("init: fork failed (%s)\n", strerror(errno));
                return -1;
        }
        if (pid == 0) {
                signal(SIGINT, SIG_DFL);
                signal(SIGQUIT, SIG_DFL);
                signal(SIGTERM, SIG_DFL);
                signal(SIGHUP, SIG_DFL);

                if (setsid() < 0) {
                        dprintf(2, "init: setsid: %s\n", strerror(errno));
                        _exit(126);
                }

                int fd = open(tty, O_RDWR);
                if (fd < 0) {
                        dprintf(2, "init: open %s: %s\n", tty, strerror(errno));
                        _exit(126);
                }

                if (ioctl(fd, TIOCSCTTY, 0) < 0) {
                        dprintf(2, "init: TIOCSCTTY: %s\n", strerror(errno));
                        _exit(126);
                }

                for (int i = 0; i < 3; i++) {
                        if (dup2(fd, i) < 0) {
                                dprintf(2, "init: dup2(%d): %s\n", i, strerror(errno));
                                _exit(126);
                        }
                }

                if (fd > 2)
                        close(fd);

                char *argv[] = {(char *)"-sh", NULL};
                execve(SHELL, argv, envp);

                dprintf(2, "init: execve: %s\n", strerror(errno));
                _exit(127);
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
