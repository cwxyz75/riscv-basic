#ifndef INTPROPS_H
#define INTPROPS_H
#include <limits.h>
#define TYPE_SIGNED(t) (!((t)0 < (t)-1))
#define TYPE_MAXIMUM(t) ((t)(TYPE_SIGNED(t) ? ~((t)1 << (sizeof(t) * CHAR_BIT - 1)) : (t)-1))
#define TYPE_MINIMUM(t) ((t)(TYPE_SIGNED(t) ? (t)1 << (sizeof(t) * CHAR_BIT - 1) : 0))
#define INT_ADD_WRAPV(a, b, r) __builtin_add_overflow(a, b, r)
#define INT_SUBTRACT_WRAPV(a, b, r) __builtin_sub_overflow(a, b, r)
#define INT_MULTIPLY_WRAPV(a, b, r) __builtin_mul_overflow(a, b, r)
#define INT_ADD_OVERFLOW(a, b) __builtin_add_overflow_p(a, b, (__typeof__((a) + (b)))0)
#define INT_STRLEN_BOUND(t) (sizeof(t) * CHAR_BIT / 3 + 2)
#endif
