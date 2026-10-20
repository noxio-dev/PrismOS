#-
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The program entry point.  PrismOS loads static PIE executables, so the
# address of main must be taken RIP-relative -- the small code model's
# 32-bit immediates do not survive relocation at the loader's base.
#
# The kernel hands _start the System V x86-64 initial stack (argc, argv,
# NULL, envp, NULL, auxv, AT_NULL -- sys/kern/src/user/elf.rs): rsp goes
# to __mlibc_entry as-is.

	.section .text
	.global _start
	.type _start, @function
_start:
	mov %rsp, %rdi            # the initial stack, __mlibc_entry's argument
	lea main(%rip), %rsi      # main, RIP-relative (static PIE)
	call __mlibc_entry        # does not return

	.size _start, . - _start

	.section .note.GNU-stack,"",%progbits
