#ifndef _OS_LOCALE_H
#define _OS_LOCALE_H
#define LC_ALL 6
#define LC_COLLATE 3
#define LC_CTYPE 0
#define LC_MONETARY 4
#define LC_NUMERIC 1
#define LC_TIME 2
struct lconv { char *decimal_point; char *thousands_sep; char *grouping; };
struct lconv *localeconv(void);
char *setlocale(int category, const char *name);
#endif
