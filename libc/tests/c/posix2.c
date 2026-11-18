/* The POSIX additions of the gap survey (LIBC-GAPS.md): the `*at` family, `error`, `exec*` with a list, `strings.h`, `creat`,
 * `ftello`, `mkostemp`. Run on the OS only: `ctest posix2`. Every check prints `ok: <what>` or `FAIL: <what>`; the return value
 * is the number of failures. */
#include <errno.h>
#include <error.h>
#include <fcntl.h>
#include <getopt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <signal.h>
#include <unistd.h>
#include <wchar.h>
#include <wctype.h>
#include <spawn.h>
#include <semaphore.h>
#include <threads.h>
extern char **environ;
#include <locale.h>
#include <fnmatch.h>
#include <termios.h>

static int failures;

static mtx_t t_mtx;
static cnd_t t_cnd;
static int t_ready, t_once_count, t_sum;
static once_flag t_once = ONCE_FLAG_INIT;
static tss_t t_key;
static sem_t t_sem;
static void t_once_fn(void) { t_once_count++; }
static int t_worker(void *arg) {
    call_once(&t_once, t_once_fn);
    tss_set(t_key, arg);
    mtx_lock(&t_mtx);
    t_sum += (int)(long)tss_get(t_key);
    t_ready++;
    cnd_signal(&t_cnd);
    mtx_unlock(&t_mtx);
    sem_post(&t_sem);
    return (int)(long)arg + 100;
}

#define CHECK(cond, what) do { if (cond) printf("ok: %s\n", what); else { printf("FAIL: %s (errno %d)\n", what, errno); failures++; } fflush(stdout); } while (0)

