#ifndef _OS_FCNTL_H
#define _OS_FCNTL_H
#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR 2
#define O_CREAT 0x40
#define O_EXCL 0x80
#define O_TRUNC 0x200
#define O_DIRECTORY 0x10000
/* O_SEARCH: open a directory only to resolve names below it (here: a read-only open) */
#define O_SEARCH 0
#define AT_FDCWD (-100)
#define O_APPEND 0x400
#define O_NONBLOCK 04000
#define O_CLOEXEC 02000000
#define F_GETFD 1
#define F_SETFD 2
#define F_GETFL 3
#define F_SETFL 4
#define FD_CLOEXEC 1
int fcntl(int fd, int cmd, ...);
int open(const char *path, int flags, ...);
int openat(int dirfd, const char *path, int flags, ...);
/* advisory record locks (SQLite): the library answers every request as granted, nothing is enforced between processes (work item T142) */
#define F_GETLK 5
#define F_SETLK 6
#define F_SETLKW 7
#define F_RDLCK 0
#define F_WRLCK 1
#define F_UNLCK 2
struct flock { short l_type; short l_whence; long l_start; long l_len; int l_pid; };
#define F_DUPFD 0
#define F_DUPFD_CLOEXEC 1030
#define O_NOCTTY 0400
#define O_NOFOLLOW 0400000
#define O_SYNC 04010000
#define F_GETLK 5
#define F_SETLK 6
#define F_SETLKW 7
#define AT_EACCESS 0x200
#define AT_SYMLINK_NOFOLLOW 0x100
#define AT_REMOVEDIR 0x200
#define AT_SYMLINK_FOLLOW 0x400
#define UTIME_NOW ((1l << 30) - 1l)
#define UTIME_OMIT ((1l << 30) - 2l)
#endif
