#ifndef _OS_SYS_SELECT_H
#define _OS_SYS_SELECT_H
#define FD_SETSIZE 256
typedef struct { unsigned long fds_bits[FD_SETSIZE / 64]; } fd_set;
#define FD_ZERO(s) do { for (int _i = 0; _i < FD_SETSIZE / 64; _i++) (s)->fds_bits[_i] = 0; } while (0)
#define FD_SET(fd, s) ((s)->fds_bits[(fd) / 64] |= 1ul << ((fd) % 64))
#define FD_CLR(fd, s) ((s)->fds_bits[(fd) / 64] &= ~(1ul << ((fd) % 64)))
#define FD_ISSET(fd, s) (((s)->fds_bits[(fd) / 64] >> ((fd) % 64)) & 1)
struct timeval { long tv_sec; long tv_usec; };
int select(int n, fd_set *r, fd_set *w, fd_set *e, struct timeval *tv);
#endif
