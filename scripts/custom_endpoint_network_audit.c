// Test-only macOS interposer: record and reject non-loopback network activity.
// Loaded only into the smoke-test subprocess, never installed system-wide.
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static void record_attempt(const char *operation, const char *host, int port, bool allowed) {
    const char *path = getenv("CUSTOM_ENDPOINT_NETWORK_AUDIT_LOG");
    if (!path) return;
    int fd = open(path, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd < 0) return;
    char line[512];
    int size = snprintf(line, sizeof(line), "%s %s %d %s\n", operation, host, port,
                        allowed ? "allowed" : "blocked");
    if (size > 0) (void)write(fd, line, (size_t)size < sizeof(line) ? (size_t)size : sizeof(line) - 1);
    close(fd);
}

static bool check_address(const char *operation, const struct sockaddr *address) {
    if (!address || (address->sa_family != AF_INET && address->sa_family != AF_INET6)) return true;
    char host[INET6_ADDRSTRLEN];
    int port;
    bool allowed;
    if (address->sa_family == AF_INET) {
        const struct sockaddr_in *ipv4 = (const struct sockaddr_in *)address;
        inet_ntop(AF_INET, &ipv4->sin_addr, host, sizeof(host));
        port = ntohs(ipv4->sin_port);
        allowed = (ntohl(ipv4->sin_addr.s_addr) >> 24) == 127;
    } else {
        const struct sockaddr_in6 *ipv6 = (const struct sockaddr_in6 *)address;
        inet_ntop(AF_INET6, &ipv6->sin6_addr, host, sizeof(host));
        port = ntohs(ipv6->sin6_port);
        allowed = IN6_IS_ADDR_LOOPBACK(&ipv6->sin6_addr);
    }
    record_attempt(operation, host, port, allowed);
    if (!allowed) errno = EACCES;
    return allowed;
}

static int audit_connect(int fd, const struct sockaddr *address, socklen_t length) {
    return check_address("connect", address) ? connect(fd, address, length) : -1;
}

static int audit_connectx(int fd, const sa_endpoints_t *endpoints, sae_associd_t association,
                          unsigned int flags, const struct iovec *iov, unsigned int count,
                          size_t *length, sae_connid_t *connection) {
    return check_address("connectx", endpoints ? endpoints->sae_dstaddr : NULL)
        ? connectx(fd, endpoints, association, flags, iov, count, length, connection) : -1;
}

static ssize_t audit_sendto(int fd, const void *buffer, size_t size, int flags,
                            const struct sockaddr *address, socklen_t length) {
    return check_address("sendto", address) ? sendto(fd, buffer, size, flags, address, length) : -1;
}

static ssize_t audit_sendmsg(int fd, const struct msghdr *message, int flags) {
    return check_address("sendmsg", message ? message->msg_name : NULL)
        ? sendmsg(fd, message, flags) : -1;
}

static int audit_getaddrinfo(const char *host, const char *service,
                             const struct addrinfo *hints, struct addrinfo **result) {
    struct in_addr ipv4;
    struct in6_addr ipv6;
    bool allowed = !host || !strcmp(host, "localhost")
        || (inet_pton(AF_INET, host, &ipv4) == 1 && (ntohl(ipv4.s_addr) >> 24) == 127)
        || (inet_pton(AF_INET6, host, &ipv6) == 1 && IN6_IS_ADDR_LOOPBACK(&ipv6));
    record_attempt("dns", host ? host : "(null)", 0, allowed);
    if (!allowed) return EAI_FAIL;
    return getaddrinfo(host, service, hints, result);
}

#define INTERPOSE(replacement, original) \
    __attribute__((used)) static struct { const void *new_fn; const void *old_fn; } \
    interpose_##original __attribute__((section("__DATA,__interpose"))) = \
    { (const void *)(replacement), (const void *)(original) }

INTERPOSE(audit_connect, connect);
INTERPOSE(audit_connectx, connectx);
INTERPOSE(audit_sendto, sendto);
INTERPOSE(audit_sendmsg, sendmsg);
INTERPOSE(audit_getaddrinfo, getaddrinfo);
