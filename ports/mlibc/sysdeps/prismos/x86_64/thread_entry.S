#-
# SPDX-License-Identifier: GPL-3.0-or-later
#
# int __mlibc_spawn_thread(flags, stack, pid_out, child_tid, tcb)
#                          rdi    rsi    rdx      rcx        r8
#
# Phase 4.2: the kernel's clone (syscall 104, sys/kern/src/user/process.rs)
# takes the Linux x86-64 register order -- flags rdi, stack rsi, ptid rdx,
# ctid r10, tls r8 -- so only the fourth argument moves (rcx is clobbered
# by `syscall`, the kernel reads it from r10).  The parent gets the new
# thread id (or -errno) and returns it.  The child comes back with rax = 0
# on the stack PrepareStack prepared -- [entry][user_arg] at its top -- its
# FS base already the TCB (CLONE_SETTLS), and falls into
# __mlibc_enter_thread(entry, user_arg).  The two pops leave rsp 16-byte
# aligned, so the call hands the callee the ABI's "rsp + 8" alignment.

	.section .text
	.global __mlibc_spawn_thread
	.type __mlibc_spawn_thread, @function
__mlibc_spawn_thread:
	mov %rcx, %r10            # child_tid -> the kernel's fourth argument
	mov $104, %eax            # PRISM_SYS_CLONE
	syscall
	test %eax, %eax
	jnz 1f
	xor %ebp, %ebp            # the outermost frame: end of backtraces
	pop %rdi                  # entry
	pop %rsi                  # user_arg
	call __mlibc_enter_thread
	ud2                       # a thread's end is thread_exit, not a return
1:
	ret
	.size __mlibc_spawn_thread, . - __mlibc_spawn_thread

	.section .note.GNU-stack,"",%progbits
