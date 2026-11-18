/* GNU-style getopt_long for the ported programs (original code; the C library's `getopt` is POSIX and does not permute).
 * Options may follow operands: the words of an option (it and its separate argument) are moved in front of the operands
 * skipped so far, so that on return `optind` is the first operand. A leading '+' in the option string, or POSIXLY_CORRECT
 * in the environment, stops at the first operand. `optind = 0` starts afresh. A leading ':' makes a missing argument
 * return ':' and silences the messages. `--` ends the options. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "getopt.h"

static int in_cluster; /* offset into a cluster of short options, 0 between words */
static int cl_word;    /* the word of that cluster */
static int cl_base;    /* where the operands skipped before it begin */

static int is_option_word(const char *w) { return w[0] == '-' && w[1]; }

/* argv[base..opt) are operands and argv[opt..end) the words of one option: put the option first */
static void move_before(char **argv, int base, int opt, int end) {
    for (int i = opt; i < end; i++) {
        char *t = argv[i];
        for (int j = i; j > base + (i - opt); j--) argv[j] = argv[j - 1];
        argv[base + (i - opt)] = t;
    }
}

static int bad(const char *prog, const char *fmt, const char *a, int quiet) {
    if (opterr && !quiet) fprintf(stderr, fmt, prog, a);
    return '?';
}

int getopt_long(int argc, char *const argvc[], const char *shortopts, const struct option *longopts, int *longind) {
    char **argv = (char **)argvc;
    const char *prog = argv[0] ? argv[0] : "?";
    int permute = !(shortopts[0] == '+' || getenv("POSIXLY_CORRECT"));
    if (shortopts[0] == '+' || shortopts[0] == '-') shortopts++;
    int quiet = shortopts[0] == ':';
    optarg = 0;
    if (optind == 0) {
        optind = 1;
        in_cluster = 0;
    }
    if (!in_cluster) {
        int base = optind, j = optind;
        if (permute) {
            while (j < argc && !is_option_word(argv[j])) j++;
        } else if (j < argc && !is_option_word(argv[j])) {
            return -1;
        }
        if (j >= argc) return -1;
        if (!strcmp(argv[j], "--")) {
            move_before(argv, base, j, j + 1);
            optind = base + 1;
            return -1;
        }
        if (argv[j][1] == '-') {
            char *name = argv[j] + 2, *eq = strchr(name, '=');
            size_t len = eq ? (size_t)(eq - name) : strlen(name);
            const struct option *found = 0;
            int idx = -1, ambiguous = 0;
            for (int i = 0; longopts && longopts[i].name; i++) {
                if (strncmp(longopts[i].name, name, len)) continue;
                if (strlen(longopts[i].name) == len) { found = &longopts[i]; idx = i; ambiguous = 0; break; }
                if (!found) { found = &longopts[i]; idx = i; }
                else if (found->has_arg != longopts[i].has_arg || found->flag != longopts[i].flag || found->val != longopts[i].val) ambiguous = 1;
            }
            int end = j + 1;
            int ret;
            char tmp[256];
            snprintf(tmp, sizeof tmp, "%.*s", (int)len, name);
            if (ambiguous) { optopt = 0; ret = bad(prog, "%s: option '--%s' is ambiguous\n", tmp, quiet); }
            else if (!found) { optopt = 0; ret = bad(prog, "%s: unrecognized option '--%s'\n", tmp, quiet); }
            else if (eq && !found->has_arg) { optopt = found->val; ret = bad(prog, "%s: option '--%s' doesn't allow an argument\n", found->name, quiet); }
            else {
                ret = found->val;
                if (eq) optarg = eq + 1;
                else if (found->has_arg == required_argument) {
                    if (end < argc) optarg = argv[end++];
                    else { optopt = found->val; bad(prog, "%s: option '--%s' requires an argument\n", found->name, quiet); ret = quiet ? ':' : '?'; found = 0; }
                }
                if (found) {
                    if (longind) *longind = idx;
                    if (found->flag) { *found->flag = found->val; ret = 0; }
                }
            }
            move_before(argv, base, j, end);
            optind = base + (end - j);
            return ret;
        }
        in_cluster = 1;
        cl_word = j;
        cl_base = base;
    }
    char *arg = argv[cl_word];
    char c = arg[in_cluster++];
    const char *spec = c == ':' ? 0 : strchr(shortopts, c);
    int end = cl_word + 1, ret = (unsigned char)c;
    int more = arg[in_cluster] != 0;
    if (!spec) {
        optopt = (unsigned char)c;
        if (opterr && !quiet) fprintf(stderr, "%s: invalid option -- '%c'\n", prog, c);
        ret = '?';
    } else if (spec[1] == ':') {
        if (spec[2] == ':') { /* optional: only when attached */
            if (more) { optarg = arg + in_cluster; more = 0; }
        } else if (more) {
            optarg = arg + in_cluster;
            more = 0;
        } else if (end < argc) {
            optarg = argv[end++];
        } else {
            optopt = (unsigned char)c;
            if (opterr && !quiet) fprintf(stderr, "%s: option requires an argument -- '%c'\n", prog, c);
            ret = quiet ? ':' : '?';
        }
    }
    if (more) return ret; /* the rest of the cluster is for the next call */
    in_cluster = 0;
    move_before(argv, cl_base, cl_word, end);
    optind = cl_base + (end - cl_word);
    return ret;
}
