/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * Threads, the PrismOS sysdeps (phase 4.2): the three tags mlibc's
 * pthread_create stands on -- PrepareStack (a stack for the new thread),
 * Clone (the kernel's thread clone) and ThreadExit (end the caller only) --
 * plus the C entry the trampoline in x86_64/thread_entry.S calls.  Compiled
 * into libc only: ld.so has no use for them.  The shapes follow mlibc's
 * Linux sysdeps, the kernel taking the Linux clone's argument order
 * (sys/kern/src/user/{process,thread_core}.rs).
 *
 * What a program needs to know:
 *   * pthread_create needs the dynamically linked half of the library: the
 *     TCB of the new thread is allocated by ld.so (__rtld_allocateTcb), so
 *     a thread program is linked like hello-dyn, against libc.so;
 *   * a thread's id is the kernel's thread id (gettid); getpid() answers
 *     the whole group's; exit() ends every thread, pthread_exit() one;
 *   * cancellation is not supported (SIGCANCEL needs SA_SIGINFO).
 */

#include <abi-bits/clone-flags.h>
#include <abi-bits/vm-flags.h>
#include <bits/ensure.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>

#include <mlibc/all-sysdeps.hpp>
#include <mlibc/tcb.hpp>
#include <mlibc/thread.hpp>
#include <mlibc/thread-entry.hpp>
#include <prismos/syscall.h>

extern "C" void __mlibc_enter_thread(void *entry, void *user_arg) {
	// The kernel already installed the TCB as the FS base (CLONE_SETTLS).
	auto tcb = mlibc::get_current_tcb();

	// Wait until our parent has stored the thread id in the TCB.
	while(!__atomic_load_n(&tcb->tid, __ATOMIC_RELAXED))
		mlibc::sysdep<FutexWait>(&tcb->tid, 0, nullptr);

	// Cancellation can be enabled once the TCB is up.
	__atomic_fetch_or(&tcb->cancelBits, tcbCancelEnableBit, __ATOMIC_RELAXED);

	tcb->invokeThreadFunc(entry, user_arg);

	mlibc::thread_exit(tcb->returnValue);
}

namespace mlibc {

static constexpr size_t default_stacksize = 0x200000;

int Sysdeps<PrepareStack>::operator()(void **stack, void *entry, void *user_arg, void *tcb,
		size_t *stack_size, size_t *guard_size, void **stack_base) {
	(void)tcb;
	if(!*stack_size)
		*stack_size = default_stacksize;

	void *map;
	if(*stack) {
		// The caller brought its own stack: no guard of ours.
		map = *stack;
		*guard_size = 0;
	} else {
		// Reserve guard + stack PROT_NONE (the kernel maps such pages
		// supervisor-only, so a touch in the guard faults), then open the
		// stack proper with mprotect.
		auto r = sysdep<VmMap>(nullptr, *stack_size + *guard_size, PROT_NONE,
				MAP_PRIVATE | MAP_ANONYMOUS, -1, 0, &map);
		if(r || map == MAP_FAILED)
			return EAGAIN;
		int ret = sysdep_or_enosys<VmProtect>(
				reinterpret_cast<void *>(reinterpret_cast<uintptr_t>(map) + *guard_size),
				*stack_size, PROT_READ | PROT_WRITE);
		if(ret)
			return EAGAIN;
	}

	*stack_base = map;
	auto sp = reinterpret_cast<uintptr_t *>(reinterpret_cast<uintptr_t>(map) + *guard_size + *stack_size);
	// What the trampoline pops: entry first, then the argument.
	*--sp = reinterpret_cast<uintptr_t>(user_arg);
	*--sp = reinterpret_cast<uintptr_t>(entry);
	*stack = reinterpret_cast<void *>(sp);
	return 0;
}

int Sysdeps<Clone>::operator()(void *tcb, pid_t *pid_out, void *stack) {
	// The thread flag word the kernel's parse_clone serves
	// (sys/kern/src/user/thread_core.rs).  CLONE_PARENT_SETTID stores the new
	// thread id at pid_out before the child can run; the child then waits for
	// the parent to copy it into the TCB (__mlibc_enter_thread above).
	unsigned long flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND
			| CLONE_THREAD | CLONE_SYSVSEM | CLONE_SETTLS | CLONE_PARENT_SETTID;
	auto ret = __mlibc_spawn_thread(flags, stack, pid_out, nullptr, tcb);
	if(ret < 0)
		return -ret;
	return 0;
}

[[noreturn]] void Sysdeps<ThreadExit>::operator()() {
	// Ends the calling thread only; the process lives on until its last
	// thread goes (the kernel's thread_exit, syscall 106).
	__do_syscall0(PRISM_SYS_THREAD_EXIT);
	__builtin_trap();
}

} // namespace mlibc
