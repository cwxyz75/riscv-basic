#ifndef STAT_TIME_H
#define STAT_TIME_H
#include <sys/stat.h>
#include <time.h>
static inline struct timespec get_stat_atime(const struct stat *st) { return st->st_atim; }
static inline struct timespec get_stat_mtime(const struct stat *st) { return st->st_mtim; }
#endif
