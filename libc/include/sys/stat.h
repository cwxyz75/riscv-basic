#ifndef _OS_SYS_STAT_H
#define _OS_SYS_STAT_H
/* not Linux's struct: the file protocol knows a size and a kind */
struct stat { long long st_size; unsigned st_mode; };
#define S_IFMT 0170000
#define S_IFDIR 0040000
#define S_IFREG 0100000
#define S_ISDIR(m) (((m) & S_IFMT) == S_IFDIR)
#define S_ISREG(m) (((m) & S_IFMT) == S_IFREG)
int stat(const char *path, struct stat *out);
int mkdir(const char *path, unsigned mode);
#endif
