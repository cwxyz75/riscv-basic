#ifndef _OS_GETOPT_H
#define _OS_GETOPT_H
extern char *optarg;
extern int optind, opterr, optopt;
struct option {
    const char *name;
    int has_arg;
    int *flag;
    int val;
};
#define no_argument 0
#define required_argument 1
#define optional_argument 2
int getopt(int argc, char *const argv[], const char *optstring);
/* GNU `getopt_long`: options may follow operands (the vector is permuted) unless the option string starts with `+` or
 * POSIXLY_CORRECT is set; `optind = 0` starts again. */
int getopt_long(int argc, char *const argv[], const char *shortopts, const struct option *longopts, int *longindex);
#endif
