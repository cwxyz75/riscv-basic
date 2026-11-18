/* What gzip 1.13's configure would find on this system (written by hand). */
#ifndef GZIP_CONFIG_H
#define GZIP_CONFIG_H
#define PACKAGE "gzip"
#define PACKAGE_NAME "gzip"
#define PACKAGE_VERSION "1.13"
#define PACKAGE_STRING "gzip 1.13"
#define PACKAGE_BUGREPORT "bug-gzip@gnu.org"
#define HAVE_CHOWN 1
#define HAVE_FCHOWN 1
#define HAVE_FCHMOD 1
#define HAVE_OPENAT 0
#define HAVE_UNLINKAT 0
#define HAVE_WORKING_O_NOFOLLOW 0
#define HAVE_DIRENT_H 1
#define HAVE_UNISTD_H 1
#define HAVE_MEMORY_H 1
#define HAVE_STRING_H 1
#define HAVE_STDLIB_H 1
#define SIZEOF_OFF_T 8
#define _GL_ATTRIBUTE_PURE __attribute__((__pure__))
#define _GL_ATTRIBUTE_CONST __attribute__((__const__))
#define _GL_ATTRIBUTE_NORETURN __attribute__((__noreturn__))
#define _GL_ATTRIBUTE_FALLTHROUGH __attribute__((__fallthrough__))
#define _Noreturn __attribute__((__noreturn__))
#include <stdbool.h>
#include <stddef.h>
int fdatasync(int);
#define O_SEARCH 0
#define O_BINARY 0
#define TIMESPEC_RESOLUTION 1000000000
#define S_IRWXUGO 0777
#include <unistd.h>
#endif
