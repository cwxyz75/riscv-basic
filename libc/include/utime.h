#ifndef _OS_UTIME_H
#define _OS_UTIME_H
#include <sys/types.h>
struct utimbuf { time_t actime; time_t modtime; };
/* file times cannot be set yet (the file protocol has no call for it): ENOSYS */
int utime(const char *path, const struct utimbuf *times);
#endif
