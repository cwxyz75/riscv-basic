#ifndef _OS_STDIO_H
#define _OS_STDIO_H
#include <stddef.h>
#include <stdarg.h>
#include <sys/types.h>
typedef struct FILE FILE;
extern FILE *stdin, *stdout, *stderr;
#define EOF (-1)
#define BUFSIZ 4096
#define FILENAME_MAX 1024
#define FOPEN_MAX 64
#define L_tmpnam 64
#define TMP_MAX 1000000
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define _IOFBF 0
#define _IOLBF 1
#define _IONBF 2
int printf(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
int fprintf(FILE *f, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
int sprintf(char *buf, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
int snprintf(char *buf, size_t n, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
int vprintf(const char *fmt, va_list ap);
int vfprintf(FILE *f, const char *fmt, va_list ap);
int vsprintf(char *buf, const char *fmt, va_list ap);
int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap);
FILE *fopen(const char *path, const char *mode);
FILE *freopen(const char *path, const char *mode, FILE *f);
int fclose(FILE *f);
int fflush(FILE *f);
size_t fread(void *p, size_t size, size_t n, FILE *f);
size_t fwrite(const void *p, size_t size, size_t n, FILE *f);
int fgetc(FILE *f);
int getc(FILE *f);
int getchar(void);
int ungetc(int c, FILE *f);
char *fgets(char *s, int n, FILE *f);
int fputs(const char *s, FILE *f);
int puts(const char *s);
int fputc(int c, FILE *f);
int putc(int c, FILE *f);
int putchar(int c);
int feof(FILE *f);
int ferror(FILE *f);
void clearerr(FILE *f);
int fileno(FILE *f);
int fseek(FILE *f, long off, int whence);
long ftell(FILE *f);
long long ftello(FILE *f);
int renameat(int olddirfd, const char *old, int newdirfd, const char *new_);
void rewind(FILE *f);
int setvbuf(FILE *f, char *buf, int mode, size_t size);
void setbuf(FILE *f, char *buf);
FILE *tmpfile(void);
char *tmpnam(char *buf);
int remove(const char *path);
int rename(const char *from, const char *to);
void perror(const char *msg);
int getc_unlocked(FILE *f);
int getchar_unlocked(void);
int putc_unlocked(int c, FILE *f);
int putchar_unlocked(int c);
int fputs_unlocked(const char *s, FILE *f);
int ferror_unlocked(FILE *f);
int fileno_unlocked(FILE *f);
int sscanf(const char *s, const char *fmt, ...);
int vasprintf(char **out, const char *fmt, va_list ap);
int dprintf(int fd, const char *fmt, ...);
ssize_t getline(char **line, size_t *n, FILE *f);
int fseeko(FILE *f, off_t off, int whence);
FILE *fdopen(int fd, const char *mode);
FILE *popen(const char *cmd, const char *mode);
int pclose(FILE *f);
char *fgets_unlocked(char *s, int n, FILE *f);
int feof_unlocked(FILE *f);
#endif
