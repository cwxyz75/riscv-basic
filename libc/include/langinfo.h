#ifndef _OS_LANGINFO_H
#define _OS_LANGINFO_H
/* the C locale, whose one encoding is UTF-8; the numbers are glibc's */
#define CODESET 14
#define RADIXCHAR 0x10000
#define THOUSEP 0x10001
#define YESEXPR 0x50000
#define NOEXPR 0x50001
char *nl_langinfo(int item);
#endif
