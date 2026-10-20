#ifndef _ABIBITS_IOCTLS_H
#define _ABIBITS_IOCTLS_H

/* The tty pair PrismOS's ioctl (syscall 29, sys/kern/src/user/fd.rs)
 * actually answers; every other request is -ENOTTY, so nothing else is
 * defined.  The values are the classic Linux ones the kernel expects. */

#define TCGETS     0x5401
#define TIOCGWINSZ 0x5413

#endif /* _ABIBITS_IOCTLS_H */
