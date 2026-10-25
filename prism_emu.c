/* prism_emu.c -- a host-side runner for PrismOS static-PIE userland binaries.
 *
 * The PrismOS userland ABI is fully specified: ET_DYN images mapped at
 * USER_BASE = 1 << 39, R_X86_64_RELATIVE applied by the loader, the SysV
 * initial stack (argc / argv / envp / auxv) built by the kernel, and the
 * syscall convention rax = number, rdi / rsi / rdx / r10 / r8 / r9 = the
 * six arguments, the answer in rax with [-4096, -1] meaning -errno.
 *
 * This tool loads a static PIE into a child process at exactly those
 * addresses, builds exactly that stack, and runs it under PTRACE_SYSCALL.
 * Each PrismOS syscall becomes either a rewritten Linux syscall (the
 * register order and errno convention line up) or a neutralized getpid
 * whose exit rax is overwritten with the emulated answer.
 *
 * Usage: prism_emu <static-pie> [args...]
 * Output: the program's stdout, cube_emu.ppm when it blitted anything, and
 * a PrismOS-style crash line (rip / cr2) when it faults.
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <signal.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/ptrace.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/user.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <time.h>
#include <signal.h>
#include <errno.h>

/* ---- the PrismOS constants the loader and syscall layer live by ------- */

#define PRISM_USER_BASE   0x8000000000ULL          /* vmm.rs: USER_BASE */
#define PRISM_STACK_TOP   0x400000000000ULL        /* vmm.rs: USER_END */
#define PRISM_MMAP_BASE   0x9000000000ULL          /* vmm.rs: USER_MMAP_BASE */
/* the child's scratch spot for planted strings */
#define PRISM_SCRATCH     (PRISM_STACK_TOP - 8192)

enum {                                  /* prismos/syscall.h */
        P_EXIT = 0, P_WRITE = 1, P_GETPID = 2, P_UPTIME_MS = 3, P_YIELD = 4,
        P_SLEEP = 5, P_READ = 13, P_OPEN = 14, P_CLOSE = 15, P_BRK = 21,
        P_MMAP = 22, P_IOCTL = 29, P_ARCH_PRCTL = 36, P_FB_INFO = 44,
        P_FB_BLIT = 45, P_LSEEK = 16,
};

struct prism_fb_info {                  /* prism_raw.h, packed the same */
        uint32_t w, h;
        uint32_t bpp;
        uint32_t r_shift, r_bits;
        uint32_t g_shift, g_bits;
        uint32_t b_shift, b_bits;
};

static struct prism_fb_info FB = { 1280, 800, 32, 16, 8, 8, 8, 0, 8 };

static uint64_t page_up(uint64_t x) { return (x + 4095) & ~4095ULL; }

/* ---- ELF --------------------------------------------------------------- */

struct ph { uint64_t type, flags, off, vaddr, filesz, memsz, align; };

struct image {
        struct ph ph[32];
        int nph;
        uint64_t entry, phoff;
        uint32_t phentsize, phnum;
        uint64_t dyn_vaddr;
        uint8_t *bytes;
        size_t size;
};

static uint64_t u64at(void *p) { uint64_t v; memcpy(&v, p, 8); return v; }

static int load_elf(const char *path, struct image *im)
{
        FILE *f = fopen(path, "rb");
        if (!f) { perror(path); return -1; }
        fseek(f, 0, SEEK_END);
        im->size = ftell(f);
        fseek(f, 0, SEEK_SET);
        im->bytes = malloc(im->size);
        if (!im->bytes || fread(im->bytes, 1, im->size, f) != im->size) {
                fprintf(stderr, "emu: short read\n"); return -1;
        }
        fclose(f);

        uint8_t *e = im->bytes;
        if (memcmp(e, "\x7f""ELF", 4)) { fprintf(stderr, "emu: not an ELF\n"); return -1; }
        im->entry     = u64at(e + 0x18);
        im->phoff     = u64at(e + 0x20);
        im->phentsize = *(uint16_t *)(e + 0x36);
        im->phnum     = *(uint16_t *)(e + 0x38);
        im->dyn_vaddr = 0;
        im->nph = 0;
        for (unsigned i = 0; i < im->phnum; i++) {
                uint8_t *p = e + im->phoff + i * im->phentsize;
                struct ph s;
                s.type   = *(uint32_t *)(p + 0);
                s.flags  = *(uint32_t *)(p + 4);
                s.off    = u64at(p + 8);
                s.vaddr  = u64at(p + 16);
                s.filesz = u64at(p + 32);
                s.memsz  = u64at(p + 40);
                s.align  = u64at(p + 48);
                if (s.type == 2) { im->dyn_vaddr = s.vaddr; continue; }
                if (s.type != 1) continue;
                im->ph[im->nph++] = s;
        }
        return 0;
}

