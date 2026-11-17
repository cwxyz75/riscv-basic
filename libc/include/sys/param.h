#ifndef _OS_SYS_PARAM_H
#define _OS_SYS_PARAM_H
#include <sys/types.h>
#include <limits.h>
#ifndef MAXPATHLEN
#define MAXPATHLEN 4096
#endif
#define MAXHOSTNAMELEN 64
#define NGROUPS_MAX 65536
#define MIN(a, b) (((a) < (b)) ? (a) : (b))
#define MAX(a, b) (((a) > (b)) ? (a) : (b))
#endif
