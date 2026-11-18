#include <stdlib.h>
#include <string.h>
#include "xalloc.h"
void *xmalloc(size_t n) { void *p = malloc(n ? n : 1); if (!p) xalloc_die(); return p; }
void *xcalloc(size_t n, size_t m) { void *p = calloc(n ? n : 1, m ? m : 1); if (!p) xalloc_die(); return p; }
void *xrealloc(void *q, size_t n) { void *p = realloc(q, n ? n : 1); if (!p) xalloc_die(); return p; }
char *xstrdup(const char *s) { size_t n = strlen(s) + 1; char *p = xmalloc(n); memcpy(p, s, n); return p; }
