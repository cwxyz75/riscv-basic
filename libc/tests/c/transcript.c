/* The libc transcript (S7c-2): one C program, built twice - with the host's compiler and libc, and
 * with our headers and library on the OS - whose output must be identical. Everything printed is
 * defined by the C standard (no pointers, no rand() values, no strerror text, no last-digit libm). */
#include <assert.h>
#include <ctype.h>
#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <setjmp.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <locale.h>
#include <signal.h>
#include <time.h>

static void section(const char *name) { printf("== %s\n", name); }

static void formats(void) {
    section("printf");
    printf("%d %i %u %x %X %o %c %s %%\n", -42, 42, 4000000000u, 255, 255, 8, 'z', "text");
    printf("[%5d][%-5d][%05d][%+d][% d][%.3d][%5.3d]\n", 42, 42, 42, 42, 42, 7, 7);
    printf("[%#x][%#o][%#X][%x]\n", 255, 8, 255, 0);
    printf("[%10s][%-10s][%.3s][%10.3s]\n", "abcdef", "abcdef", "abcdef", "abcdef");
    printf("%ld %lld %hd %hhd %zu %jd %td\n", -1234567890123L, -1234567890123456789LL, (short)-5, (signed char)-5, (size_t)99, (long)7, (long)8);
    printf("%lu %llu %lx\n", 18446744073709551615UL, 18446744073709551615ULL, 0xdeadbeefUL);
    printf("%*d|%-*d|%.*f\n", 6, 42, 6, 42, 2, 3.14159);
    printf("%f %e %g %E %G\n", 3.14159, 31415.9, 0.0001234, 1e-10, 123456789.0);
    printf("%.0f %.1f %.10f %8.3f|%-8.2f|%+.2f\n", 2.5, 0.05, 1.0 / 3, 3.14159, 2.71828, 1.0);
    printf("%g %g %g %g %g\n", 100000.0, 1000000.0, 1e-5, 0.5, 123456.789);
    printf("%.3g %.10g %#g %g\n", 1234.5678, 1234.5678, 1.5, 0.0);
    printf("%a %a %a %A\n", 1.0, 0.5, 255.5, 3.0);
    printf("%f %f %e\n", 1e15, 123456789012345678.0, 0.0);
    printf("%d%%|%c%c|\n", 50, 'o', 'k');
    char buf[64];
    int n = snprintf(buf, sizeof buf, "%s-%d", "ab", 12);
    printf("snprintf: %d [%s]\n", n, buf);
    n = snprintf(buf, 5, "%s", "truncated");
    printf("snprintf trunc: %d [%s]\n", n, buf);
    n = snprintf(NULL, 0, "%05.1f", 3.14159);
    printf("snprintf size: %d\n", n);
    n = sprintf(buf, "%x", 48879);
    printf("sprintf: %d [%s]\n", n, buf);
}

static void strings(void) {
    section("string");
    char a[32], b[32];
    strcpy(a, "hello");
    strcat(a, ", world");
    printf("%s %zu\n", a, strlen(a));
    strncpy(b, "abc", 8);
    printf("%zu %d %d\n", strlen(b), b[3], b[7]);
    printf("%d %d %d\n", strcmp("a", "b") < 0, strcmp("b", "a") > 0, strcmp("same", "same"));
    printf("%d %d\n", strncmp("abcd", "abcz", 3), strncmp("abcd", "abcz", 4) < 0);
    const char *s = "the quick brown fox";
    printf("%d %d %d\n", (int)(strchr(s, 'q') - s), (int)(strrchr(s, 'o') - s), strchr(s, '!') == NULL);
    /* the terminating NUL is part of the string for strchr/strrchr */
    printf("%d %d\n", (int)(strchr(s, 0) - s) == (int)strlen(s), (int)(strrchr(s, 0) - s) == (int)strlen(s));
    printf("%d %d\n", (int)(strstr(s, "brown") - s), strstr(s, "slow") == NULL);
    printf("%zu %zu %d\n", strspn("aabbcc", "ab"), strcspn("hello world", " "), (int)(strpbrk("find the x", "xyz") - "find the x"));
    char *d = strdup("duplicate");
    printf("%s %zu\n", d, strlen(d));
    free(d);
    strcpy(a, "abc");
    strncat(a, "defgh", 2);
    printf("%s\n", a);
    char m[10];
    memset(m, 'x', 9);
    m[9] = 0;
    memcpy(m + 2, "AB", 2);
    memmove(m + 1, m, 4);
    printf("%s %d\n", m, memcmp("abc", "abd", 3) < 0);
    printf("%d\n", (int)((char *)memchr("hello", 'l', 5) - "hello"));
}

