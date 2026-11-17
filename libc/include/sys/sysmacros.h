#ifndef _OS_SYS_SYSMACROS_H
#define _OS_SYS_SYSMACROS_H
#define major(d) ((unsigned)(((d) >> 8) & 0xfff))
#define minor(d) ((unsigned)(((d) & 0xff) | (((d) >> 12) & 0xfff00)))
#define makedev(ma, mi) ((((unsigned long long)(ma) & 0xfff) << 8) | ((unsigned long long)(mi) & 0xff) | (((unsigned long long)(mi) & 0xfff00) << 12))
#endif
