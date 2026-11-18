#ifndef _OS_ALLOCA_H
#define _OS_ALLOCA_H
#include <stddef.h>
#define alloca(n) __builtin_alloca(n)
#endif
