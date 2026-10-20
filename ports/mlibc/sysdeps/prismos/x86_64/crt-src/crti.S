#-
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The _init / _fini prologues (the classic crti pair): a frame for the
# .init_array / .fini_array walkers.

	.section .init
	.global _init
	.type _init, @function
_init:
	push %rax

	.section .fini
	.global _fini
	.type _fini, @function
_fini:
	push %rax

	.section .note.GNU-stack,"",%progbits
