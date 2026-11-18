#ifndef XALLOC_H
#define XALLOC_H
#include <stddef.h>
void *xmalloc(size_t n);
void *xcalloc(size_t n, size_t m);
void *xrealloc(void *p, size_t n);
char *xstrdup(const char *s);
_Noreturn void xalloc_die(void);
#endif
