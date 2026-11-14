#ifndef _OS_SETJMP_H
#define _OS_SETJMP_H
/* ra, sp, s0-s11 */
typedef unsigned long jmp_buf[26]; /* ra, sp, s0-s11, fs0-fs11 */
int setjmp(jmp_buf env) __attribute__((returns_twice));
int _setjmp(jmp_buf env) __attribute__((returns_twice));
void longjmp(jmp_buf env, int val) __attribute__((noreturn));
#endif