int posix2_test(void) {
    struct stat st;
    /* the *at calls, against a directory descriptor and AT_FDCWD */
    mkdir("/tmp/at", 0755);
    int d = open("/tmp/at", O_RDONLY | O_DIRECTORY);
    CHECK(d >= 0, "open a directory descriptor");
    int f = openat(d, "a.txt", O_WRONLY | O_CREAT, 0644);
    CHECK(f >= 0 && write(f, "hello", 5) == 5 && close(f) == 0, "openat makes a file below the descriptor");
    CHECK(fstatat(d, "a.txt", &st, 0) == 0 && st.st_size == 5 && S_ISREG(st.st_mode), "fstatat sees it");
    CHECK(fstatat(AT_FDCWD, "/tmp/at/a.txt", &st, 0) == 0 && st.st_size == 5, "fstatat with AT_FDCWD and an absolute path");
    CHECK(fstatat(d, "missing", &st, 0) != 0 && errno == ENOENT, "fstatat of a missing name is ENOENT");
    CHECK(faccessat(d, "a.txt", R_OK, 0) == 0 && faccessat(d, "missing", F_OK, 0) != 0, "faccessat");
    CHECK(fchmodat(d, "a.txt", 0600, 0) == 0 && fstatat(d, "a.txt", &st, 0) == 0 && (st.st_mode & 0777) == 0600, "fchmodat");
    CHECK(mkdirat(d, "sub", 0755) == 0 && fstatat(d, "sub", &st, 0) == 0 && S_ISDIR(st.st_mode), "mkdirat");
    /* (symlinks live on the volume, not in /tmp: `symlinkat` and `readlinkat` are checked in the mount test) */
    CHECK(renameat(d, "a.txt", d, "b.txt") == 0 && fstatat(d, "a.txt", &st, 0) != 0 && fstatat(d, "b.txt", &st, 0) == 0, "renameat");
    CHECK(linkat(d, "b.txt", d, "hard", 0) != 0 && errno == EPERM, "linkat is refused");
    CHECK(mkfifoat(d, "fifo", 0644) != 0 && errno == ENOSYS, "mkfifoat is not supported");
    CHECK(unlinkat(d, "b.txt", 0) == 0, "unlinkat removes a file");
    CHECK(unlinkat(d, "sub", AT_REMOVEDIR) == 0, "unlinkat with AT_REMOVEDIR removes a directory");
    CHECK(unlinkat(d, "sub", 0) != 0, "and a missing name fails");
    CHECK(faccessat(99, "x", F_OK, 0) != 0 && errno == EBADF, "a bad descriptor is EBADF");
    close(d);
    rmdir("/tmp/at");
    /* strings.h, creat, ftello, mkostemp */
    CHECK(bcmp("abc", "abd", 3) != 0 && bcmp("abc", "abc", 3) == 0, "bcmp");
    char b[8] = "abcdef";
    bcopy(b, b + 2, 4);
    CHECK(memcmp(b, "ababcd", 6) == 0, "bcopy overlaps like memmove");
    CHECK(index("a/b/c", '/') != NULL && *(index("a/b/c", '/') + 1) == 'b' && *(rindex("a/b/c", '/') + 1) == 'c', "index and rindex");
    int c = creat("/tmp/creat.txt", 0644);
    CHECK(c >= 0 && write(c, "xy", 2) == 2 && close(c) == 0 && stat("/tmp/creat.txt", &st) == 0 && st.st_size == 2, "creat");
    c = creat("/tmp/creat.txt", 0644);
    CHECK(c >= 0 && close(c) == 0 && stat("/tmp/creat.txt", &st) == 0 && st.st_size == 0, "creat truncates");
    FILE *fp = fopen("/tmp/creat.txt", "w");
    fputs("12345", fp);
    CHECK(ftello(fp) == 5, "ftello");
    fclose(fp);
    unlink("/tmp/creat.txt");
    char tmpl[] = "/tmp/xXXXXXX";
    int t = mkostemp(tmpl, 0);
    CHECK(t >= 0 && close(t) == 0 && unlink(tmpl) == 0, "mkostemp");
    char x[8];
    CHECK(strxfrm(x, "abc", sizeof x) == 3 && strcmp(x, "abc") == 0, "strxfrm in the C locale is a copy");
    /* getopt_long: options after operands are found, and the vector is permuted so that the operands come last */
    {
        char *av[] = {"prog", "x", "-a", "--beta=1", "y", "--gamma", "-c", "z", "--", "-q", NULL};
        static struct option lo[] = {{"alpha", no_argument, 0, 'A'}, {"beta", required_argument, 0, 'B'}, {"gamma", optional_argument, 0, 'G'}, {0, 0, 0, 0}};
        int ac = 9, r, got[8], n = 0, li = -1;
        char *bval = NULL;
        optind = 0;
        while ((r = getopt_long(ac, av, "ac:", lo, &li)) != -1 && n < 8) {
            got[n++] = r;
            if (r == 'B') bval = optarg;
        }
        CHECK(n == 4 && got[0] == 'a' && got[1] == 'B' && got[2] == 'G' && got[3] == 'c' && bval && strcmp(bval, "1") == 0, "getopt_long returns short and long options in order");
        CHECK(optind == 7 && strcmp(av[7], "x") == 0 && strcmp(av[8], "y") == 0, "and the operands are moved behind them (`--` ends the options)");
        char *av2[] = {"prog", "--al", "q", NULL};
        optind = 0;
        r = getopt_long(3, av2, "a", lo, &li);
        CHECK(r == 'A' && li == 0 && getopt_long(3, av2, "a", lo, &li) == -1 && optind == 2, "an abbreviated long option and its index");
        optind = 0;
        opterr = 0;
        char *av3[] = {"prog", "--nope", NULL};
        CHECK(getopt_long(2, av3, "a", lo, &li) == '?', "an unknown long option is '?'");
    }
    /* wide characters: UTF-8 is the one encoding */
    {
        wchar_t w = 0;
        mbstate_t ps;
        memset(&ps, 0, sizeof ps);
        CHECK(mbrtowc(&w, "\xc3\xa9", 2, &ps) == 2 && w == 0xe9 && mbsinit(&ps), "mbrtowc decodes a two-byte character");
        errno = 0;
        CHECK(mbrtowc(&w, "\xff", 1, &ps) == (size_t)-1 && errno == EILSEQ, "mbrtowc refuses bytes that are not UTF-8 with EILSEQ");
        memset(&ps, 0, sizeof ps);
        size_t r1 = mbrtowc(&w, "\xe6\x97", 2, &ps);
        CHECK(r1 == (size_t)-2 && !mbsinit(&ps) && mbrtowc(&w, "\xa5", 1, &ps) == 1 && w == 0x65e5, "a character split across two calls (-2, then the rest)");
        char out[4];
        CHECK(wcrtomb(out, 0x65e5, NULL) == 3 && (unsigned char)out[0] == 0xe6 && (unsigned char)out[2] == 0xa5, "wcrtomb encodes three bytes");
        CHECK(wcrtomb(out, 0xd800, NULL) == (size_t)-1 && errno == EILSEQ, "and refuses a surrogate");
        wchar_t ws[8];
        CHECK(mbstowcs(ws, "a\xc3\xb1" "b", 8) == 3 && ws[1] == 0xf1 && ws[3] == 0 && wcslen(ws) == 3, "mbstowcs and wcslen");
        CHECK(wcwidth(0x65e5) == 2 && wcwidth('a') == 1 && wcwidth(0x301) == 0 && wcswidth(ws, 3) == 3, "wcwidth: wide, narrow, combining");
        CHECK(iswalpha(0xe9) && !iswalpha('1') && iswspace(0x3000) && iswupper(0xc9) && towupper(0xe9) == 0xc9 && towlower(0xc9) == 0xe9, "the wide classes and case");
        CHECK(iswctype('x', wctype("alpha")) && !iswctype('x', wctype("digit")) && wctype("nope") == 0, "wctype and iswctype");
    }
    /* fscanf reads fields in turn and leaves the rest of the stream */
    {
        FILE *sf = fopen("/tmp/scan.txt", "w");
        fputs("12 abc 3.5\nline two 7\n", sf);
        fclose(sf);
        sf = fopen("/tmp/scan.txt", "r");
        int a = 0, b = 0;
        char w[16];
        double dv = 0;
        int n1 = fscanf(sf, "%d %15s %lf", &a, w, &dv);
        CHECK(n1 == 3 && a == 12 && strcmp(w, "abc") == 0 && dv == 3.5, "fscanf reads three fields");
        char l2[32];
        CHECK(fgets(l2, sizeof l2, sf) != NULL && strcmp(l2, "\n") == 0 && fgets(l2, sizeof l2, sf) != NULL && strcmp(l2, "line two 7\n") == 0, "the rest of the stream is where the format stopped");
        rewind(sf);
        n1 = fscanf(sf, "%d", &a);
        int n2 = fscanf(sf, "%s", w);
        int n3 = fscanf(sf, "%lf", &dv);
        CHECK(n1 == 1 && n2 == 1 && n3 == 1 && a == 12 && strcmp(w, "abc") == 0 && dv == 3.5, "repeated fscanf calls continue where the last one ended");
        CHECK(fscanf(sf, "%d", &b) == 0, "a field that is not a number fails with no conversions");
        fclose(sf);
        unlink("/tmp/scan.txt");
    }
    /* sigwait takes a blocked, pending signal */
    {
        sigset_t ss;
        sigemptyset(&ss);
        sigaddset(&ss, SIGUSR1);
        sigprocmask(SIG_BLOCK, &ss, NULL);
        kill(getpid(), SIGUSR1);
        int got = 0;
        CHECK(sigwait(&ss, &got) == 0 && got == SIGUSR1, "sigwait returns a blocked pending signal");
        sigprocmask(SIG_UNBLOCK, &ss, NULL);
    }
    /* error() */
    unsigned before = error_message_count;
    error(0, ENOENT, "cannot open %s", "file");
    error_at_line(0, 0, "f.c", 7, "bad %d", 3);
    CHECK(error_message_count == before + 2, "error counts its messages (the text is on standard error)");
    /* exec with a list of arguments, in a vfork child */
    pid_t pid = vfork();
    if (pid == 0) {
        execlp("echo", "echo", "from execlp", (char *)0);
        _exit(127);
    }
    int status = 0;
    CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0, "execlp runs a program found by name");
    /* a script runs through its `#!` line, and /bin/sh is the compat shell */
    {
        FILE *f = fopen("/tmp/script.sh", "w");
        if (f) { fputs("#!/bin/sh\necho from-script $1\n", f); fclose(f); }
        fflush(stdout);
        int st = system("/tmp/script.sh arg");
        CHECK(st == 0, "a script with a #! line runs");
        pid_t sp;
        char *sh_argv[] = {"sh", "-c", "echo spawned-sh; exit 3", NULL};
        fflush(stdout);
        int rc = posix_spawn(&sp, "/bin/sh", NULL, NULL, sh_argv, environ);
        int wst = 0;
        if (rc == 0) waitpid(sp, &wst, 0);
        CHECK(rc == 0 && WIFEXITED(wst) && WEXITSTATUS(wst) == 3, "posix_spawn of /bin/sh runs the compat shell");
        char *bad_argv[] = {"nothere", NULL};
        CHECK(posix_spawn(&sp, "/tmp/nothere.sh", NULL, NULL, bad_argv, environ) == ENOENT, "a missing script is ENOENT");
        FILE *g = fopen("/tmp/plain.txt", "w");
        if (g) { fputs("not a script\n", g); fclose(g); }
        CHECK(posix_spawn(&sp, "/tmp/plain.txt", NULL, NULL, bad_argv, environ) == ENOEXEC, "a file with no #! line is ENOEXEC");
    }
    /* the rest of POSIX's error list, in Linux numbers, each with its own message (the library publishes Linux's numbering) */
    {
        struct { const char *name; int v, want; } e[] = {
            {"EBADMSG", EBADMSG, 74}, {"ECANCELED", ECANCELED, 125}, {"ECONNABORTED", ECONNABORTED, 103}, {"EDQUOT", EDQUOT, 122},
            {"EFBIG", EFBIG, 27}, {"EIDRM", EIDRM, 43}, {"EMLINK", EMLINK, 31}, {"EMULTIHOP", EMULTIHOP, 72}, {"ENETRESET", ENETRESET, 102},
            {"ENODATA", ENODATA, 61}, {"ENOLINK", ENOLINK, 67}, {"ENOMSG", ENOMSG, 42}, {"ENOSR", ENOSR, 63}, {"ENOSTR", ENOSTR, 60},
            {"ENOTRECOVERABLE", ENOTRECOVERABLE, 131}, {"EOWNERDEAD", EOWNERDEAD, 130}, {"EPROTO", EPROTO, 71}, {"EPROTOTYPE", EPROTOTYPE, 91},
            {"EROFS", EROFS, 30}, {"ESTALE", ESTALE, 116}, {"ETIME", ETIME, 62}, {"ETXTBSY", ETXTBSY, 26}, {"ENOTBLK", ENOTBLK, 15},
        };
        int bad = 0;
        for (unsigned i = 0; i < sizeof e / sizeof e[0]; i++) {
            if (e[i].v != e[i].want) { printf("FAIL: %s is %d, not %d\n", e[i].name, e[i].v, e[i].want); bad++; }
            if (strcmp(strerror(e[i].want), "Unknown error") == 0) { printf("FAIL: %s has no message\n", e[i].name); bad++; }
        }
        CHECK(bad == 0, "the rest of POSIX's errno names have Linux's numbers and messages");
        CHECK(strcmp(strerror(EROFS), "Read-only file system") == 0 && strcmp(strerror(EDQUOT), "Disk quota exceeded") == 0, "strerror texts");
    }
    /* constants gnulib and busybox ask for */
    CHECK(MB_CUR_MAX == 4 && LC_MESSAGES == 5 && AT_EACCESS == 0x200 && FNM_FILE_NAME == FNM_PATHNAME && FNM_LEADING_DIR == 8, "MB_CUR_MAX, LC_MESSAGES, AT_EACCESS, FNM_FILE_NAME and FNM_LEADING_DIR");
    CHECK(VEOL == 11 && VSTART == 8 && VSTOP == 9 && VSUSP == 10 && CS5 == 0 && CS6 == 0020 && CS7 == 0040 && FLUSHO == 0010000, "the remaining termios names");
    /* C11 threads and POSIX semaphores, over the pthread layer */
    {
        mtx_init(&t_mtx, mtx_plain);
        cnd_init(&t_cnd);
        sem_init(&t_sem, 0, 0);
        tss_create(&t_key, NULL);
        thrd_t a, b;
        CHECK(thrd_create(&a, t_worker, (void *)1) == thrd_success && thrd_create(&b, t_worker, (void *)2) == thrd_success, "thrd_create");
        mtx_lock(&t_mtx);
        while (t_ready < 2) cnd_wait(&t_cnd, &t_mtx);
        mtx_unlock(&t_mtx);
        int ra = 0, rb = 0;
        CHECK(thrd_join(a, &ra) == thrd_success && thrd_join(b, &rb) == thrd_success && ra == 101 && rb == 102, "thrd_join returns the result");
        CHECK(t_sum == 3 && t_once_count == 1, "a mutex, a condition variable, thread-specific data and call_once");
        int v = -1;
        sem_getvalue(&t_sem, &v);
        CHECK(v == 2 && sem_trywait(&t_sem) == 0 && sem_wait(&t_sem) == 0 && sem_trywait(&t_sem) == -1 && errno == EAGAIN, "semaphore counts");
        struct timespec soon;
        clock_gettime(CLOCK_REALTIME, &soon);
        soon.tv_nsec += 20 * 1000 * 1000;
        if (soon.tv_nsec >= 1000000000L) { soon.tv_sec++; soon.tv_nsec -= 1000000000L; }
        CHECK(sem_timedwait(&t_sem, &soon) == -1 && errno == ETIMEDOUT, "sem_timedwait times out");
        CHECK(sem_post(&t_sem) == 0 && sem_timedwait(&t_sem, &soon) == 0, "and succeeds after a post");
        CHECK(sem_open("/x", 0) == SEM_FAILED && errno == ENOSYS, "named semaphores are not supported");
        CHECK(mtx_trylock(&t_mtx) == thrd_success && mtx_trylock(&t_mtx) == thrd_busy && mtx_unlock(&t_mtx) == thrd_success, "mtx_trylock");
    }
    printf("posix2: %d failures\n", failures);
    return failures;
}
