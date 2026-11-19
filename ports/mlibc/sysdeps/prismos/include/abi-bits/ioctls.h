#ifndef _ABIBITS_IOCTLS_H
#define _ABIBITS_IOCTLS_H

/* The tty side PrismOS's ioctl (syscall 29, sys/kern/src/user/fd.rs)
 * answers, the words libc itself needs.  The values are the classic Linux
 * ones the kernel expects.  <sys/ioctl.h> adds the rest of the Linux set
 * through <asm/ioctls.h>, and the VT / KD / evdev / DRM numbers live in
 * the UAPI headers the sysroot ships (ports/mlibc/linux-headers).
 *
 * Phase 3.3 grew the set: the termios write side (TCSETS and its drain /
 * flush variants -- what tcsetattr rides), and the foreground pair a
 * shell parks its child on while it waits (the ^C of ISIG goes to it). */

#define TCGETS     0x5401
#define TCSETS     0x5402
#define TCSETSW    0x5403
#define TCSETSF    0x5404
#define TIOCSCTTY  0x540E
#define TIOCGPGRP  0x540F
#define TIOCSPGRP  0x5410
#define TIOCGWINSZ 0x5413

#endif /* _ABIBITS_IOCTLS_H */
