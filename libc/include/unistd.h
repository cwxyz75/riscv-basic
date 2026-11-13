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
int rmdir(const char *path);
#endif
