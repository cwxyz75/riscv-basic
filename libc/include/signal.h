#ifndef _OS_SIGNAL_H
#define _OS_SIGNAL_H
#include <sys/types.h>
/* Signal numbers follow Linux (riscv64) so ported software finds what it expects. Signals are delivered where a program waits
 * or calls the library, not asynchronously into a loop (S11, OD119, sig.rs): a blocking call a handler interrupted returns EINTR. */
typedef int sig_atomic_t;
typedef void (*sighandler_t)(int);
typedef unsigned long sigset_t;
#define SIG_DFL ((sighandler_t)0)
#define SIG_IGN ((sighandler_t)1)
#define SIG_ERR ((sighandler_t)-1)
#define SIGHUP 1
#define SIGINT 2
#define SIGQUIT 3
#define SIGILL 4
#define SIGTRAP 5
#define SIGABRT 6
#define SIGBUS 7
#define SIGFPE 8
#define SIGKILL 9
#define SIGUSR1 10
#define SIGSEGV 11
#define SIGUSR2 12
#define SIGPIPE 13
#define SIGALRM 14
#define SIGTERM 15
#define SIGCHLD 17
#define SIGCONT 18
#define SIGSTOP 19
#define SIGTSTP 20
#define SIGTTIN 21
#define SIGTTOU 22
#define SIGURG 23
#define SIGXCPU 24
#define SIGXFSZ 25
#define SIGPROF 27
#define SIGVTALRM 26
#define SIGWINCH 28
#define SIGPOLL 29
#define SIGIO SIGPOLL
#define SIGSYS 31
#define SIGRTMIN 34
#define SIGRTMAX 64
#define NSIG 65
#define SA_NOCLDSTOP 1
#define SA_SIGINFO 4
#define SA_RESTART 0x10000000
#define SA_NODEFER 0x40000000
#define SA_RESETHAND 0x80000000
#define SIG_BLOCK 0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2
#define SI_USER 0
typedef struct {
int si_signo;
int si_errno;
int si_code;
int _pad;
int si_pid;
unsigned si_uid;
void *si_value;
char _rest[96];
} siginfo_t;
struct sigaction {
    union {
    sighandler_t sa_handler;
    void (*sa_sigaction)(int, siginfo_t *, void *);
    };
    sigset_t sa_mask;
    int sa_flags;
};
sighandler_t signal(int sig, sighandler_t h);
int raise(int sig);
int kill(pid_t pid, int sig);
int sigaction(int sig, const struct sigaction *act, struct sigaction *old);
int sigprocmask(int how, const sigset_t *set, sigset_t *old);
int sigwait(const sigset_t *set, int *sig);
struct timespec;
int sigwaitinfo(const sigset_t *set, siginfo_t *info);
int sigtimedwait(const sigset_t *set, siginfo_t *info, const struct timespec *timeout);
int pthread_sigmask(int how, const sigset_t *set, sigset_t *old);
int sigsuspend(const sigset_t *mask);
int sigemptyset(sigset_t *set);
int sigfillset(sigset_t *set);
int sigaddset(sigset_t *set, int sig);
int sigdelset(sigset_t *set, int sig);
int sigismember(const sigset_t *set, int sig);
unsigned alarm(unsigned seconds);
int sigisemptyset(const sigset_t *set);
int sigpending(sigset_t *set);
int pause(void);
#endif
