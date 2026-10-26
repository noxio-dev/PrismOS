/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * init -- the session keeper (phase 3.3.7).
 *
 * The kernel starts this as pid 1 instead of its own shell
 * (sys/kern/src/main.rs): load /sbin/init, hand over, and the userland
 * is on.  What a session keeper owes:
 *
 *   * a shell on the console, always -- fork, exec /usr/bin/sh, and
 *     again when it exits (the shell's own exit, a ^D at the prompt,
 *     a crash: everything comes back to the respawn loop);
 *   * the reaping -- waitpid(-1) collects the shell, the shell's
 *     orphaned grandchildren (the kernel re-parents them here, like
 *     every unix) and anything else that dies alone;
 *   * composure -- the terminal signals bounce off (a ^C is for the
 *     foreground child, not for the session), and nothing here reads
 *     the console: the shell owns it.
 *
 * Deliberately not here yet (the roadmap's 3.4 step): /etc/inittab
 * parsing, runlevels, a shutdown protocol -- busybox init brings all
 * of that.  This is the honest minimum a pid 1 must be.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define SHELL "/usr/bin/sh"

/* The environment every session program starts with -- the same words
 * the kernel shell's `exec` hands its programs (PATH walks the
 * usr-merge layout first). */
static char *const envp[] = {
	"PATH=/usr/bin:/bin",
	"HOME=/",
	"TERM=prism",
	NULL,
};

int main(void)
{
	/* A ^C is for the foreground command, not for the session. */
	signal(SIGINT, SIG_IGN);
	signal(SIGQUIT, SIG_IGN);
	signal(SIGTERM, SIG_IGN);
	signal(SIGHUP, SIG_IGN);

	setvbuf(stdout, 0, _IONBF, 0);
	printf("\nPrismOS init: starting the console shell\n");

	for (;;) {
		pid_t pid = fork();

		if (pid < 0) {
			/* fork failed: say so, breathe, try again. */
			printf("init: fork failed (%s)\n", strerror(errno));
			sleep(1);
			continue;
		}
		if (pid == 0) {
			/* The child: back to signal defaults, then the shell.
			 * execve would reset the caught ones anyway; this keeps
			 * the failed-exec path honest too. */
			signal(SIGINT, SIG_DFL);
			signal(SIGQUIT, SIG_DFL);
			signal(SIGTERM, SIG_DFL);
			signal(SIGHUP, SIG_DFL);
			char *argv[] = { (char *)SHELL, NULL };
			execve(SHELL, argv, envp);
			/* Not reached on success. */
			printf("init: cannot exec %s (%s)\n", SHELL,
				strerror(errno));
			_exit(127);
		}

		/* The parent: reap everything that dies while the shell runs
		 * (the orphans the kernel re-parents here included), then
		 * wait for the shell itself and go around again. */
		int st;
		while (waitpid(-1, &st, 0) > 0)
			;

		if (WIFSIGNALED(st))
			printf("init: shell killed by signal %d, respawning\n",
				WTERMSIG(st));
		else
			printf("init: shell exited (status %d), respawning\n",
				WIFEXITED(st) ? WEXITSTATUS(st) : 0);
		/* Respawn pacing: a shell that dies instantly must not turn
		 * the session into a fork bomb against the log. */
		sleep(1);
	}
}