static void ctype_table(void) {
    section("ctype");
    for (int c = 0; c < 128; c += 1) {
        int bits = (isalpha(c) != 0) | (isdigit(c) != 0) << 1 | (isspace(c) != 0) << 2 | (isupper(c) != 0) << 3 |
                   (islower(c) != 0) << 4 | (isprint(c) != 0) << 5 | (ispunct(c) != 0) << 6 | (iscntrl(c) != 0) << 7 |
                   (isxdigit(c) != 0) << 8 | (isalnum(c) != 0) << 9 | (isgraph(c) != 0) << 10;
        printf("%x ", bits);
        if (c % 16 == 15) printf("\n");
    }
    printf("%c%c %c%c\n", toupper('a'), toupper('Z'), tolower('A'), tolower('z'));
}

static int cmp_int(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}
static int cmp_str(const void *a, const void *b) { return strcmp(*(const char *const *)a, *(const char *const *)b); }

static void stdlib_things(void) {
    section("stdlib");
    char *end;
    printf("%ld %ld %ld %ld\n", strtol("  123abc", &end, 10), strtol("-0x1f", NULL, 0), strtol("0777", NULL, 0), strtol("zz", NULL, 36));
    strtol("123abc", &end, 10);
    printf("%d\n", (int)(end - "123abc"));
    printf("%lu %llu\n", strtoul("4294967295", NULL, 10), strtoull("18446744073709551615", NULL, 10));
    errno = 0;
    long big = strtol("99999999999999999999", NULL, 10);
    printf("%ld %d\n", big, errno == ERANGE);
    errno = 0;
    strtol("12", NULL, 10);
    printf("%d\n", errno);
    printf("%g %g %g %g\n", strtod("3.5e2xyz", &end), strtod("0x1.8p1", NULL), strtod("-inf", NULL), strtod(".5", NULL));
    printf("%d %d %g\n", atoi("  -77x"), (int)atol("12345"), atof("2.25"));
    printf("%d %ld %lld\n", abs(-7), labs(-8L), llabs(-9LL));
    int v[] = {5, 3, 9, 1, 7, 2, 8, 6, 4, 0};
    qsort(v, 10, sizeof v[0], cmp_int);
    for (int i = 0; i < 10; i++) printf("%d ", v[i]);
    int key = 7;
    int *hit = bsearch(&key, v, 10, sizeof v[0], cmp_int);
    key = 11;
    printf("| %d %d\n", (int)(hit - v), bsearch(&key, v, 10, sizeof v[0], cmp_int) == NULL);
    const char *names[] = {"pear", "apple", "fig", "banana", "cherry"};
    qsort(names, 5, sizeof names[0], cmp_str);
    for (int i = 0; i < 5; i++) printf("%s ", names[i]);
    printf("\n");
    int r = rand();
    srand(7);
    printf("%d\n", r >= 0 && rand() >= 0);
    printf("%d\n", getenv("NO_SUCH_VARIABLE_HERE") == NULL);
}

