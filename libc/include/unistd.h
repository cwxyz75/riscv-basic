#ifndef _OS_UNISTD_H
#define _OS_UNISTD_H
#include <stddef.h>
#include <sys/types.h>
ssize_t read(int fd, void *buf, size_t n);
ssize_t write(int fd, const void *buf, size_t n);
int close(int fd);
off_t lseek(int fd, off_t off, int whence);
int isatty(int fd);
unsigned sleep(unsigned seconds);
int usleep(unsigned usec);
int unlink(const char *path);
char *getcwd(char *buf, size_t size);
int chdir(const char *path);
int access(const char *path, int mode);
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4
long readlink(const char *path, char *buf, size_t size);
int symlink(const char *target, const char *linkpath);
int link(const char *oldpath, const char *newpath);
int chown(const char *path, int uid, int gid);
int fchown(int fd, int uid, int gid);
int rmdir(const char *path);
#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2
extern char **environ;
extern char *optarg;
extern int optind, opterr, optopt;
int getopt(int argc, char *const argv[], const char *optstring);
void _exit(int code) __attribute__((noreturn));
pid_t getpid(void);
pid_t getppid(void);
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int setuid(uid_t uid);
int setgid(gid_t gid);
int seteuid(uid_t uid);
int setegid(gid_t gid);
int getgroups(int size, gid_t list[]);
pid_t setsid(void);
int dup(int fd);
int dup2(int from, int to);
int pipe(int fds[2]);
int fsync(int fd);
int ftruncate(int fd, off_t len);
int fchdir(int fd);
int chroot(const char *path);
int lchown(const char *path, uid_t uid, gid_t gid);
pid_t fork(void);
pid_t vfork(void) __attribute__((returns_twice));
int execv(const char *path, char *const argv[]);
int execve(const char *path, char *const argv[], char *const envp[]);
int execvp(const char *file, char *const argv[]);
int execvpe(const char *file, char *const argv[], char *const envp[]);
int getlogin_r(char *buf, size_t n);
int ttyname_r(int fd, char *buf, size_t n);
int getpagesize(void);
long gethostid(void);
long sysconf(int name);
#define _SC_ARG_MAX 0
#define _SC_CLK_TCK 2
#define _SC_NGROUPS_MAX 3
#define _SC_OPEN_MAX 4
#define _SC_PAGESIZE 30
#define _SC_NPROCESSORS_ONLN 84
int tcsetpgrp(int fd, pid_t pgrp);
pid_t tcgetpgrp(int fd);
int setpgid(pid_t pid, pid_t pgid);
pid_t getpgrp(void);
#endif
