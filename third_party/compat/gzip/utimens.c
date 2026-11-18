#include <sys/stat.h>
#include <time.h>
#include "utimens.h"
/* The file systems keep no settable times (`utimensat` is ENOSYS, TIDY-UP T144), so keeping a file's times is skipped, not an error. */
int fdutimens(int fd, const char *file, const struct timespec ts[2]) {
    (void)fd; (void)file; (void)ts;
    return 0;
}

#include <errno.h>
/* gzip only calls this when it opened a directory with openat, which it does not do here (HAVE_OPENAT is 0). */
int unlinkat(int dirfd, const char *name, int flags) {
    (void)dirfd; (void)name; (void)flags;
    errno = 38;
    return -1;
}

#include <dirent.h>
/* (same: only reached after an openat-opened directory) */
DIR *fdopendir(int fd) {
    (void)fd;
    errno = 38;
    return 0;
}
