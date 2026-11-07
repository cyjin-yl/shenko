# Guest transport for the WASI preview1 network capability

This directory is the guest side of `cap.network.tcp/1`. The host side is
`crates/shenko-wasm/src/net.rs` (`NetworkAdmission`, `open_admitted`) plus the
preview1 socket host calls in `crates/shenko-wasm/src/p1.rs`.

## What the host provides, and what it does not

- The host opens a socket only for a target an admission names, before the
  guest exists: `connect` gives one connected TCP stream, `listen` binds a
  listener, `accept` lets the guest take inbound streams from it.
- Descriptors are installed after the directory preopens, in admission order,
  and published to the guest as `SHENKO_NET_<index>=<fd>,<host>,<port>,<ops>`.
  A caller-supplied key with that prefix is refused, so the guest can trust the
  list.
- The guest gets the ordinary preview1 socket surface on those descriptors:
  `read`/`write` (host `fd_read`/`fd_write`), `poll_oneoff` readiness,
  `sock_recv`/`sock_send` with scatter/gather, `sock_shutdown`, `sock_accept`,
  `fd_fdstat_set_flags` `NONBLOCK`, `fd_close`.
- Every blocking socket call polls in bounded slices against the run's stop
  cell, so cancellation and the deadline end a guest blocked on a silent peer.
  A run that carries an admission must also carry a deadline or a cancellation
  handle; the host refuses an unbounded networked run.
- There is no socket creation, no name resolution and no re-pointing in the
  guest. A host:port the admission did not name cannot be reached at all.
- TCP only. Preview1 here exposes no datagram socket, no unix socket and no
  TLS: a port that needs TLS builds it in-module.

## What this port contains

- `include/shenko_net.h`, `src/shenko_net.c` — the descriptor shim: enumerate
  the host's descriptors, map an admitted `host:port` onto its descriptor,
  half-close. It also re-checks admission in the guest, so port code cannot
  widen its reach even if its own arguments are wrong.
- `smoke/transport-smoke.c` — a real command that sends one request line to the
  first admitted stream, half-closes, and copies the reply to stdout.
- `build.sh` — builds `libshenko_net.a` plus `transport-smoke.wasm` into a fresh
  run directory. Nothing is downloaded.

## Building

```sh
crates/shenko-wasm/ports/wasi-net/build.sh /path/to/wasi-sdk /tank/shared/.../runs/wasi-net-1
```

The output archive is the input for the tool ports in `../git-wasi`,
`../openssh-wasi`, `../gh-wasi` and `../tea-wasi`. Building this shim does not
build or claim any tool artifact: those ports remain unresolved until a real
artifact exists.