static void memory(void) {
    section("malloc");
    unsigned char *p[64];
    for (int i = 0; i < 64; i++) {
        p[i] = malloc((size_t)(i * 37 % 500 + 1));
        assert(p[i] != NULL);
        memset(p[i], i, (size_t)(i * 37 % 500 + 1));
    }
    int ok = 1;
    for (int i = 0; i < 64; i++)
        for (int j = 0; j < i * 37 % 500 + 1; j++)
            if (p[i][j] != i) ok = 0;
    printf("filled %d\n", ok);
    for (int i = 0; i < 64; i += 2) free(p[i]);
    unsigned char *q = calloc(100, 3);
    int zero = 1;
    for (int j = 0; j < 300; j++) if (q[j]) zero = 0;
    printf("calloc zero %d\n", zero);
    unsigned char *g = malloc(10);
    memcpy(g, "0123456789", 10);
    g = realloc(g, 5000);
    printf("grown %d %d\n", memcmp(g, "0123456789", 10), (int)(((unsigned long)g) % 16));
    g = realloc(g, 4);
    printf("shrunk %d\n", memcmp(g, "0123", 4));
    free(g);
    free(q);
    /* size classes: blocks are reused, a grown or shrunk block keeps its contents across every class
     * boundary (16-byte classes up to 512, then the heap), and nothing overlaps */
    {
        unsigned char *live[40];
        int sizes[40], intact = 1, aligned = 1;
        for (int round = 0; round < 6; round++) {
            for (int i = 0; i < 40; i++) {
                if (round == 0) live[i] = NULL, sizes[i] = 0;
                int want = (i * 53 + round * 211) % 900 + 1;
                live[i] = realloc(live[i], (size_t)want);
                assert(live[i] != NULL);
                if ((unsigned long)live[i] % 16) aligned = 0;
                int keep = sizes[i] < want ? sizes[i] : want;
                for (int j = 0; j < keep; j++) if (live[i][j] != (unsigned char)(i + j)) intact = 0;
                for (int j = 0; j < want; j++) live[i][j] = (unsigned char)(i + j);
                sizes[i] = want;
            }
            for (int i = 0; i < 40; i += 3) { free(live[i]); live[i] = NULL; sizes[i] = 0; }
            for (int i = 0; i < 40; i++)
                for (int j = 0; j < sizes[i]; j++) if (live[i][j] != (unsigned char)(i + j)) intact = 0;
        }
        for (int i = 0; i < 40; i++) free(live[i]);
        /* malloc(0) blocks sit next to small blocks; freeing and reusing them must not touch a
         * neighbour (a free list link written past a zero-size block would) */
        unsigned char *nb[16];
        for (int i = 0; i < 16; i++) { nb[i] = malloc(16); memset(nb[i], 0x5a + i, 16); }
        for (int round = 0; round < 4; round++) {
            void *z[16];
            for (int i = 0; i < 16; i++) z[i] = malloc(0);
            for (int i = 0; i < 16; i++) free(z[i]);
        }
        for (int i = 0; i < 16; i++) {
            for (int j = 0; j < 16; j++) if (nb[i][j] != (unsigned char)(0x5a + i)) intact = 0;
            free(nb[i]);
        }
        printf("classes %d %d\n", intact, aligned);
    }
    for (int i = 1; i < 64; i += 2) free(p[i]);
    void *fresh = realloc(NULL, 8); /* realloc(NULL, n) is malloc(n) */
    printf("%d\n", fresh != NULL);
    free(fresh);
}

static void math_things(void) {
    section("math");
    printf("%g %g %g %g\n", sqrt(2.0), floor(-2.5), ceil(-2.5), fabs(-3.25));
    printf("%g %g %g %g\n", fmod(10.5, 3), ldexp(1.5, 4), trunc(-2.7), round(2.5));
    int e;
    double m = frexp(48.0, &e);
    double ip;
    double fp = modf(-3.75, &ip);
    printf("%g %d %g %g\n", m, e, fp, ip);
    printf("%.6g %.6g %.6g %.6g\n", sin(1.0), cos(1.0), tan(0.5), atan2(1.0, 2.0));
    printf("%.6g %.6g %.6g %.6g\n", exp(1.0), log(10.0), pow(2.0, 0.5), log10(1000.0));
    printf("%.6g %.6g %.6g\n", sinh(1.0), cosh(1.0), tanh(0.5));
    printf("%d %d\n", isnan(NAN) != 0, isinf(-INFINITY) != 0); /* C says "nonzero": glibc gives -1 */
    printf("%g %g\n", pow(2, 10), hypot(3, 4));
}

