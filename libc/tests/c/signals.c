/* Signals (S11): the subset the C library delivers. The same program is built and run on the host (as `signals_host`, below) and its
 * output must equal what the OS prints: the host is the oracle for the semantics, the OS for delivery between processes.
 * Signals are delivered where a program waits or calls the library (not asynchronously into a running loop). */
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static volatile sig_atomic_t got;
static volatile sig_atomic_t order[8];
static volatile int norder;

static void on_sig(int s) {
    got = s;
    if (norder < 8) order[norder++] = s;
}
static void on_term_exit7(int s) {
    (void)s;
    _exit(7);
}

static int self_tests(void) {
    struct sigaction sa, old;
    sigset_t set, pend;

    /* signal() and raise(): the handler runs, and the previous disposition is returned */
    void (*prev)(int) = signal(SIGUSR1, on_sig);
    printf("signal returned SIG_DFL: %d\n", prev == SIG_DFL);
    raise(SIGUSR1);
    printf("raise ran the handler: %d\n", got == SIGUSR1);

    /* sigaction: the old action comes back; SIG_IGN drops the signal */
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_IGN;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR2, &sa, &old);
    printf("old SIGUSR2 action was default: %d\n", old.sa_handler == SIG_DFL);
    got = 0;
    raise(SIGUSR2);
    printf("an ignored signal does nothing: %d\n", got == 0);
    sigaction(SIGUSR2, NULL, &old);
    printf("sigaction reads the action: %d\n", old.sa_handler == SIG_IGN);

    /* a blocked signal stays pending until it is unblocked, then runs once even if raised twice */
    sa.sa_handler = on_sig;
    sigaction(SIGUSR2, &sa, NULL);
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigaddset(&set, SIGUSR2);
    got = 0;
    norder = 0;
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR1);
    raise(SIGUSR1);
    raise(SIGUSR2);
    printf("blocked: handler has not run: %d\n", got == 0);
    sigpending(&pend);
    printf("pending: USR1 %d USR2 %d ALRM %d\n", sigismember(&pend, SIGUSR1), sigismember(&pend, SIGUSR2),
           sigismember(&pend, SIGALRM));
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    printf("unblocked: ran %d handlers, one for each signal: %d\n", norder,
           (order[0] == SIGUSR1 && order[1] == SIGUSR2) || (order[0] == SIGUSR2 && order[1] == SIGUSR1));
    sigpending(&pend);
    printf("nothing pending now: %d\n", !sigismember(&pend, SIGUSR1) && !sigismember(&pend, SIGUSR2));

    /* sigprocmask returns the previous mask */
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_SETMASK, &set, NULL);
    sigset_t was;
    sigprocmask(SIG_SETMASK, NULL, &was);
    printf("mask set and read: %d %d\n", sigismember(&was, SIGUSR1), sigismember(&was, SIGUSR2));
    sigemptyset(&set);
    sigprocmask(SIG_SETMASK, &set, NULL);

    /* alarm interrupts sleep: the handler runs and sleep reports the time left */
    signal(SIGALRM, on_sig);
    got = 0;
    alarm(1);
    unsigned left = sleep(10);
    printf("alarm interrupted sleep: handler %d, time left %s\n", got == SIGALRM, left >= 7 && left <= 9 ? "about 9" : "wrong");
    printf("alarm(0) with nothing set returns %u\n", alarm(0));

    /* pause() returns -1 with EINTR once a handler has run */
    got = 0;
    alarm(1);
    int r = pause();
    printf("pause: %d errno EINTR %d handler %d\n", r, errno == EINTR, got == SIGALRM);

    /* poll() is interrupted too */
    got = 0;
    alarm(1);
    r = poll(NULL, 0, 5000);
    printf("poll: %d errno EINTR %d handler %d\n", r, errno == EINTR, got == SIGALRM);

    /* sigsuspend: swap the mask, wait, restore */
    sigemptyset(&set);
    sigaddset(&set, SIGALRM);
    sigprocmask(SIG_BLOCK, &set, NULL);
    sigemptyset(&pend);
    got = 0;
    alarm(1);
    r = sigsuspend(&pend);
    printf("sigsuspend: %d errno EINTR %d handler %d\n", r, errno == EINTR, got == SIGALRM);
    sigprocmask(SIG_SETMASK, &pend, NULL);

    /* a vfork child's changes to dispositions and the mask are its own */
    signal(SIGUSR1, on_sig);
    sigemptyset(&set);
    sigprocmask(SIG_SETMASK, &set, NULL);
    pid_t vp = vfork();
    if (vp == 0) {
        signal(SIGUSR1, SIG_IGN);
        sigemptyset(&pend);
        sigaddset(&pend, SIGUSR2);
        sigprocmask(SIG_BLOCK, &pend, NULL);
        _exit(0);
    }
    waitpid(vp, NULL, 0);
    sigaction(SIGUSR1, NULL, &old);
    sigprocmask(SIG_SETMASK, NULL, &was);
    printf("vfork child's signal changes stay in the child: %d\n", old.sa_handler == on_sig && !sigismember(&was, SIGUSR2));

    /* alarm returns what was left of the previous one */
    alarm(5);
    unsigned prev_left = alarm(0);
    printf("alarm returns the time left: %s\n", prev_left >= 4 && prev_left <= 5 ? "about 5" : "wrong");
    return 0;
}