/* ---- child memory helpers (parent side) -------------------------------- */

static pid_t pid;

static int rd_mem(uint64_t addr, void *buf, size_t n)
{
        struct iovec l = { buf, n }, r = { (void *)addr, n };
        return process_vm_readv(pid, &l, 1, &r, 1, 0) == (ssize_t)n ? 0 : -1;
}

static int wr_mem(uint64_t addr, const void *buf, size_t n)
{
        struct iovec l = { (void *)buf, n }, r = { (void *)addr, n };
        return process_vm_writev(pid, &l, 1, &r, 1, 0) == (ssize_t)n ? 0 : -1;
}

/* ---- the emulated screen ------------------------------------------------ */

static uint32_t *screen;
static long blits;

static void blit(uint64_t srcp, uint32_t w, uint32_t h, uint32_t stride,
        int32_t x, int32_t y)
{
        uint32_t *row = malloc((size_t)w * 4);
        if (!row) return;
        for (uint32_t r = 0; r < h; r++) {
                int32_t dy = y + (int32_t)r;
                if (dy < 0 || dy >= (int32_t)FB.h) continue;
                if (rd_mem(srcp + (uint64_t)r * stride, row, (size_t)w * 4) < 0) {
                        free(row); return;
                }
                for (uint32_t c = 0; c < w; c++) {
                        int32_t dx = x + (int32_t)c;
                        if (dx < 0 || dx >= (int32_t)FB.w) continue;
                        screen[(size_t)dy * FB.w + dx] = row[c];
                }
        }
        free(row);
        blits++;
}

static void dump_ppm(const char *path)
{
        if (!blits) return;
        FILE *f = fopen(path, "wb");
        if (!f) return;
        fprintf(f, "P6\n%u %u\n255\n", FB.w, FB.h);
        for (size_t i = 0; i < (size_t)FB.w * FB.h; i++) {
                uint32_t p = screen[i];
                fputc((p >> 16) & 255, f);
                fputc((p >> 8) & 255, f);
                fputc(p & 255, f);
        }
        fclose(f);
        fprintf(stderr, "emu: %ld blits -> %s\n", blits, path);
}

/* ---- the child ---------------------------------------------------------- */

