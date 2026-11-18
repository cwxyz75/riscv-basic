#ifndef TIMESPEC_H
#define TIMESPEC_H
#include <time.h>
static inline int timespec_cmp(struct timespec a, struct timespec b) {
    return a.tv_sec < b.tv_sec ? -1 : a.tv_sec > b.tv_sec ? 1 : (a.tv_nsec > b.tv_nsec) - (a.tv_nsec < b.tv_nsec);
}
#endif
