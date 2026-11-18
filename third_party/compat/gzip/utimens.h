#ifndef UTIMENS_H
#define UTIMENS_H
#include <time.h>
int fdutimens(int fd, const char *file, const struct timespec ts[2]);
#endif
