/* The socket shim's own C test (S9 step 6, Q28): BSD sockets over loopback and the stack, run on the OS only (a host run
 * would test the host). Every check prints `ok: <what>` or `FAIL: <what>`; the return value is the number of failures. */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/random.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <netdb.h>

static int failures;

#define CHECK(cond, what) do { if (cond) printf("ok: %s\n", what); else { printf("FAIL: %s (errno %d)\n", what, errno); failures++; } fflush(stdout); } while (0)

static struct sockaddr_in addr_of(const char *ip, int port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

static unsigned char byte_at(unsigned long i) { return (unsigned char)((i * 2654435761ul) >> 24); }

#define BULK 100000

/* the server thread: accept one connection, echo it back until the client's end of stream, then shut down */
static void *echo_server(void *arg) {
    int lfd = *(int *)arg;
    struct sockaddr_in peer;
    socklen_t plen = sizeof peer;
    int c = accept(lfd, (struct sockaddr *)&peer, &plen);
    if (c < 0) return (void *)1;
    char buf[1000];
    long total = 0;
    for (;;) {
        long n = recv(c, buf, sizeof buf, 0);
        if (n <= 0) break;
        long done = 0;
        while (done < n) {
            long w = send(c, buf + done, (size_t)(n - done), 0);
            if (w <= 0) return (void *)2;
            done += w;
        }
        total += n;
    }
    shutdown(c, SHUT_WR);
    close(c);
    return (void *)(long)(total == BULK ? 0 : 3);
}

static void refusals(void) {
    CHECK(socket(AF_INET6, SOCK_STREAM, 0) < 0 && errno == EAFNOSUPPORT, "AF_INET6 is EAFNOSUPPORT");
    CHECK(socket(AF_INET, 3, 0) < 0 && errno == EPROTONOSUPPORT, "SOCK_RAW is EPROTONOSUPPORT");
    CHECK(socket(AF_INET, SOCK_STREAM, 17) < 0 && errno == EPROTONOSUPPORT, "TCP with protocol 17 is EPROTONOSUPPORT");
    int s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(s >= 3, "a stream socket gets a descriptor");
    int one = 1;
    CHECK(setsockopt(s, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one) < 0 && errno == ENOPROTOOPT, "SO_KEEPALIVE is ENOPROTOOPT");
    CHECK(setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) == 0, "SO_REUSEADDR is accepted");
    struct linger l = {1, 5};
    CHECK(setsockopt(s, SOL_SOCKET, SO_LINGER, &l, sizeof l) < 0 && errno == EINVAL, "SO_LINGER {1,5} is EINVAL");
    char b[4];
    CHECK(recv(s, b, 4, MSG_PEEK) < 0 && errno == EOPNOTSUPP, "MSG_PEEK is EOPNOTSUPP");
    CHECK(send(s, b, 4, MSG_OOB) < 0 && errno == EOPNOTSUPP, "MSG_OOB is EOPNOTSUPP");
    CHECK(recv(s, b, 4, 0) < 0 && errno == ENOTCONN, "recv on an unconnected stream is ENOTCONN");
    CHECK(listen(s, 4) < 0 && errno == EDESTADDRREQ, "listen without bind is EDESTADDRREQ");
    CHECK(send(1, b, 1, 0) < 0 && errno == ENOTSOCK, "send on a standard stream is ENOTSOCK");
    CHECK(send(200, b, 1, 0) < 0 && errno == EBADF, "send on a closed descriptor is EBADF");
    close(s);
    {
        char r[16];
        CHECK(getrandom(r, sizeof r, 0) < 0 && errno == EACCES, "getrandom without a `random` name in the view is EACCES");
    }
    struct in_addr a;
    CHECK(inet_pton(AF_INET, "10.0.2.15", &a) == 1 && ntohl(a.s_addr) == 0x0a00020f, "inet_pton");
    CHECK(inet_pton(AF_INET, "10.0.2", &a) == 0, "inet_pton refuses a short address");
    char text[INET_ADDRSTRLEN];
    CHECK(inet_ntop(AF_INET, &a, text, sizeof text) != NULL, "inet_ntop");
}

