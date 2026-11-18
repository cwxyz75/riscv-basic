#ifndef _OS_WCHAR_H
#define _OS_WCHAR_H
#include <stddef.h>
#include <stdio.h>
/* Wide characters for the one encoding the system has, UTF-8; `wchar_t` is a 32-bit `int` holding a code point. */
typedef unsigned int wint_t;
typedef struct { unsigned char __b[4]; unsigned char __n; unsigned char __pad[3]; } mbstate_t;
#define WEOF 0xffffffffu
size_t mbrtowc(wchar_t *pwc, const char *s, size_t n, mbstate_t *ps);
size_t wcrtomb(char *s, wchar_t wc, mbstate_t *ps);
int mbsinit(const mbstate_t *ps);
size_t mbrlen(const char *s, size_t n, mbstate_t *ps);
wint_t btowc(int c);
int wctob(wint_t c);
size_t mbstowcs(wchar_t *dst, const char *src, size_t n);
size_t wcstombs(char *dst, const wchar_t *src, size_t n);
size_t wcslen(const wchar_t *s);
wchar_t *wcscpy(wchar_t *d, const wchar_t *s);
wchar_t *wcsncpy(wchar_t *d, const wchar_t *s, size_t n);
int wcscmp(const wchar_t *a, const wchar_t *b);
int wcsncmp(const wchar_t *a, const wchar_t *b, size_t n);
wchar_t *wcschr(const wchar_t *s, wchar_t c);
wchar_t *wcsrchr(const wchar_t *s, wchar_t c);
wchar_t *wcscat(wchar_t *d, const wchar_t *s);
wchar_t *wmemcpy(wchar_t *d, const wchar_t *s, size_t n);
wchar_t *wmemmove(wchar_t *d, const wchar_t *s, size_t n);
wchar_t *wmemset(wchar_t *d, wchar_t c, size_t n);
int wmemcmp(const wchar_t *a, const wchar_t *b, size_t n);
wchar_t *wmemchr(const wchar_t *s, wchar_t c, size_t n);
int wcwidth(wchar_t wc);
int wcswidth(const wchar_t *s, size_t n);
#endif
