#ifndef _OS_SCHED_H
#define _OS_SCHED_H
#include <sys/types.h>
int sched_yield(void);
/* One word of CPUs is enough for the machines this runs on. */
typedef struct { unsigned long __bits[16]; } cpu_set_t;
#define CPU_COUNT(set) __cpu_count(set)
int __cpu_count(const cpu_set_t *set);
int sched_getaffinity(pid_t pid, size_t size, cpu_set_t *set);
#endif
