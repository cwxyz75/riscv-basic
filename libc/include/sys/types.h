#ifndef _OS_SYS_TYPES_H
#define _OS_SYS_TYPES_H
#include <stddef.h>
#include <stdint.h>
typedef long ssize_t;
typedef long off_t;
typedef long long off64_t;
typedef int pid_t;
typedef unsigned int uid_t;
typedef unsigned int gid_t;
typedef unsigned int mode_t;
typedef unsigned long long dev_t;
typedef unsigned long long ino_t;
typedef unsigned int nlink_t;
typedef long long blkcnt_t;
typedef long blksize_t;
typedef unsigned int useconds_t;
typedef long suseconds_t;
typedef int id_t;
typedef int key_t;
typedef unsigned char u_char;
typedef unsigned short u_short;
typedef unsigned int u_int;
typedef unsigned long u_long;
typedef unsigned long fsblkcnt_t;
typedef unsigned long fsfilcnt_t;
#endif
