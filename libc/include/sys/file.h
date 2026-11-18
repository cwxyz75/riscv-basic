#ifndef _OS_SYS_FILE_H
#define _OS_SYS_FILE_H
#include <fcntl.h>
#define LOCK_SH 1
#define LOCK_EX 2
#define LOCK_NB 4
#define LOCK_UN 8
/* Advisory locks do nothing here: the file servers do not share a file between programs with a lock (always succeeds). */
int flock(int fd, int operation);
#endif
