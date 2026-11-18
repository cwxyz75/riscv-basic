#ifndef _OS_SEMAPHORE_H
#define _OS_SEMAPHORE_H
/* Unnamed semaphores over the library's mutex and condition variable (which sit on the kernel's futexes). Named semaphores
 * (`sem_open`) need a place for them to live, which this system has no name for: they fail with ENOSYS. */
#include <errno.h>
#include <pthread.h>
#include <time.h>
typedef struct { pthread_mutex_t m; pthread_cond_t c; unsigned v; } sem_t;
#define SEM_FAILED ((sem_t *)0)
#define SEM_VALUE_MAX 2147483647
static inline int sem_init(sem_t *s, int pshared, unsigned value) {
    if (pshared) { errno = ENOSYS; return -1; } /* (no memory shared between processes) */
    if (value > SEM_VALUE_MAX) { errno = EINVAL; return -1; }
    pthread_mutex_init(&s->m, 0);
    pthread_cond_init(&s->c, 0);
    s->v = value;
    return 0;
}
static inline int sem_destroy(sem_t *s) { pthread_cond_destroy(&s->c); pthread_mutex_destroy(&s->m); return 0; }
static inline int sem_post(sem_t *s) {
    pthread_mutex_lock(&s->m);
    if (s->v == SEM_VALUE_MAX) { pthread_mutex_unlock(&s->m); errno = EOVERFLOW; return -1; }
    s->v++;
    pthread_cond_signal(&s->c);
    pthread_mutex_unlock(&s->m);
    return 0;
}
static inline int sem_wait(sem_t *s) {
    pthread_mutex_lock(&s->m);
    while (s->v == 0) pthread_cond_wait(&s->c, &s->m);
    s->v--;
    pthread_mutex_unlock(&s->m);
    return 0;
}
static inline int sem_trywait(sem_t *s) {
    int r = 0;
    pthread_mutex_lock(&s->m);
    if (s->v == 0) { errno = EAGAIN; r = -1; } else s->v--;
    pthread_mutex_unlock(&s->m);
    return r;
}
static inline int sem_timedwait(sem_t *s, const struct timespec *abs) {
    int r = 0;
    pthread_mutex_lock(&s->m);
    while (s->v == 0) {
        int e = pthread_cond_timedwait(&s->c, &s->m, abs);
        if (e != 0 && s->v == 0) { errno = e; r = -1; break; }
    }
    if (r == 0) s->v--;
    pthread_mutex_unlock(&s->m);
    return r;
}
static inline int sem_getvalue(sem_t *s, int *out) {
    pthread_mutex_lock(&s->m);
    *out = (int)s->v;
    pthread_mutex_unlock(&s->m);
    return 0;
}
static inline sem_t *sem_open(const char *name, int oflag, ...) { (void)name; (void)oflag; errno = ENOSYS; return SEM_FAILED; }
static inline int sem_close(sem_t *s) { (void)s; errno = ENOSYS; return -1; }
static inline int sem_unlink(const char *name) { (void)name; errno = ENOSYS; return -1; }
#endif
