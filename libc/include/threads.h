#ifndef _OS_THREADS_H
#define _OS_THREADS_H
/* C11 threads over pthread (header only). */
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdlib.h>
#include <time.h>
typedef pthread_t thrd_t;
typedef pthread_mutex_t mtx_t;
typedef pthread_cond_t cnd_t;
typedef pthread_key_t tss_t;
typedef pthread_once_t once_flag;
typedef int (*thrd_start_t)(void *);
typedef void (*tss_dtor_t)(void *);
#define ONCE_FLAG_INIT PTHREAD_ONCE_INIT
#define TSS_DTOR_ITERATIONS 4
#define thread_local _Thread_local
enum { thrd_success = 0, thrd_busy = 1, thrd_error = 2, thrd_nomem = 3, thrd_timedout = 4 };
enum { mtx_plain = 0, mtx_recursive = 1, mtx_timed = 2 };
struct _thrd_arg { thrd_start_t f; void *a; };
static inline void *_thrd_tramp(void *p) {
    struct _thrd_arg x = *(struct _thrd_arg *)p;
    free(p);
    return (void *)(long)x.f(x.a);
}
static inline int thrd_create(thrd_t *t, thrd_start_t f, void *a) {
    struct _thrd_arg *x = malloc(sizeof *x);
    if (!x) return thrd_nomem;
    x->f = f; x->a = a;
    if (pthread_create(t, 0, _thrd_tramp, x) != 0) { free(x); return thrd_error; }
    return thrd_success;
}
static inline int thrd_join(thrd_t t, int *res) {
    void *r;
    if (pthread_join(t, &r) != 0) return thrd_error;
    if (res) *res = (int)(long)r;
    return thrd_success;
}
static inline int thrd_detach(thrd_t t) { return pthread_detach(t) == 0 ? thrd_success : thrd_error; }
static inline thrd_t thrd_current(void) { return pthread_self(); }
static inline int thrd_equal(thrd_t a, thrd_t b) { return pthread_equal(a, b); }
static inline void thrd_exit(int r) { pthread_exit((void *)(long)r); }
static inline void thrd_yield(void) { sched_yield(); }
static inline int thrd_sleep(const struct timespec *d, struct timespec *rem) { return nanosleep(d, rem); }
static inline int mtx_init(mtx_t *m, int type) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_settype(&a, (type & mtx_recursive) ? PTHREAD_MUTEX_RECURSIVE : PTHREAD_MUTEX_NORMAL);
    int r = pthread_mutex_init(m, &a);
    pthread_mutexattr_destroy(&a);
    return r == 0 ? thrd_success : thrd_error;
}
static inline void mtx_destroy(mtx_t *m) { pthread_mutex_destroy(m); }
static inline int mtx_lock(mtx_t *m) { return pthread_mutex_lock(m) == 0 ? thrd_success : thrd_error; }
static inline int mtx_trylock(mtx_t *m) { int e = pthread_mutex_trylock(m); return e == 0 ? thrd_success : (e == EBUSY ? thrd_busy : thrd_error); }
static inline int mtx_unlock(mtx_t *m) { return pthread_mutex_unlock(m) == 0 ? thrd_success : thrd_error; }
static inline int cnd_init(cnd_t *c) { return pthread_cond_init(c, 0) == 0 ? thrd_success : thrd_error; }
static inline void cnd_destroy(cnd_t *c) { pthread_cond_destroy(c); }
static inline int cnd_signal(cnd_t *c) { return pthread_cond_signal(c) == 0 ? thrd_success : thrd_error; }
static inline int cnd_broadcast(cnd_t *c) { return pthread_cond_broadcast(c) == 0 ? thrd_success : thrd_error; }
static inline int cnd_wait(cnd_t *c, mtx_t *m) { return pthread_cond_wait(c, m) == 0 ? thrd_success : thrd_error; }
static inline int cnd_timedwait(cnd_t *c, mtx_t *m, const struct timespec *abs) {
    int e = pthread_cond_timedwait(c, m, abs);
    return e == 0 ? thrd_success : (e == ETIMEDOUT ? thrd_timedout : thrd_error);
}
static inline void call_once(once_flag *f, void (*fn)(void)) { pthread_once(f, fn); }
static inline int tss_create(tss_t *k, tss_dtor_t d) { return pthread_key_create(k, d) == 0 ? thrd_success : thrd_error; }
static inline void tss_delete(tss_t k) { pthread_key_delete(k); }
static inline void *tss_get(tss_t k) { return pthread_getspecific(k); }
static inline int tss_set(tss_t k, void *v) { return pthread_setspecific(k, v) == 0 ? thrd_success : thrd_error; }
#endif