static int child_tests(const char *self) {
    int status;
    pid_t pid;
    /* a child that handles SIGTERM by exiting 7 */
    char *argv1[] = {(char *)self, "signals", "child-handled", NULL};
    if (posix_spawn(&pid, self, NULL, NULL, argv1, environ) != 0) { printf("spawn failed\n"); return 1; }
    sleep(1);
    printf("kill 0 finds the child: %d\n", kill(pid, 0));
    kill(pid, SIGTERM);
    waitpid(pid, &status, 0);
    printf("handled SIGTERM: exited %d status %d\n", WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    /* a child with the default action dies of it */
    char *argv2[] = {(char *)self, "signals", "child-default", NULL};
    if (posix_spawn(&pid, self, NULL, NULL, argv2, environ) != 0) { printf("spawn failed\n"); return 1; }
    sleep(1);
    kill(pid, SIGTERM);
    waitpid(pid, &status, 0);
    printf("default SIGTERM: signaled %d signal %d\n", WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : -1);
    /* an ignored signal does not end it; SIGKILL cannot be ignored */
    char *argv3[] = {(char *)self, "signals", "child-ignores", NULL};
    if (posix_spawn(&pid, self, NULL, NULL, argv3, environ) != 0) { printf("spawn failed\n"); return 1; }
    sleep(1);
    kill(pid, SIGTERM);
    sleep(1);
    printf("ignored SIGTERM: still there: %d\n", kill(pid, 0));
    kill(pid, SIGKILL);
    waitpid(pid, &status, 0);
    printf("SIGKILL: signaled %d signal %d\n", WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : -1);
    /* a signal to nobody */
    printf("kill of a pid that is not ours: %d errno ESRCH %d\n", kill(99999, SIGTERM), errno == ESRCH);
    return 0;
}

int signals_test(int argc, char **argv) {
    /* a program kills the program that started it (SIGKILL goes through the shell: the parent is not the caller's child) */
    if (argc > 2 && strcmp(argv[2], "killparent") == 0) {
        char *a[] = {argv[0], "signals", "killparent-child", NULL};
        pid_t pid;
        if (posix_spawn(&pid, argv[0], NULL, NULL, a, environ) != 0) return 1;
        int status;
        waitpid(pid, &status, 0);
        sleep(30);
        printf("the parent survived\n");
        return 0;
    }
    if (argc > 2 && strcmp(argv[2], "killparent-child") == 0) {
        kill(getppid(), SIGKILL);
        return 0;
    }
    if (argc > 2 && strcmp(argv[2], "child-handled") == 0) {
        signal(SIGTERM, on_term_exit7);
        for (;;) sleep(10);
    }
    if (argc > 2 && strcmp(argv[2], "child-default") == 0) {
        for (;;) sleep(10);
    }
    if (argc > 2 && strcmp(argv[2], "child-ignores") == 0) {
        signal(SIGTERM, SIG_IGN);
        for (;;) sleep(10);
    }
    int rc = self_tests();
    if (rc == 0) rc = child_tests(argv[0]);
    printf("signals: done\n");
    return rc;
}

#ifdef STANDALONE
int main(int argc, char **argv) {
    return signals_test(argc, argv);
}
#endif
