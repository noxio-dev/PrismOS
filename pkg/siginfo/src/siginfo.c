/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/siginfo -- SA_SIGINFO, SA_ONSTACK and sigaltstack, end to end
 * (kernel user/signal.rs, syscall 112; the Xorg step, docs/XORG.md 2.1).
 * One line per scenario and a summary ("siginfo: all 7 passed").
 *
 *   info      an SA_SIGINFO handler gets signo / si_code SI_USER / the
 *             sender's pid and uid, and a ucontext whose saved rsp / rip
 *             look sane; a plain handler still works
 *   sender    a forked child kills its parent: si_pid is the child's
 *   regs      a handler that edits uc_mcontext.gregs[REG_RAX] changes what
 *             the interrupted code sees after sigreturn (kill's own result)
 *   fpu       xmm0 survives a handler that clobbers it (the FXSAVE image
 *             rides in the ucontext)
 *   altstack  SA_ONSTACK runs the handler inside the sigaltstack area,
 *             uc_stack reports SS_ONSTACK there, EPERM to replace it from
 *             inside, and the original stack is back afterwards
 *   altquery  sigaltstack query / disable round trip; too small -> ENOMEM,
 *             bad flags -> EINVAL
 *   mask      the handler runs with its signal blocked (uc_sigmask is the
 *             interrupted mask), and sigreturn restores it
 */
#define _GNU_SOURCE 1

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

static int fails;

static void check(const char *what, int ok, const char *detail)
{
        printf("  %-9s %s -- %s\n", what, ok ? "ok" : "FAIL", detail);
        if (!ok)
                fails++;
}

/* kill(2) as a raw syscall: the interrupted code then sees the handler's
 * edit of rax as the call's result (libc's wrapper would swallow it). */
static long raw_kill(long pid, long sig)
{
        long r;
        __asm__ volatile("syscall" : "=a"(r) : "0"(10L), "D"(pid), "S"(sig)
                         : "rcx", "r11", "memory");
        return r;
}

static void install(int sig, void (*fn)(int, siginfo_t *, void *), int flags)
{
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_sigaction = fn;
        sa.sa_flags = flags;
        sigemptyset(&sa.sa_mask);
        sigaction(sig, &sa, NULL);
}

/* ------------------------------------------------------------------ info */

static volatile int h_signo, h_code, h_pid, h_uid, h_ran, h_sane;

static void h_info(int sig, siginfo_t *si, void *ctx)
{
        ucontext_t *uc = ctx;
        h_signo = si->si_signo;
        h_code = si->si_code;
        h_pid = si->si_pid;
        h_uid = (int)si->si_uid;
        h_ran = sig;
        uint64_t rsp = uc->uc_mcontext.gregs[REG_RSP];
        uint64_t rip = uc->uc_mcontext.gregs[REG_RIP];
        h_sane = rsp > 0x10000 && rip > 0x10000 && sig == SIGUSR1;
}

static volatile int plain_ran;
static void h_plain(int sig) { plain_ran = sig; }

static void t_info(void)
{
        install(SIGUSR1, h_info, SA_SIGINFO);
        kill(getpid(), SIGUSR1);
        check("info", h_ran == SIGUSR1 && h_signo == SIGUSR1 && h_code == SI_USER
                      && h_pid == getpid() && h_uid == (int)getuid() && h_sane,
              "signo, SI_USER, si_pid, si_uid, a sane saved rsp / rip");
        signal(SIGUSR2, h_plain);
        kill(getpid(), SIGUSR2);
        if (plain_ran != SIGUSR2)
                check("info", 0, "a plain (non SA_SIGINFO) handler no longer runs");
}

/* ---------------------------------------------------------------- sender */

static void t_sender(void)
{
        h_ran = 0;
        install(SIGUSR1, h_info, SA_SIGINFO);
        pid_t parent = getpid();
        pid_t pid = fork();
        if (pid == 0) {
                kill(parent, SIGUSR1);
                _exit(0);
        }
        for (int i = 0; i < 300 && !h_ran; i++)
                usleep(10000);
        int st;
        waitpid(pid, &st, 0);
        check("sender", h_ran == SIGUSR1 && h_pid == pid && h_code == SI_USER,
              "si_pid names the child that sent it");
}

/* ------------------------------------------------------------------ regs */

static void h_regs(int sig, siginfo_t *si, void *ctx)
{
        (void)sig; (void)si;
        ((ucontext_t *)ctx)->uc_mcontext.gregs[REG_RAX] = 4242;
}

static void t_regs(void)
{
        install(SIGUSR1, h_regs, SA_SIGINFO);
        long r = raw_kill(getpid(), SIGUSR1);
        check("regs", r == 4242, "the handler's edit of rax is what sigreturn restores");
}

/* ------------------------------------------------------------------- fpu */

static void h_fpu(int sig, siginfo_t *si, void *ctx)
{
        (void)sig; (void)si; (void)ctx;
        uint64_t junk = 0xdeadbeefcafef00dULL;
        __asm__ volatile("movq %0, %%xmm0" : : "r"(junk) : "xmm0");
}

static void t_fpu(void)
{
        install(SIGUSR1, h_fpu, SA_SIGINFO);
        uint64_t in = 0x1122334455667788ULL, out = 0;
        long ret;
        __asm__ volatile("movq %3, %%xmm0\n\t"
                         "syscall\n\t"
                         "movq %%xmm0, %1"
                         : "=a"(ret), "=r"(out)
                         : "0"(10L), "r"(in), "D"((long)getpid()), "S"((long)SIGUSR1)
                         : "rcx", "r11", "xmm0", "memory");
        check("fpu", out == in, "xmm0 is restored after a handler clobbered it");
}

/* -------------------------------------------------------------- altstack */

#define ALT_SIZE 16384
static char altbuf[ALT_SIZE] __attribute__((aligned(16)));
static volatile int a_inside, a_flags_ok, a_eperm;

static void h_alt(int sig, siginfo_t *si, void *ctx)
{
        (void)sig; (void)si;
        ucontext_t *uc = ctx;
        char here;
        uintptr_t p = (uintptr_t)&here;
        a_inside = p >= (uintptr_t)altbuf && p < (uintptr_t)altbuf + ALT_SIZE;
        a_flags_ok = uc->uc_stack.ss_flags == SS_ONSTACK
                     && uc->uc_stack.ss_sp == altbuf && uc->uc_stack.ss_size == ALT_SIZE;
        stack_t other = { .ss_sp = altbuf, .ss_size = ALT_SIZE, .ss_flags = 0 };
        a_eperm = sigaltstack(&other, NULL) == -1 && errno == EPERM;
}

static void t_altstack(void)
{
        stack_t ss = { .ss_sp = altbuf, .ss_size = ALT_SIZE, .ss_flags = 0 };
        int r = sigaltstack(&ss, NULL);
        install(SIGUSR1, h_alt, SA_SIGINFO | SA_ONSTACK);
        kill(getpid(), SIGUSR1);
        stack_t now;
        sigaltstack(NULL, &now);
        check("altstack", r == 0 && a_inside && a_flags_ok && a_eperm && now.ss_flags == 0,
              "ran on the alternate stack, SS_ONSTACK inside, EPERM to replace it, flags 0 after");
}

/* -------------------------------------------------------------- altquery */

static void t_altquery(void)
{
        stack_t ss = { .ss_sp = altbuf, .ss_size = ALT_SIZE, .ss_flags = 0 }, old;
        int ok = sigaltstack(&ss, NULL) == 0;
        memset(&old, 0xff, sizeof old);
        ok &= sigaltstack(NULL, &old) == 0 && old.ss_sp == altbuf && old.ss_size == ALT_SIZE
              && old.ss_flags == 0;
        stack_t off = { .ss_flags = SS_DISABLE };
        ok &= sigaltstack(&off, &old) == 0 && old.ss_sp == altbuf;
        ok &= sigaltstack(NULL, &old) == 0 && (old.ss_flags & SS_DISABLE);
        stack_t tiny = { .ss_sp = altbuf, .ss_size = 64, .ss_flags = 0 };
        int small = sigaltstack(&tiny, NULL) == -1 && errno == ENOMEM;
        stack_t bad = { .ss_sp = altbuf, .ss_size = ALT_SIZE, .ss_flags = 0x40 };
        int badf = sigaltstack(&bad, NULL) == -1 && errno == EINVAL;
        check("altquery", ok && small && badf,
              "query, disable, ENOMEM below MINSIGSTKSZ, EINVAL for bad flags");
}

/* ------------------------------------------------------------------ mask */

static volatile int m_blocked, m_old_clear;

static void h_mask(int sig, siginfo_t *si, void *ctx)
{
        (void)sig; (void)si;
        sigset_t cur;
        sigprocmask(SIG_BLOCK, NULL, &cur);
        m_blocked = sigismember(&cur, SIGUSR1) == 1;
        m_old_clear = sigismember(&((ucontext_t *)ctx)->uc_sigmask, SIGUSR1) == 0;
}

static void t_mask(void)
{
        install(SIGUSR1, h_mask, SA_SIGINFO);
        kill(getpid(), SIGUSR1);
        sigset_t after;
        sigprocmask(SIG_BLOCK, NULL, &after);
        check("mask", m_blocked && m_old_clear && sigismember(&after, SIGUSR1) == 0,
              "blocked inside, uc_sigmask is the old mask, restored by sigreturn");
}

int main(void)
{
        printf("siginfo: SA_SIGINFO / SA_ONSTACK / sigaltstack\n");
        t_info(); t_sender(); t_regs(); t_fpu(); t_altstack(); t_altquery(); t_mask();
        if (fails)
                printf("siginfo: %d failure(s), FAILURES\n", fails);
        else
                printf("siginfo: all 7 passed\n");
        return fails ? 1 : 0;
}
