#ifndef _OS_SYS_STAT_H
#define _OS_SYS_STAT_H
#include <sys/types.h>
#include <time.h>
#include <stdint.h>
/* The mode, owner and group are advisory attributes (or defaults); nothing enforces them. st_nlink is always 1; st_dev and
 * st_ino are opaque tokens, stable within a mount. */
struct stat {
    unsigned long long st_dev;
    unsigned long long st_ino;
    unsigned int st_mode;
    unsigned int st_nlink;
    unsigned int st_uid;
    unsigned int st_gid;
    unsigned long long st_rdev;
    long long st_size;
    long long st_blksize;
    long long st_blocks;
    struct timespec st_atim, st_mtim, st_ctim;
};
#define st_atime st_atim.tv_sec
#define st_mtime st_mtim.tv_sec
#define st_ctime st_ctim.tv_sec
#define S_IFMT 0170000
#define S_IFDIR 0040000
#define S_IFCHR 0020000
#define S_IFBLK 0060000
#define S_IFIFO 0010000
#define S_IFREG 0100000
#define S_IFLNK 0120000
#define S_IFSOCK 0140000
#define S_ISDIR(m) (((m) & S_IFMT) == S_IFDIR)
#define S_ISREG(m) (((m) & S_IFMT) == S_IFREG)
#define S_ISLNK(m) (((m) & S_IFMT) == S_IFLNK)
#define S_ISCHR(m) (((m) & S_IFMT) == S_IFCHR)
#define S_ISFIFO(m) (((m) & S_IFMT) == S_IFIFO)
#define S_ISSOCK(m) (((m) & S_IFMT) == S_IFSOCK)
#define S_ISUID 04000
#define S_ISGID 02000
#define S_ISVTX 01000
#define S_IRWXU 0700
#define S_IRUSR 0400
#define S_IWUSR 0200
#define S_IXUSR 0100
#define S_IRWXG 0070
#define S_IRGRP 0040
#define S_IWGRP 0020
#define S_IXGRP 0010
#define S_IRWXO 0007
#define S_IROTH 0004
#define S_IWOTH 0002
#define S_IXOTH 0001
/* (the file offsets are 64 bits already: the `64` names are the same calls) */
#define stat64 stat
#define fstat64 fstat
#define lstat64 lstat
int stat(const char *path, struct stat *out);
int lstat(const char *path, struct stat *out);
int fstat(int fd, struct stat *out);
int mkdir(const char *path, unsigned mode);
int fstatat(int dirfd, const char *path, struct stat *out, int flags);
int mkdirat(int dirfd, const char *path, unsigned mode);
int fchmodat(int dirfd, const char *path, unsigned mode, int flags);
int mkfifoat(int dirfd, const char *path, unsigned mode);
int mknodat(int dirfd, const char *path, unsigned mode, unsigned long long dev);
int chmod(const char *path, unsigned mode);
int fchmod(int fd, unsigned mode);
unsigned umask(unsigned mask);
#define S_ISBLK(m) (((m) & S_IFMT) == S_IFBLK)
int mkfifo(const char *path, mode_t mode);
int mknod(const char *path, mode_t mode, dev_t dev);
int futimens(int fd, const struct timespec times[2]);
int utimensat(int dirfd, const char *path, const struct timespec times[2], int flags);
#endif
