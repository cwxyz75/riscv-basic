#ifndef _OS_FNMATCH_H
#define _OS_FNMATCH_H
#define FNM_NOMATCH 1
/* the Linux (glibc) values, like the rest of the headers; Apple's and the BSDs' swap the first two */
#define FNM_PATHNAME 0x01
#define FNM_NOESCAPE 0x02
#define FNM_PERIOD 0x04
#define FNM_CASEFOLD 0x10
int fnmatch(const char *pattern, const char *string, int flags);
#endif
