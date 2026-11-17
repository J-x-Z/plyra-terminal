#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define REQUEST_TIMEOUT_MS 4000
#define MAX_RESPONSE_BYTES 4096

#if defined(__aarch64__) && defined(__BIONIC__)
// Static ARM64 executables need PT_TLS aligned to Bionic's 64-byte TCB.
// Keep a reference so section garbage collection retains the alignment.
_Alignas(64) static _Thread_local volatile unsigned char tls_alignment;
#endif

static void usage(FILE *stream) {
    fputs("Usage: glass-android {battery|settings|--help}\n"
          "Uses this session's GLASS_ANDROID_API abstract Unix socket.\n"
          "battery: JSON level, scale, status and temperature (tenths Celsius).\n"
          "settings: open Plyra Terminal settings through its Android UI.\n", stream);
}

static int64_t monotonic_ms(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return -1;
    return (int64_t) value.tv_sec * 1000 + value.tv_nsec / 1000000;
}

static int wait_ready(int fd, short events, int64_t deadline) {
    for (;;) {
        int64_t now = monotonic_ms();
        if (now < 0) return -1;
        int64_t remaining = deadline - now;
        if (remaining <= 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        struct pollfd item = {.fd = fd, .events = events};
        int result = poll(&item, 1, (int) remaining);
        if (result < 0 && errno == EINTR) continue;
        if (result == 0) {
            errno = ETIMEDOUT;
            return -1;
        }
        if (result < 0) return -1;
        if (item.revents & (events | POLLHUP)) return 0;
        errno = EIO;
        return -1;
    }
}

static int response_success(const char *json) {
    // Our bounded server emits the boolean `ok` as its first JSON field.
    size_t end = strlen(json);
    while (end > 0 && (json[end - 1] == ' ' || json[end - 1] == '\t' || json[end - 1] == '\r')) end--;
    if (end == 0 || json[end - 1] != '}') return -1;
    const char *cursor = json;
    while (*cursor == ' ' || *cursor == '\t' || *cursor == '\r') cursor++;
    if (*cursor++ != '{') return -1;
    while (*cursor == ' ' || *cursor == '\t') cursor++;
    if (strncmp(cursor, "\"ok\"", 4) != 0) return -1;
    cursor += 4;
    while (*cursor == ' ' || *cursor == '\t') cursor++;
    if (*cursor++ != ':') return -1;
    while (*cursor == ' ' || *cursor == '\t') cursor++;
    size_t length;
    int success;
    if (strncmp(cursor, "true", 4) == 0) {
        length = 4;
        success = 1;
    } else if (strncmp(cursor, "false", 5) == 0) {
        length = 5;
        success = 0;
    } else {
        return -1;
    }
    cursor += length;
    while (*cursor == ' ' || *cursor == '\t' || *cursor == '\r') cursor++;
    if (*cursor != ',' && *cursor != '}') return -1;
    return success;
}

static int request(const char *name, const char *action) {
    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    size_t name_length = strnlen(name, sizeof(address.sun_path));
    if (name_length == 0 || name_length > sizeof(address.sun_path) - 2) {
        fputs("glass-android: invalid GLASS_ANDROID_API name length\n", stderr);
        return 1;
    }
    for (size_t index = 0; index < name_length; index++) {
        unsigned char byte = (unsigned char) name[index];
        if (!((byte >= 'a' && byte <= 'z') || (byte >= 'A' && byte <= 'Z')
                || (byte >= '0' && byte <= '9') || byte == '.' || byte == '_' || byte == '-')) {
            fputs("glass-android: GLASS_ANDROID_API must be a raw abstract socket name\n", stderr);
            return 1;
        }
    }
    memcpy(address.sun_path + 1, name, name_length);
    int64_t start = monotonic_ms();
    if (start < 0) {
        perror("glass-android: clock");
        return 1;
    }
    int64_t deadline = start + REQUEST_TIMEOUT_MS;
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (fd < 0) {
        perror("glass-android: socket");
        return 1;
    }
    int result = 1;
    socklen_t address_length = (socklen_t) (offsetof(struct sockaddr_un, sun_path) + 1 + name_length);
    if (connect(fd, (struct sockaddr *) &address, address_length) != 0) {
        if (errno != EINPROGRESS || wait_ready(fd, POLLOUT, deadline) != 0) goto failed;
        int error = 0;
        socklen_t length = sizeof(error);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) != 0) goto failed;
        if (error != 0) {
            errno = error;
            goto failed;
        }
    }
    struct ucred peer;
    socklen_t peer_length = sizeof(peer);
    if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &peer, &peer_length) != 0) goto failed;
    if (peer_length != sizeof(peer) || peer.uid != geteuid()) {
        fputs("glass-android: API server UID differs from this session\n", stderr);
        goto done;
    }
    char command[16];
    int command_length = snprintf(command, sizeof(command), "%s\n", action);
    if (command_length < 0 || (size_t) command_length >= sizeof(command)) goto done;
    size_t sent = 0;
    while (sent < (size_t) command_length) {
        if (wait_ready(fd, POLLOUT, deadline) != 0) goto failed;
        ssize_t count = send(fd, command + sent, (size_t) command_length - sent, MSG_NOSIGNAL);
        if (count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) continue;
        if (count <= 0) goto failed;
        sent += (size_t) count;
    }
    char response[MAX_RESPONSE_BYTES + 1];
    size_t received = 0;
    while (received < MAX_RESPONSE_BYTES) {
        if (wait_ready(fd, POLLIN, deadline) != 0) goto failed;
        ssize_t count = recv(fd, response + received, MAX_RESPONSE_BYTES - received, 0);
        if (count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) continue;
        if (count <= 0) {
            if (count == 0) errno = EPROTO;
            goto failed;
        }
        size_t previous = received;
        received += (size_t) count;
        for (size_t index = previous; index < received; index++) {
            if (response[index] == '\0') {
                errno = EPROTO;
                goto failed;
            }
            if (response[index] == '\n') {
                response[index] = '\0';
                int success = response_success(response);
                if (success < 0) {
                    errno = EPROTO;
                    goto failed;
                }
                if (puts(response) == EOF) goto failed;
                result = success ? 0 : 1;
                goto done;
            }
        }
    }
    errno = EMSGSIZE;