static void tcp_loopback(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = addr_of("0.0.0.0", 7300);
    CHECK(bind(l, (struct sockaddr *)&a, sizeof a) == 0, "bind");
    CHECK(listen(l, 4) == 0, "listen");
    struct sockaddr_in got;
    socklen_t gl = sizeof got;
    CHECK(getsockname(l, (struct sockaddr *)&got, &gl) == 0 && ntohs(got.sin_port) == 7300, "getsockname of the listener");
    pthread_t th;
    pthread_create(&th, NULL, echo_server, &l);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in to = addr_of("127.0.0.1", 7300);
    CHECK(connect(c, (struct sockaddr *)&to, sizeof to) == 0, "connect over loopback");
    gl = sizeof got;
    CHECK(getpeername(c, (struct sockaddr *)&got, &gl) == 0 && ntohs(got.sin_port) == 7300, "getpeername");
    int one = 1;
    CHECK(setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one) == 0, "TCP_NODELAY");
    /* send 100,000 bytes while reading the echo, from this one thread */
    fcntl(c, F_SETFL, fcntl(c, F_GETFL) | O_NONBLOCK);
    long sent = 0, rcvd = 0, bad = 0;
    unsigned char out[1500], in[1500];
    int spins = 0;
    while (rcvd < BULK && spins < 2000000) {
        spins++;
        if (sent < BULK) {
            long n = BULK - sent < 1500 ? BULK - sent : 1500;
            for (long i = 0; i < n; i++) out[i] = byte_at((unsigned long)(sent + i));
            long w = send(c, out, (size_t)n, 0);
            if (w > 0) {
                /* a partial send is possible: resend the tail next time by only counting what went */
                sent += w;
            } else if (errno != EAGAIN) break;
        }
        long r = recv(c, in, sizeof in, 0);
        if (r > 0) {
            for (long i = 0; i < r; i++) if (in[i] != byte_at((unsigned long)(rcvd + i))) bad++;
            rcvd += r;
        } else if (r == 0) break;
        else if (errno == EAGAIN) {
            struct pollfd p = {c, POLLIN | POLLOUT, 0};
            poll(&p, 1, 20);
        } else break;
    }
    CHECK(sent == BULK && rcvd == BULK && bad == 0, "100,000 bytes echoed byte for byte through nonblocking send/recv and poll");
    CHECK(shutdown(c, SHUT_WR) == 0, "shutdown(SHUT_WR)");
    long r = -1;
    for (int i = 0; i < 200 && (r = recv(c, in, sizeof in, 0)) < 0 && errno == EAGAIN; i++) {
        struct pollfd p = {c, POLLIN, 0};
        poll(&p, 1, 50);
    }
    CHECK(r == 0, "the server's end of stream reads as 0 (clean)");
    void *rv = (void *)9;
    pthread_join(th, &rv);
    CHECK(rv == NULL, "the server thread echoed all of it");
    close(c);
    close(l);
}

static void nonblocking_and_poll(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = addr_of("0.0.0.0", 7301);
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 2);
    fcntl(l, F_SETFL, O_NONBLOCK);
    CHECK(accept(l, NULL, NULL) < 0 && errno == EAGAIN, "a nonblocking accept with nothing pending is EAGAIN");
    struct pollfd p = {l, POLLIN, 0};
    CHECK(poll(&p, 1, 30) == 0, "poll on an idle listener times out with 0");
    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in to = addr_of("127.0.0.1", 7301);
    int r = connect(c, (struct sockaddr *)&to, sizeof to);
    CHECK(r < 0 && errno == EINPROGRESS, "a nonblocking connect is EINPROGRESS");
    struct pollfd w = {c, POLLOUT, 0};
    CHECK(poll(&w, 1, 5000) == 1 && (w.revents & POLLOUT), "poll reports the connect finished (POLLOUT)");
    int err = -1;
    socklen_t el = sizeof err;
    getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &el);
    CHECK(err == 0, "SO_ERROR is 0 after a good connect");
    CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLIN), "the listener is POLLIN once a connection arrived");
    int s = accept(l, NULL, NULL);
    CHECK(s >= 3, "accept takes it");
    fd_set rs;
    FD_ZERO(&rs);
    FD_SET(s, &rs);
    struct timeval tv = {0, 20000};
    CHECK(select(s + 1, &rs, NULL, NULL, &tv) == 0, "select with nothing to read times out");
    char ch = 'x';
    send(c, &ch, 1, 0);
    FD_ZERO(&rs);
    FD_SET(s, &rs);
    tv.tv_sec = 5;
    tv.tv_usec = 0;
    CHECK(select(s + 1, &rs, NULL, NULL, &tv) == 1 && FD_ISSET(s, &rs), "select sees the byte");
    char got = 0;
    CHECK(recv(s, &got, 1, 0) == 1 && got == 'x', "and it is read");
    /* a receive timeout */
    struct timeval to2 = {0, 100000};
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &to2, sizeof to2);
    CHECK(recv(s, &got, 1, 0) < 0 && errno == EAGAIN, "SO_RCVTIMEO ends a blocking recv with EAGAIN");
    close(s);
    close(c);
    /* connection refused, reported through SO_ERROR */
    int d = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in none = addr_of("127.0.0.1", 7399);
    connect(d, (struct sockaddr *)&none, sizeof none);
    struct pollfd dw = {d, POLLOUT, 0};
    poll(&dw, 1, 5000);
    err = 0;
    el = sizeof err;
    getsockopt(d, SOL_SOCKET, SO_ERROR, &err, &el);
    CHECK(err == ECONNREFUSED, "a connect to a closed port reports ECONNREFUSED through SO_ERROR");
    close(d);
    close(l);
    int d2 = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(connect(d2, (struct sockaddr *)&none, sizeof none) < 0 && errno == ECONNREFUSED, "a blocking connect to a closed port is ECONNREFUSED");
    close(d2);
}