static jmp_buf env;
static int depth_reached;
static void deep(int n) {
    depth_reached = n;
    if (n == 0) longjmp(env, 42);
    deep(n - 1);
}

static void jumps(void) {
    section("setjmp");
    volatile int counter = 0;
    int r = setjmp(env);
    printf("setjmp returned %d, counter %d\n", r, counter);
    if (r == 0) {
        counter = 1;
        deep(50);
    }
    printf("deep reached %d\n", depth_reached);
    r = setjmp(env);
    if (r == 0) longjmp(env, 0); /* a zero value becomes 1 */
    printf("longjmp(0) gave %d\n", r);
    volatile int loops = 0;
    for (int i = 0; i < 3; i++) {
        if (setjmp(env) == 0) {
            loops++;
            longjmp(env, 7);
        }
    }
    printf("loops %d\n", loops);
}


static void files(void) {
    section("files");
    const char *path = "s7c-transcript.txt"; /* relative: the working directory is the scratch place */
    FILE *f = fopen(path, "w");
    assert(f != NULL);
    fprintf(f, "line one\nline %d\n", 2);
    fputs("third line\n", f);
    fwrite("binary\0data", 1, 11, f);
    fputc('!', f);
    printf("close %d\n", fclose(f));
    f = fopen(path, "r");
    assert(f != NULL);
    char line[40];
    while (fgets(line, sizeof line, f)) {
        size_t n = strlen(line);
        printf("got [%.*s] %zu\n", (int)(n > 0 && line[n - 1] == '\n' ? n - 1 : n), line, n);
    }
    printf("eof %d err %d tell %ld\n", feof(f) != 0, ferror(f) != 0, ftell(f));
    rewind(f);
    printf("after rewind: tell %ld eof %d\n", ftell(f), feof(f) != 0);
    char buf[16];
    fseek(f, 5, SEEK_SET);
    size_t got = fread(buf, 1, 6, f);
    printf("fread %zu [%.6s] tell %ld\n", got, buf, ftell(f));
    fseek(f, -4, SEEK_END);
    got = fread(buf, 1, 16, f);
    printf("tail %zu [%.3s] eof %d\n", got, buf, feof(f) != 0);
    fseek(f, 0, SEEK_SET);
    int c = getc(f);
    ungetc('X', f); /* only one push-back is guaranteed */
    int a = getc(f), b = getc(f), d = getc(f);
    printf("unget %c %c %c %c\n", c, a, b, d);
    fseek(f, 2, SEEK_CUR);
    c = getc(f);
    long where = ftell(f);
    printf("seek cur %d tell %ld\n", c, where);
    fseek(f, 0, SEEK_END);
    printf("size %ld\n", ftell(f));
    fclose(f);
    f = fopen(path, "a");
    fputs("appended\n", f);
    fclose(f);
    f = fopen(path, "r+");
    fputc('L', f);
    fseek(f, 0, SEEK_SET);
    fgets(line, sizeof line, f);
    printf("r+ [%s]", line);
    fseek(f, 0, SEEK_END);
    printf("size %ld\n", ftell(f));
    fclose(f);
    errno = 0;
    f = fopen("s7c-no-such-file", "r");
    printf("missing %d %d\n", f == NULL, errno == ENOENT);
    FILE *t = tmpfile();
    assert(t != NULL);
    fprintf(t, "scratch %d", 99);
    rewind(t);
    char sb[32] = {0};
    fread(sb, 1, 31, t);
    printf("tmpfile [%s]\n", sb);
    fclose(t);
    f = fopen(path, "w");
    setvbuf(f, NULL, _IONBF, 0);
    fputs("unbuffered", f);
    fclose(f);
    f = fopen(path, "r");
    char ub[16] = {0};
    fread(ub, 1, 15, f);
    fclose(f);
    printf("unbuf [%s]\n", ub);
    /* a failed seek changes nothing; freopen reuses the stream; paths with ./ and empty paths */
    f = fopen("./s7c-transcript.txt", "w");
    fputs("abcdefghij", f);
    fclose(f);
    f = fopen(path, "r");
    char three[4] = {0};
    fread(three, 1, 3, f);
    int rc = fseek(f, -1000, SEEK_CUR);
    long at = ftell(f); /* (the arguments of one call are evaluated in no particular order) */
    int next = getc(f);
    printf("bad seek %d tell %ld next %c\n", rc, at, next);
    FILE *same = freopen("s7c-second.txt", "w", f);
    printf("freopen same stream %d\n", same == f);
    fputs("second file", f);
    fclose(f);
    f = fopen("s7c-second.txt", "r");
    char two[16] = {0};
    fread(two, 1, 15, f);
    fclose(f);
    printf("second [%s]\n", two);
    printf("empty path %d\n", fopen("", "r") == NULL);
    /* rename: the data moves with the name, an existing file is replaced, a missing source is ENOENT */
    printf("rename %d\n", rename("s7c-second.txt", "s7c-moved.txt"));
    printf("old name gone %d\n", fopen("s7c-second.txt", "r") == NULL);
    f = fopen("s7c-moved.txt", "r");
    char moved[16] = {0};
    fread(moved, 1, 15, f);
    fclose(f);
    printf("moved [%s]\n", moved);
    f = fopen("s7c-victim.txt", "w");
    fputs("victim", f);
    fclose(f);
    printf("replace %d\n", rename("s7c-moved.txt", "s7c-victim.txt"));
    f = fopen("s7c-victim.txt", "r");
    char repl[16] = {0};
    fread(repl, 1, 15, f);
    fclose(f);
    printf("replaced [%s]\n", repl);
    errno = 0;
    int miss = rename("s7c-nothing.txt", "s7c-x.txt"); /* (separate statements: argument order is unspecified) */
    printf("missing %d %d\n", miss, errno == ENOENT);
    remove("s7c-victim.txt");
    printf("remove %d\n", remove(path));
    printf("gone %d\n", fopen(path, "r") == NULL);
}