static void run_child(struct image *im, char *arg0)
{
        /* map the image span at USER_BASE (RW, then protect) */
        uint64_t lo = im->ph[0].vaddr, hi = 0;
        for (int i = 0; i < im->nph; i++) {
                uint64_t end = im->ph[i].vaddr + im->ph[i].memsz;
                if (end > hi) hi = end;
        }
        uint8_t *img = mmap((void *)PRISM_USER_BASE, page_up(hi - lo),
                PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        if (img == MAP_FAILED) { perror("emu: mmap image"); _exit(1); }

        for (int i = 0; i < im->nph; i++) {
                memcpy(img + im->ph[i].vaddr, im->bytes + im->ph[i].off, im->ph[i].filesz);
                int prot = ((im->ph[i].flags & 4) ? PROT_READ : 0)
                         | ((im->ph[i].flags & 2) ? PROT_WRITE : 0)
                         | ((im->ph[i].flags & 1) ? PROT_EXEC : 0);
                mprotect((void *)(PRISM_USER_BASE + (im->ph[i].vaddr & ~4095ULL)),
                    page_up(im->ph[i].vaddr + im->ph[i].memsz) - (im->ph[i].vaddr & ~4095ULL), prot);
        }

        /* the kernel's apply_relocs: R_X86_64_RELATIVE out of _DYNAMIC */
        if (im->dyn_vaddr) {
                uint8_t *d = img + im->dyn_vaddr;
                uint64_t rela = 0, relasz = 0;
                for (int i = 0; i < 512; i++) {
                        uint64_t tag = u64at(d + i * 16), val = u64at(d + i * 16 + 8);
                        if (!tag) break;
                        if (tag == 7) rela = val;
                        if (tag == 8) relasz = val;
                }
                uint8_t *rt = img + rela;
                for (uint64_t i = 0; i < relasz / 24; i++) {
                        uint64_t off = u64at(rt + i * 24);
                        uint64_t info = u64at(rt + i * 24 + 8);
                        uint64_t addend = u64at(rt + i * 24 + 16);
                        if ((uint32_t)info == 8) {         /* R_X86_64_RELATIVE */
                                uint64_t val = PRISM_USER_BASE + addend;
                                memcpy(img + off, &val, 8);
                        }
                }
        }

        /* the stack, mapped to PRISM_STACK_TOP like the kernel's exec */
        uint64_t stack_lo = PRISM_STACK_TOP - (8ULL << 20);
        if (mmap((void *)stack_lo, PRISM_STACK_TOP - stack_lo, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == MAP_FAILED) {
                perror("emu: mmap stack"); _exit(1);
        }

        /* build the initial stack (the kernel's build_stack layout) */
        uint64_t sp = PRISM_STACK_TOP - 4096;
        uint64_t scratch = PRISM_SCRATCH;
        memcpy((void *)scratch, "/dev/null", 10); /* the child plants it itself */
        uint64_t rnd = sp - 16; sp -= 16;           /* AT_RANDOM bytes */
        unsigned char rndb[16];
        for (int i = 0; i < 16; i++) rndb[i] = rand();
        memcpy((void *)rnd, rndb, 16);
        uint64_t astr = sp; sp -= strlen(arg0) + 1; /* argv[0] */
        memcpy((void *)astr, arg0, strlen(arg0) + 1);
        char execfn[] = "/emu";
        uint64_t fstr = sp; sp -= sizeof execfn;
        memcpy((void *)fstr, execfn, sizeof execfn);

        sp &= ~15ULL;
        uint64_t auxv[] = {
                3 /*AT_PHDR*/,    PRISM_USER_BASE + im->phoff,
                4 /*AT_PHENT*/,   im->phentsize,
                5 /*AT_PHNUM*/,   im->phnum,
                6 /*AT_PAGESZ*/,  4096,
                7 /*AT_BASE*/,    0,
                8 /*AT_FLAGS*/,   0,
                9 /*AT_ENTRY*/,   PRISM_USER_BASE + im->entry,
                11 /*AT_UID*/, 0, 12 /*AT_EUID*/, 0,
                13 /*AT_GID*/, 0, 14 /*AT_EGID*/, 0,
                17 /*AT_CLKTCK*/, 100,
                23 /*AT_SECURE*/, 0,
                25 /*AT_RANDOM*/, rnd,
                31 /*AT_EXECFN*/, fstr,
                0, 0,
        };
        uint64_t words[128];
        int nw = 0;
        words[nw++] = 1;                    /* argc */
        words[nw++] = astr;                 /* argv[0] */
        words[nw++] = 0;                    /* argv end */
        words[nw++] = 0;                    /* envp end */
        for (unsigned i = 0; i < sizeof auxv / 8; i++)
                words[nw++] = auxv[i];
        sp -= (uint64_t)nw * 8;
        sp &= ~15ULL;
        memcpy((void *)sp, words, (size_t)nw * 8);

        ptrace(PTRACE_TRACEME, 0, 0, 0);
        kill(getpid(), SIGSTOP);

        /* switch stacks, stash the scratch address in r15, enter */
        __asm__ volatile(
            "mov %0, %%rsp\n\t"
            "mov %1, %%r15\n\t"
            "jmp *%2\n\t"
            :: "r"(sp), "r"(scratch), "r"(PRISM_USER_BASE + im->entry)
            : "memory");
        __builtin_unreachable();
}

/* ---- the parent's syscall server ---------------------------------------- */

static int in_syscall;                 /* entry (0) vs exit (1) stop */
static uint64_t pending_answer;        /* for the neutralized calls */
static int neutralized;
static uint64_t mmap_next = PRISM_MMAP_BASE;
static uint64_t scratch_addr = PRISM_SCRATCH;

static long now_ms(void)
{
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

static int trace;

static void serve_entry(struct user_regs_struct *r)
{
        neutralized = 0;
        /* At an entry stop the syscall number lives in orig_rax; rax is
         * -ENOSYS until the call runs.  Redirecting the call = orig_rax. */
        long nr = (long)r->orig_rax;
        if (trace)
                fprintf(stderr, "[in ] nr=%ld a0=0x%llx a1=0x%llx a2=0x%llx a3=0x%llx\n",
                        nr, (unsigned long long)r->rdi, (unsigned long long)r->rsi,
                        (unsigned long long)r->rdx, (unsigned long long)r->r10);
        switch (nr) {
        /* ---- passthrough rewrites: the Linux ABIs line up ---- */
        case P_EXIT:       r->orig_rax = 60;  break;   /* exit(status) */
        case P_WRITE:      r->orig_rax = 1;   break;   /* write: same regs */
        case P_READ:       r->orig_rax = 0;   break;   /* read: same regs */
        case P_CLOSE:      r->orig_rax = 3;   break;   /* close(fd) */
        case P_ARCH_PRCTL: r->orig_rax = 158; break;   /* arch_prctl: same regs */
        case P_GETPID:     r->orig_rax = 39;  break;   /* getpid() */
        case P_OPEN: {                          /* open(path, flags): no mode! */
                char path[256];
                if (rd_mem(r->rdi, path, sizeof path - 1) == 0) {
                        path[255] = 0;
                        if (!strcmp(path, "/dev/console"))
                                r->rdi = scratch_addr;  /* the planted "/dev/null" */
                }
                r->orig_rax = 2;
                break;
        }
        case P_MMAP: {                          /* mmap: same 6-arg order */
                uint64_t addr = mmap_next;
                mmap_next = addr + page_up(r->rsi);
                r->rdi = addr;
                r->r10 = (r->r10 & ~0x10ULL) | 0x10;  /* force MAP_FIXED */
                r->orig_rax = 9;
                break;
        }
        /* ---- neutralized: run getpid, answer at exit ---- */
        case P_UPTIME_MS: pending_answer = (uint64_t)now_ms(); neutralized = 1; r->orig_rax = 39; break;
        case P_YIELD:     pending_answer = 0;        neutralized = 1; r->orig_rax = 39; break;
        case P_SLEEP:     pending_answer = r->rdi;   neutralized = 1; r->orig_rax = 39; break;
        case P_BRK:       pending_answer = r->rdi;   neutralized = 1; r->orig_rax = 39; break;
        case P_IOCTL:     /* isatty's TCGETS: "not a tty" is the honest answer,
                             * and what makes mlibc's stdio go fully buffered */
                          pending_answer = (uint64_t)-25; neutralized = 1; r->orig_rax = 39; break;
        case P_LSEEK:     /* stdio's seekability probe: the offset asked for,
                             * as if the console were a regular file */
                          pending_answer = r->rsi;    neutralized = 1; r->orig_rax = 39; break;
        case P_FB_INFO:
                wr_mem(r->rdi, &FB, sizeof FB);
                pending_answer = 0; neutralized = 1; r->orig_rax = 39; break;
        case P_FB_BLIT: {
                uint32_t w = r->rsi, h = r->rdx, stride = r->r10;
                blit(r->rdi, w, h, stride, (int32_t)r->r8, (int32_t)r->r9);
                pending_answer = 0; neutralized = 1; r->orig_rax = 39; break;
        }
        default:
                fprintf(stderr, "emu: unhandled syscall %ld -> ENOSYS\n", nr);
                pending_answer = (uint64_t)-38; neutralized = 1; r->orig_rax = 39;
                break;
        }
}

static void serve_exit(struct user_regs_struct *r)
{
        if (neutralized)
                r->rax = pending_answer;
        if (trace)
                fprintf(stderr, "[out] rax=%lld (0x%llx)\n", (long long)r->rax,
                        (unsigned long long)r->rax);
}

/* the child plants "/dev/null" in its own scratch page at a fixed spot */

static volatile sig_atomic_t alrm;

static void on_alrm(int sig) { (void)sig; alrm = 1; }

int main(int argc, char **argv)
{
        struct sigaction sa = {0};
        sa.sa_handler = on_alrm;
        sigaction(SIGALRM, &sa, NULL); /* no SA_RESTART: the wait unwinds */
        alarm(12);                     /* the cube spins forever: 12 s of frames */
        if (argc < 2) {
                fprintf(stderr, "usage: %s [-v] <static-pie> [args...]\n", argv[0]);
                return 2;
        }
        if (!strcmp(argv[1], "-v")) {
                trace = 1;
                if (argc > 2) { argv++; argc--; }
        }
        struct image im;
        if (load_elf(argv[1], &im) < 0)
                return 2;

        screen = calloc(FB.w, FB.h * 4);

        pid = fork();
        if (pid == 0)
                run_child(&im, argv[1]);

        int st;
        waitpid(pid, &st, 0);                    /* the SIGSTOP after setup */

        struct user_regs_struct r;
        scratch_addr = PRISM_SCRATCH;           /* the child planted it itself */

        ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD);

        int rc = 0;
        for (;;) {
                if (alrm) { /* the deadline: wrap up */
                        kill(pid, SIGKILL);
                        fprintf(stderr, "emu: stopped by the alarm\n");
                        break;
                }
                if (ptrace(PTRACE_SYSCALL, pid, 0, 0) < 0) break;
                if (waitpid(pid, &st, 0) < 0) {
                        if (errno == EINTR) continue; /* the alarm flag exits the loop */
                        break;
                }
                if (WIFEXITED(st)) {
                        fprintf(stderr, "emu: exited %d\n", WEXITSTATUS(st));
                        break;
                }
                if (WIFSIGNALED(st)) {
                        fprintf(stderr, "emu: killed by %d\n", WTERMSIG(st));
                        break;
                }
                int sig = WSTOPSIG(st);
                if (sig == (SIGTRAP | 0x80)) {
                        ptrace(PTRACE_GETREGS, pid, 0, &r);
                        if (!in_syscall) serve_entry(&r);
                        else serve_exit(&r);
                        ptrace(PTRACE_SETREGS, pid, 0, &r);
                        in_syscall = !in_syscall;
                        continue;
                }
                if (sig == SIGSEGV || sig == SIGBUS || sig == SIGILL || sig == SIGFPE) {
                        siginfo_t si;
                        ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
                        ptrace(PTRACE_GETREGS, pid, 0, &r);
                        int exn = (sig == SIGSEGV) ? 14 : (sig == SIGBUS) ? 7
                                : (sig == SIGILL) ? 6 : 13;
                        const char *acc = (si.si_code == SEGV_ACCERR) ?
                                "page present, access refused" :
                                (si.si_code == SEGV_MAPERR) ? "page not mapped" : "";
                        fprintf(stderr,
                                "killed by exception %d at rip 0x%llx (cr2 0x%llx, %s)\n"
                                "  [rip - image base 0x%llx, cr2 - image base 0x%llx]\n",
                                exn, (unsigned long long)r.rip,
                                (unsigned long long)si.si_addr, acc,
                                (unsigned long long)(r.rip - PRISM_USER_BASE),
                                (unsigned long long)((uint64_t)si.si_addr - PRISM_USER_BASE));
                        kill(pid, SIGKILL);
                        rc = 42;
                        break;
                }
                /* anything else: swallow (the harness runs to a fault or exit) */
        }
        dump_ppm("cube_emu.ppm");
        return rc;
}
