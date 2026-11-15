#ifndef _OS_SYS_RANDOM_H
#define _OS_SYS_RANDOM_H
#include <stddef.h>
/* Bytes from the system random service (/svc/random): at most 256 per call. EACCES if the program has no `random` name,
 * EAGAIN if the machine has no seeded generator. */
long getrandom(void *buf, size_t len, unsigned flags);
#endif
