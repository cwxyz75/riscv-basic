#ifndef _OS_ERRNO_H
#define _OS_ERRNO_H
int *__errno_location(void);
#define errno (*__errno_location())
#define EPERM 1
#define EAGAIN 11
#define EBUSY 16
#define ENOTTY 25
#define ESPIPE 29
#define EPIPE 32
#define EDEADLK 35
#define ENAMETOOLONG 36
#define ENOTEMPTY 39
#define EOVERFLOW 75
#define ETIMEDOUT 110
#define ENOENT 2
#define EIO 5
#define EBADF 9
#define ENOMEM 12
#define EACCES 13
#define EEXIST 17
#define ENOTDIR 20
#define EISDIR 21
#define EINVAL 22
#define EMFILE 24
#define ENOSPC 28
#define EDOM 33
#define ERANGE 34
#define ENOSYS 38
#endif
