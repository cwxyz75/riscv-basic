#ifndef _OS_SYS_TIME_H
#define _OS_SYS_TIME_H
#include <sys/types.h>
#include <time.h>
struct timeval { time_t tv_sec; suseconds_t tv_usec; };
struct timezone { int tz_minuteswest; int tz_dsttime; };
struct itimerval { struct timeval it_interval; struct timeval it_value; };
int gettimeofday(struct timeval *tv, struct timezone *tz);
int settimeofday(const struct timeval *tv, const struct timezone *tz);
int utimes(const char *path, const struct timeval times[2]);
#endif