static void abort_and_reset(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = addr_of("0.0.0.0", 7302);
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 2);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in to = addr_of("127.0.0.1", 7302);
    CHECK(connect(c, (struct sockaddr *)&to, sizeof to) == 0, "connect for the reset test");
    int s = accept(l, NULL, NULL);
    CHECK(s >= 3, "accept for the reset test");
    struct linger lg = {1, 0};
    setsockopt(c, SOL_SOCKET, SO_LINGER, &lg, sizeof lg);
    close(c);
    char b[4];
    long r = recv(s, b, sizeof b, 0);
    CHECK(r < 0 && errno == ECONNRESET, "close with SO_LINGER {1,0} is a reset at the peer");
    CHECK(send(s, b, 1, 0) < 0 && (errno == EPIPE || errno == ECONNRESET), "and a send to it fails");
    close(s);
    close(l);
}

static void names(void) {
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_socktype = SOCK_STREAM;
    CHECK(getaddrinfo("10.0.2.15", "8080", &hints, &res) == 0 && res != NULL, "getaddrinfo of a dotted quad");
    struct sockaddr_in *sin = (struct sockaddr_in *)res->ai_addr;
    CHECK(res->ai_family == AF_INET && res->ai_socktype == SOCK_STREAM && ntohs(sin->sin_port) == 8080 && ntohl(sin->sin_addr.s_addr) == 0x0a00020f && res->ai_next == NULL, "with the address, port and one stream entry");
    freeaddrinfo(res);
    CHECK(getaddrinfo("localhost", NULL, NULL, &res) == 0 && res->ai_next != NULL, "localhost, both socket types when none is asked for");
    freeaddrinfo(res);
    hints.ai_flags = AI_PASSIVE;
    CHECK(getaddrinfo(NULL, "7000", &hints, &res) == 0 && ((struct sockaddr_in *)res->ai_addr)->sin_addr.s_addr == 0, "AI_PASSIVE with no node is the wildcard");
    freeaddrinfo(res);
    hints.ai_flags = AI_NUMERICHOST;
    CHECK(getaddrinfo("example.com", "80", &hints, &res) == EAI_NONAME, "AI_NUMERICHOST refuses a name");
    hints.ai_flags = 0;
    hints.ai_family = AF_INET6;
    CHECK(getaddrinfo("::1", "80", &hints, &res) == EAI_FAMILY, "IPv6 is EAI_FAMILY");
    hints.ai_family = 0;
    CHECK(getaddrinfo("127.0.0.1", "http", &hints, &res) == EAI_SERVICE, "a service name is EAI_SERVICE (numeric only)");
    CHECK(gai_strerror(EAI_NONAME) != NULL, "gai_strerror");
    /* through the harness's scripted DNS server (RESOLVER in the environment), when there is one */
    hints.ai_flags = 0;
    hints.ai_socktype = SOCK_STREAM;
    int r = getaddrinfo("files.test", "80", &hints, &res);
    if (r == 0) {
        CHECK(((struct sockaddr_in *)res->ai_addr)->sin_addr.s_addr == htonl(0x0a000202), "getaddrinfo asks the DNS server and gets 10.0.2.2");
        freeaddrinfo(res);
    } else {
        CHECK(r == EAI_AGAIN, "(no DNS server on the host: getaddrinfo times out as EAI_AGAIN, skipped)");
    }
    CHECK(getaddrinfo("nosuch.test", "80", &hints, &res) == (r == 0 ? EAI_NONAME : EAI_AGAIN), "an unknown name is EAI_NONAME");
}

static void udp(void) {
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = addr_of("0.0.0.0", 7303);
    CHECK(bind(u, (struct sockaddr *)&a, sizeof a) == 0, "bind a UDP socket");
    int v = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in to = addr_of("127.0.0.1", 7303);
    CHECK(sendto(v, "ping", 4, 0, (struct sockaddr *)&to, sizeof to) == 4, "sendto");
    char b[16];
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    struct pollfd p = {u, POLLIN, 0};
    CHECK(poll(&p, 1, 5000) == 1, "poll sees the datagram");
    long n = recvfrom(u, b, sizeof b, 0, (struct sockaddr *)&from, &fl);
    CHECK(n == 4 && memcmp(b, "ping", 4) == 0, "recvfrom returns the datagram");
    CHECK(connect(v, (struct sockaddr *)&to, sizeof to) == 0 && send(v, "pong", 4, 0) == 4, "a connected UDP socket sends to its peer");
    n = recvfrom(u, b, sizeof b, 0, NULL, NULL);
    CHECK(n == 4 && memcmp(b, "pong", 4) == 0, "and the peer receives it");
    CHECK(send(u, "x", 1, 0) < 0 && errno == EDESTADDRREQ, "send on an unconnected UDP socket is EDESTADDRREQ");
    close(u);
    close(v);
}

int sockets_test(void) {
    refusals();
    tcp_loopback();
    nonblocking_and_poll();
    abort_and_reset();
    names();
    udp();
    printf("sockets: %d failures\n", failures);
    return failures;
}
