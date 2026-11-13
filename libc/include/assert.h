#ifndef _OS_ASSERT_H
#define _OS_ASSERT_H
void __assert_fail(const char *expr, const char *file, int line) __attribute__((noreturn));
#ifdef NDEBUG
#define assert(e) ((void)0)
#else
#define assert(e) ((e) ? (void)0 : __assert_fail(#e, __FILE__, __LINE__))
#endif
#endif
