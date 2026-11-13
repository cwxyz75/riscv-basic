#ifndef _OS_STDLIB_H
#define _OS_STDLIB_H
#include <stddef.h>
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 2147483647
void *malloc(size_t n);
void *calloc(size_t count, size_t size);
void *realloc(void *p, size_t n);
void free(void *p);
int abs(int x);
long labs(long x);
long long llabs(long long x);
long strtol(const char *s, char **end, int base);
long long strtoll(const char *s, char **end, int base);
unsigned long strtoul(const char *s, char **end, int base);
unsigned long long strtoull(const char *s, char **end, int base);
double strtod(const char *s, char **end);
int atoi(const char *s);
long atol(const char *s);
double atof(const char *s);
void qsort(void *base, size_t n, size_t size, int (*cmp)(const void *, const void *));
void *bsearch(const void *key, const void *base, size_t n, size_t size, int (*cmp)(const void *, const void *));
int rand(void);
void srand(unsigned int seed);
void exit(int code) __attribute__((noreturn));
void _Exit(int code) __attribute__((noreturn));
void abort(void) __attribute__((noreturn));
int atexit(void (*f)(void));
char *getenv(const char *name);
int system(const char *cmd);
#endif
