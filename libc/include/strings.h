#ifndef _OS_STRINGS_H
#define _OS_STRINGS_H
#include <stddef.h>
int bcmp(const void *a, const void *b, size_t n);
void bcopy(const void *src, void *dst, size_t n);
char *index(const char *s, int c);
char *rindex(const char *s, int c);
int strcasecmp(const char *a, const char *b);
int strncasecmp(const char *a, const char *b, size_t n);
#endif