failed:
    perror("glass-android: API request");
done:
    close(fd);
    return result;
}

int main(int argc, char **argv) {
#if defined(__aarch64__) && defined(__BIONIC__)
    tls_alignment = 0;
#endif
    if (argc == 2 && strcmp(argv[1], "--help") == 0) {
        usage(stdout);
        return 0;
    }
    if (argc != 2 || (strcmp(argv[1], "battery") != 0 && strcmp(argv[1], "settings") != 0)) {
        usage(stderr);
        return 2;
    }
    const char *name = getenv("GLASS_ANDROID_API");
    if (name == NULL || *name == '\0') {
        fputs("glass-android: GLASS_ANDROID_API is not available in this session\n", stderr);
        return 1;
    }
    return request(name, argv[1]);
}

// FVPROV1 7549 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMWRmYjZkNWViOWIwNjFmNTFhNDRkYmMxYWIwZGJmODBlOWRmNmU1MTg1ODU4YzYxODI4ZmU2NTVhNjJiNjFjMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNTlmNDliMzEyYjI4YWJiYzE2ZTRhZDM2MDlhNTA5ZWY4YjllNTliN2FhZmI1YzA5YzU3NDUwOTJlN2Y4ZjhhNDRkZTJlOTQ4MGE3ZjliM2Y4MzJhNzlhN2NkYjVlMjVlMTRlNGQwODU3YjI3MzViMGQ3Mjg1Yjk1ZTczYWE3MDIifV19XQ== FVPROV1-END
