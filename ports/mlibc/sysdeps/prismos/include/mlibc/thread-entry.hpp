#ifndef MLIBC_THREAD_ENTRY
#define MLIBC_THREAD_ENTRY

/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The two symbols the thread start-up trampoline ties together (phase 4.2):
 * x86_64/thread_entry.S issues the kernel's clone and, in the child, calls
 * generic/thread.cpp's __mlibc_enter_thread on the stack PrepareStack laid
 * out.  The shapes are the ones mlibc's Linux sysdeps use.
 */

#include <mlibc/tcb.hpp>

extern "C" int __mlibc_spawn_thread(unsigned long flags, void *stack, void *pid_out, void *child_tid, void *tcb);
extern "C" void __mlibc_enter_thread(void *entry, void *user_arg);

#endif // MLIBC_THREAD_ENTRY