static void times(void) {
    section("time");
    time_t t = 1700000000;
    struct tm *g = gmtime(&t);
    printf("%d-%02d-%02d %02d:%02d:%02d wday %d yday %d\n", g->tm_year + 1900, g->tm_mon + 1, g->tm_mday, g->tm_hour, g->tm_min, g->tm_sec, g->tm_wday, g->tm_yday);
    char b[128];
    size_t n = strftime(b, sizeof b, "%Y-%m-%d %H:%M:%S|%a %A|%b %B|%j|%U %W|%p %I|%y %C|%e|%D|%F|%T|%R", g);
    printf("%zu [%s]\n", n, b);
    n = strftime(b, 8, "%Y-%m-%d", g);
    printf("too small: %zu\n", n);
    struct tm m;
    memset(&m, 0, sizeof m);
    m.tm_year = 100;
    m.tm_mday = 1;
    m.tm_hour = 12;
    time_t u = timegm(&m);
    printf("timegm %lld wday %d\n", (long long)u, m.tm_wday);
    m.tm_mday = 32;
    m.tm_mon = 12;
    u = timegm(&m);
    printf("normalised %lld %d-%02d-%02d\n", (long long)u, m.tm_year + 1900, m.tm_mon + 1, m.tm_mday);
    printf("difftime %g\n", difftime(t, t - 86400));
    printf("%d %d\n", time(NULL) >= 0, clock() >= 0);
}

static volatile int got_signal;
static void on_signal(int sig) { got_signal = sig; }

static void misc(void) {
    section("signal and locale");
    printf("%s [%s]\n", setlocale(LC_ALL, "C"), localeconv()->decimal_point);
    signal(SIGINT, on_signal);
    raise(SIGINT);
    printf("handler saw %d\n", got_signal);
    signal(SIGTERM, SIG_IGN);
    printf("ignored raise %d\n", raise(SIGTERM));
}

