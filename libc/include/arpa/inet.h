#ifndef _OS_ARPA_INET_H
#define _OS_ARPA_INET_H
#include <netinet/in.h>
int inet_pton(int af, const char *src, void *dst);
const char *inet_ntop(int af, const void *src, char *dst, socklen_t size);
#define INET_ADDRSTRLEN 16
#endif
