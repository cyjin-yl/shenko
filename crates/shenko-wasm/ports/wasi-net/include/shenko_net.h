// Guest side of the Shenko WASI network transport (`cap.network.tcp/1`).
//
// A preview1 guest cannot create a socket: `wasi_snapshot_preview1` has no
// `socket`/`connect`. The host opens every admitted, target-bound socket
// before the guest starts and publishes each descriptor in a host-owned
// environment entry:
//
//   SHENKO_NET_<index>=<fd>,<host>,<port>,<op>[+<op>...]
//
// `<index>` counts from 0 in admission order; `<op>` is `connect`, `listen` or
// `accept`. The values contain no whitespace and the separator characters
// (`,` and `+`) never occur inside `<host>`. A caller-supplied entry with this
// prefix is refused by the host, so a guest can trust the list it reads.
//
// Payload I/O uses ordinary POSIX calls on the returned descriptor: the host
// serves `read`/`write` (preview1 `fd_read`/`fd_write`) on a stream socket,
// and `shenko_net_shutdown` performs the half-close. `close` closes it.
//
// Nothing here resolves names or opens sockets: an unadmitted host:port is
// refused locally, so port code cannot widen its own reach even if it is
// confused about its arguments.
#ifndef SHENKO_NET_H
#define SHENKO_NET_H

#include <stddef.h>
#include <stdint.h>

#define SHENKO_NET_HOST_MAX 512
#define SHENKO_NET_DESCRIPTOR_MAX 8

/* Operations a descriptor may grant. */
#define SHENKO_NET_OP_CONNECT 0x1u
#define SHENKO_NET_OP_LISTEN 0x2u
#define SHENKO_NET_OP_ACCEPT 0x4u

/* Shutdown directions (`sock_shutdown` `sdflags`). */
#define SHENKO_NET_SHUT_RD 0x1
#define SHENKO_NET_SHUT_WR 0x2

/*
 * Preview1 errno numbers this library returns directly (the witx errno
 * ordering: success 0, ... exist 20, ... inval 28, ... notconn 110). They are
 * the guest ABIs own values, not an invented scale.
 */
#define SHENKO_NET_ERRNO_INVAL 28u
#define SHENKO_NET_ERRNO_NOTCONN 110u

struct shenko_net_target {
  int fd;
  char host[SHENKO_NET_HOST_MAX + 1];
  uint16_t port;
  unsigned operations;
};

/* Number of descriptors the host published for this run. */
int shenko_net_target_count(void);

/*
 * Copy descriptor `index`. Returns 0 on success, -1 when no such descriptor
 * exists (or it is malformed, which the host never emits).
 */
int shenko_net_target(int index, struct shenko_net_target *out);

/*
 * Return the installed descriptor for the admitted `host:port` outbound
 * stream, or -1 with `errno` set: `ENOTCONN` when the pair is not admitted for
 * `connect`, `EINVAL` for a malformed argument. No host call is made.
 */
int shenko_net_connect(const char *host, uint16_t port);

/*
 * Shut down one direction of an installed socket. `how` is a mask of
 * SHENKO_NET_SHUT_RD/WR and must be non-zero. Returns the preview1 errno
 * (0 on success).
 */
uint16_t shenko_net_shutdown(int fd, int how);

#endif /* SHENKO_NET_H */