static pthread_mutex_t counter_lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;
static int queue[64], q_head, q_tail, q_closed;
static long consumed_sum;
static int consumed_count;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static int once_runs;
static pthread_key_t key;
static volatile int detached_ran;

static void *count_up(void *arg) {
    for (int i = 0; i < 1000; i++) {
        pthread_mutex_lock(&counter_lock);
        counter++;
        pthread_mutex_unlock(&counter_lock);
    }
    return (void *)((long)arg * (long)arg);
}

static void *errno_thread(void *arg) {
    long id = (long)arg;
    errno = 100 + (int)id;
    for (int i = 0; i < 50; i++) sched_yield();
    return (void *)(long)(errno == 100 + (int)id);
}

static void *consumer(void *arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&counter_lock);
        while (q_head == q_tail && !q_closed) pthread_cond_wait(&not_empty, &counter_lock);
        if (q_head == q_tail) {
            pthread_mutex_unlock(&counter_lock);
            return NULL;
        }
        consumed_sum += queue[q_head++ % 64];
        consumed_count++;
        pthread_mutex_unlock(&counter_lock);
    }
}

static void init_once(void) { once_runs++; }

static void *once_and_key(void *arg) {
    pthread_once(&once, init_once);
    pthread_setspecific(key, arg);
    for (int i = 0; i < 20; i++) sched_yield();
    return (void *)(long)(pthread_getspecific(key) == arg);
}

static void *set_flag(void *arg) {
    (void)arg;
    detached_ran = 1;
    return NULL;
}

static void *try_lock_thread(void *arg) {
    return (void *)(long)pthread_mutex_trylock((pthread_mutex_t *)arg);
}

static int (*volatile deep_fn)(int);
static int deep_sum(int n) {
    char pad[1024];
    memset(pad, n & 0x7f, sizeof pad);
    __asm__ volatile("" : : "r"(pad) : "memory"); /* the buffer really lives on the stack */
    if (n == 0) return pad[5];
    return deep_fn(n - 1) + pad[n % 1024] % 3; /* not a tail call: the stack really grows */
}

