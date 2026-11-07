/*
 * Guest side of the Shenko WASI network transport. See include/shenko_net.h
 * for the descriptor ABI this file trusts and re-checks.
 *
 * The host owns admission: it opened every descriptor from a granted target
 * list before this module started. This file therefore never opens, resolves
 * or re-points anything; it only maps a requested host:port onto a descriptor
 * the host already published, and forwards the half-close.
 */
#include "shenko_net.h"

#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* wasi-libc exposes the process environment as the usual POSIX array. */
extern char **environ;

#define SHENKO_NET_KEY_MAX 32

/* Raw preview1 import: `sock_shutdown(fd, sdflags) -> errno`. */
__attribute__((import_module("wasi_snapshot_preview1"), import_name("sock_shutdown")))
extern uint16_t shenko_net_wasi_sock_shutdown(uint32_t fd, uint16_t how);

static int descriptor_key(int index, char out[SHENKO_NET_KEY_MAX]) {
  int written = snprintf(out, SHENKO_NET_KEY_MAX, "SHENKO_NET_%d", index);
  return written > 0 && written < SHENKO_NET_KEY_MAX ? 0 : -1;
}

static const char *environment_value(const char *key) {
  size_t key_len = strlen(key);
  for (char **entry = environ; entry != NULL && *entry != NULL; entry++) {
    if (strncmp(*entry, key, key_len) == 0 && (*entry)[key_len] == '=') {
      return *entry + key_len + 1;
    }
  }
  return NULL;
}

/* Parse "<fd>,<host>,<port>,<op>[+<op>...]". Returns 0 on success. */
static int descriptor_parse(const char *value, struct shenko_net_target *out) {
  if (value == NULL) {
    return -1;
  }
  const char *first = strchr(value, ',');
  const char *second = first == NULL ? NULL : strchr(first + 1, ',');
  const char *third = second == NULL ? NULL : strchr(second + 1, ',');
  if (third == NULL) {
    return -1;
  }
  char *end = NULL;
  unsigned long fd = strtoul(value, &end, 10);
  if (end != first || fd > (unsigned long)INT_MAX) {
    return -1;
  }
  unsigned long port = strtoul(second + 1, &end, 10);
  if (end != third || port == 0 || port > 0xfffful) {
    return -1;
  }
  size_t host_len = (size_t)(second - (first + 1));
  if (host_len == 0 || host_len > SHENKO_NET_HOST_MAX) {
    return -1;
  }
  unsigned operations = 0;
  const char *ops = third + 1;
  while (*ops != '\0') {
    const char *plus = strchr(ops, '+');
    size_t op_len = plus == NULL ? strlen(ops) : (size_t)(plus - ops);
    if (op_len == 7 && strncmp(ops, "connect", op_len) == 0) {
      operations |= SHENKO_NET_OP_CONNECT;
    } else if (op_len == 6 && strncmp(ops, "listen", op_len) == 0) {
      operations |= SHENKO_NET_OP_LISTEN;
    } else if (op_len == 6 && strncmp(ops, "accept", op_len) == 0) {
      operations |= SHENKO_NET_OP_ACCEPT;
    } else {
      return -1;
    }
    if (plus == NULL) {
      break;
    }
    ops = plus + 1;
  }
  if (operations == 0) {
    return -1;
  }
  out->fd = (int)fd;
  out->port = (uint16_t)port;
  out->operations = operations;
  memcpy(out->host, first + 1, host_len);
  out->host[host_len] = '\0';
  return 0;
}

int shenko_net_target_count(void) {
  int count = 0;
  while (count < SHENKO_NET_DESCRIPTOR_MAX) {
    char key[SHENKO_NET_KEY_MAX];
    struct shenko_net_target target;
    if (descriptor_key(count, key) != 0) {
      break;
    }
    if (descriptor_parse(environment_value(key), &target) != 0) {
      break;
    }
    count += 1;
  }
  return count;
}

int shenko_net_target(int index, struct shenko_net_target *out) {
  if (out == NULL || index < 0 || index >= SHENKO_NET_DESCRIPTOR_MAX) {
    errno = EINVAL;
    return -1;
  }
  char key[SHENKO_NET_KEY_MAX];
  if (descriptor_key(index, key) != 0) {
    errno = EINVAL;
    return -1;
  }
  if (descriptor_parse(environment_value(key), out) != 0) {
    errno = ENOTCONN;
    return -1;
  }
  return 0;
}

int shenko_net_connect(const char *host, uint16_t port) {
  if (host == NULL || host[0] == '\0' || port == 0) {
    errno = EINVAL;
    return -1;
  }
  int count = shenko_net_target_count();
  for (int index = 0; index < count; index++) {
    struct shenko_net_target target;
    if (shenko_net_target(index, &target) != 0) {
      continue;
    }
    if ((target.operations & SHENKO_NET_OP_CONNECT) == 0) {
      continue;
    }
    if (target.port != port) {
      continue;
    }
    if (strcmp(target.host, host) != 0) {
      continue;
    }
    return target.fd;
  }
  errno = ENOTCONN;
  return -1;
}

uint16_t shenko_net_shutdown(int fd, int how) {
  uint16_t flags = (uint16_t)(how & (SHENKO_NET_SHUT_RD | SHENKO_NET_SHUT_WR));
  if (flags == 0 || fd < 0) {
    return SHENKO_NET_ERRNO_INVAL;
  }
  return shenko_net_wasi_sock_shutdown((uint32_t)fd, flags);
}
