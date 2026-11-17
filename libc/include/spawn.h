#ifndef _OS_SPAWN_H
#define _OS_SPAWN_H
#include <sys/types.h>
/* posix_spawn (UV8, OD118): the program starts through the shell that started this one (a spawn grant). File actions
   support dup2 and close of descriptors 0-2 only. */
typedef struct { void *__actions; } posix_spawn_file_actions_t;
typedef struct { int __unused; } posix_spawnattr_t;
int posix_spawn_file_actions_init(posix_spawn_file_actions_t *fa);
int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *fa);
int posix_spawn_file_actions_adddup2(posix_spawn_file_actions_t *fa, int fd, int newfd);
int posix_spawn_file_actions_addclose(posix_spawn_file_actions_t *fa, int fd);
int posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *fa, const posix_spawnattr_t *attr,
                char *const argv[], char *const envp[]);
int posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *fa, const posix_spawnattr_t *attr,
                 char *const argv[], char *const envp[]);
#endif