static void threads(void) {
    section("pthreads");
    pthread_t t[4];
    void *ret;
    for (long i = 0; i < 4; i++) assert(pthread_create(&t[i], NULL, count_up, (void *)(i + 1)) == 0);
    long sum = 0;
    for (int i = 0; i < 4; i++) {
        pthread_join(t[i], &ret);
        sum += (long)ret;
    }
    printf("counter %ld returns %ld\n", counter, sum);

    errno = 7;
    for (long i = 0; i < 3; i++) assert(pthread_create(&t[i], NULL, errno_thread, (void *)i) == 0);
    long ok = 0;
    for (int i = 0; i < 3; i++) {
        pthread_join(t[i], &ret);
        ok += (long)ret;
    }
    printf("errno per thread %ld, main's errno %d\n", ok, errno);

    pthread_t c1, c2;
    pthread_create(&c1, NULL, consumer, NULL);
    pthread_create(&c2, NULL, consumer, NULL);
    for (int i = 1; i <= 300; i++) {
        pthread_mutex_lock(&counter_lock);
        while (q_tail - q_head >= 64) {
            pthread_mutex_unlock(&counter_lock);
            sched_yield();
            pthread_mutex_lock(&counter_lock);
        }
        queue[q_tail++ % 64] = i;
        pthread_cond_signal(&not_empty);
        pthread_mutex_unlock(&counter_lock);
    }
    pthread_mutex_lock(&counter_lock);
    q_closed = 1;
    pthread_cond_broadcast(&not_empty);
    pthread_mutex_unlock(&counter_lock);
    pthread_join(c1, NULL);
    pthread_join(c2, NULL);
    printf("consumed %d items, sum %ld\n", consumed_count, consumed_sum);

    pthread_key_create(&key, NULL);
    pthread_setspecific(key, (void *)99);
    long agree = 0;
    for (long i = 0; i < 4; i++) assert(pthread_create(&t[i], NULL, once_and_key, (void *)(i + 1)) == 0);
    for (int i = 0; i < 4; i++) {
        pthread_join(t[i], &ret);
        agree += (long)ret;
    }
    printf("once ran %d, keys agree %ld, main's key %ld\n", once_runs, agree, (long)pthread_getspecific(key));

    pthread_mutexattr_t attr;
    pthread_mutexattr_init(&attr);
    pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_RECURSIVE);
    pthread_mutex_t rec;
    pthread_mutex_init(&rec, &attr);
    int a = pthread_mutex_lock(&rec), b = pthread_mutex_lock(&rec);
    pthread_t tt;
    pthread_create(&tt, NULL, try_lock_thread, &rec);
    pthread_join(tt, &ret);
    printf("recursive %d %d, other thread's trylock was EBUSY %d\n", a, b, (long)ret == EBUSY);
    pthread_mutex_unlock(&rec);
    pthread_mutex_unlock(&rec);
    pthread_create(&tt, NULL, try_lock_thread, &rec);
    pthread_join(tt, &ret);
    printf("after unlock: trylock %ld\n", (long)ret);
    pthread_mutex_unlock(&rec);

    pthread_attr_t da;
    pthread_attr_init(&da);
    pthread_attr_setdetachstate(&da, PTHREAD_CREATE_DETACHED);
    pthread_create(&tt, &da, set_flag, NULL);
    for (int i = 0; i < 200 && !detached_ran; i++) {
        struct timespec ts = {0, 5000000};
        nanosleep(&ts, NULL);
    }
    /* a timed wait that nobody signals ends with ETIMEDOUT, after the deadline */
    pthread_mutex_t tm = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t tc = PTHREAD_COND_INITIALIZER;
    struct timespec before, deadline, after;
    clock_gettime(CLOCK_REALTIME, &before);
    deadline = before;
    deadline.tv_nsec += 30 * 1000000;
    if (deadline.tv_nsec >= 1000000000) {
        deadline.tv_sec++;
        deadline.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&tm);
    int trc = pthread_cond_timedwait(&tc, &tm, &deadline);
    pthread_mutex_unlock(&tm);
    clock_gettime(CLOCK_REALTIME, &after);
    long waited_ms = (after.tv_sec - before.tv_sec) * 1000 + (after.tv_nsec - before.tv_nsec) / 1000000;
    printf("timedwait timed out %d, waited at least 30ms: %d\n", trc == ETIMEDOUT, waited_ms >= 29);
    printf("detached ran %d, self equal %d\n", detached_ran, pthread_equal(pthread_self(), pthread_self()));

    /* a recursion that needs more than the loader's 256 KiB stack */
    deep_fn = deep_sum;
    printf("deep %d\n", deep_sum(700)); /* about 700 KiB of stack */
}

static void tail(void) { printf("atexit handler\n"); }

static int sum(int count, ...) {
    va_list ap;
    va_start(ap, count);
    int s = 0;
    for (int i = 0; i < count; i++) s += va_arg(ap, int);
    va_end(ap);
    return s;
}

static void streams(void) {
    section("stdio");
    puts("puts line");
    putchar('c');
    putchar('\n');
    fputs("fputs text\n", stdout);
    fputc('f', stdout);
    fputc('\n', stdout);
    fwrite("fwrite\n", 1, 7, stdout);
    fprintf(stdout, "fprintf %d\n", 5);
    fflush(stdout);
    printf("va_arg %d\n", sum(4, 10, 20, 30, 40));
}

int main(int argc, char **argv) {
    (void)argv;
    atexit(tail);
    printf("argc %d\n", argc);
    formats();
    strings();
    ctype_table();
    stdlib_things();
    memory();
    math_things();
    jumps();
    streams();
    files();
    times();
    misc();
    threads();
    printf("done\n");
    return 0;
}
