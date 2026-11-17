#ifndef _OS_GRP_H
#define _OS_GRP_H
#include <sys/types.h>
struct group { char *gr_name; char *gr_passwd; gid_t gr_gid; char **gr_mem; };
struct group *getgrgid(gid_t gid);
struct group *getgrnam(const char *name);
void endgrent(void);
int getgrouplist(const char *user, gid_t group, gid_t *groups, int *ngroups);
int initgroups(const char *user, gid_t group);
#endif
