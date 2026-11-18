#ifndef _OS_SYS_UIO_H
#define _OS_SYS_UIO_H
#include <stddef.h>
#include <sys/types.h>
struct iovec { void *iov_base; size_t iov_len; };
#define IOV_MAX 1024
ssize_t readv(int fd, const struct iovec *iov, int iovcnt);
ssize_t writev(int fd, const struct iovec *iov, int iovcnt);
#endif
