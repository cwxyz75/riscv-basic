#ifndef _OS_LIMITS_H
#define _OS_LIMITS_H
#include_next <limits.h>
#define PATH_MAX 4096
#define NAME_MAX 255
#define PIPE_BUF 4096
#define ARG_MAX 131072
#define LINE_MAX 2048
#define NGROUPS_MAX 65536
#define SSIZE_MAX LONG_MAX
#define IOV_MAX 1024
#endif
