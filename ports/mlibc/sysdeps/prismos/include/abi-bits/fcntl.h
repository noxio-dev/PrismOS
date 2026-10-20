#ifndef _ABIBITS_FCNTL_H
#define _ABIBITS_FCNTL_H

#include <abi-bits/pid_t.h>
#include <mlibc-config.h>

/* PrismOS open() flags: the eight the kernel understands are its own O_*
 * values (sys/kern/src/fs/vfs.rs), not Linux's octal encodings; the
 * kernel refuses unknown bits (-EINVAL, user/fd.rs), so the rest of the
 * POSIX names below are reference constants for generic library code --
 * an open() carrying one answers EINVAL, honestly. */

#define O_RDONLY   01
#define O_WRONLY   02
#define O_RDWR     03
#define O_ACCMODE  03

/* The kernel's own bit assignments. */
#define O_CREAT         04
#define O_TRUNC        010
#define O_APPEND       020
#define O_EXCL         040
#define O_NONBLOCK    0100
#define O_CLOEXEC     0200

/* Reference constants (refused by the kernel if actually passed). */
#define O_NOCTTY      0x100
#define O_SYNC        0x200
#define O_ASYNC       0x400
#define O_DIRECT      0x800
#define O_LARGEFILE  0x1000
#define O_DIRECTORY  0x2000
#define O_NOFOLLOW   0x4000
#define O_NOATIME    0x8000
#define O_PATH      0x10000
#define O_TMPFILE   0x20000
#define O_DSYNC     0x40000
#define O_RSYNC     O_SYNC

#define O_EXEC O_PATH
#define O_SEARCH O_PATH

#define O_TTY_INIT 0

/* fcntl() commands: F_DUPFD .. F_SETFL are the ones the kernel's fcntl
 * (sys/kern/src/user/fd.rs) switches on; the rest are reference
 * constants -- the kernel answers EINVAL for an unknown command. */
#define F_DUPFD  0
#define F_GETFD  1
#define F_SETFD  2
#define F_GETFL  3
#define F_SETFL  4

#define F_SETOWN 8
#define F_GETOWN 9
#define F_SETSIG 10
#define F_GETSIG 11

#define F_GETLK 5
#define F_SETLK 6
#define F_SETLKW 7
#define F_GETLK64 F_GETLK
#define F_SETLK64 F_SETLK
#define F_SETLKW64 F_SETLKW

#define F_SETOWN_EX 15
#define F_GETOWN_EX 16

#define F_GETOWNER_UIDS 17

#define F_SETLEASE 1024
#define F_GETLEASE 1025
#define F_NOTIFY 1026
#define F_DUPFD_CLOEXEC 1030
#define F_SETPIPE_SZ 1031
#define F_GETPIPE_SZ 1032
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034

#define F_SEAL_SEAL (1 << 0)
#define F_SEAL_SHRINK (1 << 1)
#define F_SEAL_GROW (1 << 2)
#define F_SEAL_WRITE (1 << 3)

#define F_OFD_GETLK 36
#define F_OFD_SETLK 37
#define F_OFD_SETLKW 38

/* The kernel has no file locks; these ride along fcntl's lock commands,
 * which answer EINVAL. */
#define F_RDLCK 0
#define F_WRLCK 1
#define F_UNLCK 2

#define FD_CLOEXEC 1

/* *at() conventions: the kernel has no *at syscalls, so only AT_FDCWD
 * (and, on top of it, the flag Unlinkat understands) ever resolves; the
 * rest are reference constants for the generic library code. */
#define AT_FDCWD (-100)
#define AT_SYMLINK_NOFOLLOW 0x100
#define AT_REMOVEDIR 0x200
#define AT_SYMLINK_FOLLOW 0x400
#define AT_EACCESS 0x200
#define AT_EMPTY_PATH 0x1000

#if defined(_GNU_SOURCE) || __MLIBC_POSIX2024
struct f_owner_ex {
	int type;
	pid_t pid;
};
#endif /* defined(_GNU_SOURCE) || __MLIBC_POSIX2024 */

#define F_OWNER_TID 0
#define F_OWNER_PID 1
#define F_OWNER_PGRP 2

#define POSIX_FADV_NORMAL 0
#define POSIX_FADV_RANDOM 1
#define POSIX_FADV_SEQUENTIAL 2
#define POSIX_FADV_WILLNEED 3
#define POSIX_FADV_DONTNEED 4
#define POSIX_FADV_NOREUSE 5

/* Permission bits (nothing checks them yet -- phase 3.5 -- but they ride
 * along mkdir / open mode arguments and struct stat). */
#define S_IRWXU 0700
#define S_IRUSR 0400
#define S_IWUSR 0200
#define S_IXUSR 0100
#define S_IRWXG 070
#define S_IRGRP 040
#define S_IWGRP 020
#define S_IXGRP 010
#define S_IRWXO 07
#define S_IROTH 04
#define S_IWOTH 02
#define S_IXOTH 01
#define S_ISUID 04000
#define S_ISGID 02000
#define S_ISVTX 01000

#endif /* _ABIBITS_FCNTL_H */
