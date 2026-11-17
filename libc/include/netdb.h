#ifndef _OS_NETDB_H
#define _OS_NETDB_H
#include <sys/socket.h>
struct addrinfo {
    int ai_flags;
    int ai_family;
    int ai_socktype;
    int ai_protocol;
    socklen_t ai_addrlen;
    struct sockaddr *ai_addr;
    char *ai_canonname;
    struct addrinfo *ai_next;
};
#define AI_PASSIVE 1
#define AI_NUMERICHOST 4
#define EAI_BADFLAGS (-1)
#define EAI_NONAME (-2)
#define EAI_AGAIN (-3)
#define EAI_FAIL (-4)
#define EAI_FAMILY (-6)
#define EAI_SOCKTYPE (-7)
#define EAI_SERVICE (-8)
#define EAI_MEMORY (-10)
#define EAI_SYSTEM (-11)
/* IPv4 only, numeric services only. `RESOLVER=ADDR:PORT` in the environment replaces the configured DNS server. */
int getaddrinfo(const char *node, const char *service, const struct addrinfo *hints, struct addrinfo **res);
void freeaddrinfo(struct addrinfo *ai);
const char *gai_strerror(int code);
struct hostent { char *h_name; char **h_aliases; int h_addrtype; int h_length; char **h_addr_list; };
#define h_addr h_addr_list[0]
extern int h_errno;
#define HOST_NOT_FOUND 1
#define TRY_AGAIN 2
#define NO_RECOVERY 3
#define NO_DATA 4
struct servent { char *s_name; char **s_aliases; int s_port; char *s_proto; };
struct hostent *gethostbyname(const char *name);
const char *hstrerror(int err);
struct servent *getservbyname(const char *name, const char *proto);
#define AI_CANONNAME 2
#define NI_NUMERICHOST 1
#define NI_NUMERICSERV 2
#define NI_NOFQDN 4
#define NI_NAMEREQD 8
#define NI_DGRAM 16
int getnameinfo(const struct sockaddr *sa, socklen_t salen, char *host, socklen_t hostlen, char *serv, socklen_t servlen, int flags);
#endif
