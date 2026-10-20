#-
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The _init / _fini epilogues (the classic crtn pair).

	.section .init
	pop %rax
	ret

	.section .fini
	pop %rax
	ret

	.section .note.GNU-stack,"",%progbits
