#ifndef _OS_UNISTD_H
#define _OS_UNISTD_H
#include <stddef.h>
typedef long ssize_t;
typedef long off_t;
ssize_t read(int fd, void *buf, size_t n);
ssize_t write(int fd, const void *buf, size_t n);
int close(int fd);
off_t lseek(int fd, off_t off, int whence);
int isatty(int fd);
unsigned sleep(unsigned seconds);
int usleep(unsigned usec);
int unlink(const char *path);
char *getcwd(char *buf, size_t size);
int chdir(const char *path);
int access(const char *path, int mode);
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4
long readlink(const char *path, char *buf, size_t size);
int symlink(const char *target, const char *linkpath);
int link(const char *oldpath, const char *newpath);
int chown(const char *path, int uid, int gid);
int fchown(int fd, int uid, int gid);
int rmdir(const char *path);
#endif
