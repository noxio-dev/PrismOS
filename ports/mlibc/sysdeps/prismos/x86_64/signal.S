#-
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The SA_RESTORER PrismOS's sigaction expects (sys/kern/src/user/
# signal.rs): the return address mlibc plants in every action, so a
# signal handler's plain `ret` lands here and makes the sigreturn
# syscall (12) that restores the interrupted frame register for
# register.  There is no siginfo variant -- SA_SIGINFO is refused by
# the kernel -- so one symbol serves every action.

	.section .text
	.global __mlibc_signal_restore
	.hidden __mlibc_signal_restore
	.type __mlibc_signal_restore, @function
__mlibc_signal_restore:
	mov $12, %rax             # SYS_sigreturn
	syscall
	ud2                       # sigreturn does not come back

	.size __mlibc_signal_restore, . - __mlibc_signal_restore

	.section .note.GNU-stack,"",%progbits
