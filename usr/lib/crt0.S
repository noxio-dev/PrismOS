/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * Process entry point.  The kernel's ELF loader (user/elf.rs) enters here
 * with rsp 16-byte aligned, pointing at argc, then argv[], NULL, envp[],
 * NULL and the auxiliary vector (System V x86-64 ABI, process start).
 */
	.text
	.globl	_start
	.type	_start, @function
_start:
	xorl	%ebp, %ebp		/* mark the outermost frame */
	movq	%rsp, %rdi		/* -> argc, handed to __prism_start */
	andq	$-16, %rsp
	call	__prism_start
	ud2
	.size	_start, .-_start
	.section .note.GNU-stack,"",@progbits
