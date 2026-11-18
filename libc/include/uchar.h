#ifndef _OS_UCHAR_H
#define _OS_UCHAR_H
#include <wchar.h>
typedef unsigned short char16_t;
typedef unsigned int char32_t;
size_t mbrtoc32(char32_t *pc, const char *s, size_t n, mbstate_t *ps);
size_t c32rtomb(char *s, char32_t c, mbstate_t *ps);
#endif
