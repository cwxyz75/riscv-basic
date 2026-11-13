/* The libc transcript (S7c-2): one C program, built twice - with the host's compiler and libc, and
 * with our headers and library on the OS - whose output must be identical. Everything printed is
 * defined by the C standard (no pointers, no rand() values, no strerror text, no last-digit libm). */
#include <assert.h>
#include <ctype.h>
#include <errno.h>
#include <math.h>
#include <setjmp.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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
    printf("done\n");
    return 0;
}
