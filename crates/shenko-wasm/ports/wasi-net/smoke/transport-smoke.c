/*
 * A real guest that exercises `cap.network.tcp/1` end to end: it takes the
 * first descriptor the host admitted for an outbound stream, sends one
 * request line, half-closes, and copies whatever the peer sends back to
 * stdout. Port authors use it to check a host build; the crate's native
 * integration tests cover the same path with asserted payloads.
 *
 * Usage: transport-smoke.wasm [host] [port] [payload]
 * `host`/`port` default to the first admitted `connect` descriptor, so a
 * plain `transport-smoke.wasm` never invents an endpoint.
 */
#include "shenko_net.h"

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define MAX_EXCHANGE 4096

static int write_all(int fd, const char *bytes, size_t len) {
  size_t written = 0;
  while (written < len) {
    ssize_t step = write(fd, bytes + written, len - written);
    if (step < 0) {
      if (errno == EINTR) {
        continue;
      }
      return -1;
    }
    if (step == 0) {
      return -1;
    }
    written += (size_t)step;
  }
  return 0;
}

int main(int argc, char **argv) {
  struct shenko_net_target target;
  if (shenko_net_target(0, &target) != 0) {
    fprintf(stderr, "no admitted network target\n");
    return 10;
  }
  const char *host = argc > 1 ? argv[1] : target.host;
  uint16_t port = target.port;
  if (argc > 2) {
    char *end = NULL;
    unsigned long parsed = strtoul(argv[2], &end, 10);
    if (end == NULL || *end != '\0' || parsed == 0 || parsed > 0xfffful) {
      fprintf(stderr, "port must be 1..65535\n");
      return 11;
    }
    port = (uint16_t)parsed;
  }
  const char *payload = argc > 3 ? argv[3] : "PING\n";

  int fd = shenko_net_connect(host, port);
  if (fd < 0) {
    fprintf(stderr, "target %s:%u is not admitted for connect\n", host, (unsigned)port);
    return 12;
  }
  if (write_all(fd, payload, strlen(payload)) != 0) {
    fprintf(stderr, "write failed\n");
    return 13;
  }
  uint16_t shutdown_errno = shenko_net_shutdown(fd, SHENKO_NET_SHUT_WR);
  if (shutdown_errno != 0) {
    fprintf(stderr, "shutdown failed: preview1 errno %u\n", (unsigned)shutdown_errno);
    return 14;
  }

  char buffer[MAX_EXCHANGE];
  size_t total = 0;
  for (;;) {
    ssize_t read_now = read(fd, buffer + total, sizeof(buffer) - total);
    if (read_now < 0) {
      if (errno == EINTR) {
        continue;
      }
      fprintf(stderr, "read failed\n");
      return 15;
    }
    if (read_now == 0) {
      break;
    }
    total += (size_t)read_now;
    if (total == sizeof(buffer)) {
      break;
    }
  }
  if (write_all(1, buffer, total) != 0) {
    fprintf(stderr, "stdout write failed\n");
    return 16;
  }
  return 0;
}
