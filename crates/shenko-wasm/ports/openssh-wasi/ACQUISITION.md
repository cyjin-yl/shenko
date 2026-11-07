# Acquiring OpenSSH for wasm32-wasip1

Status: **unported**. No ssh WASM artifact exists. The host `ssh` client is
not a WASM SSH and is never used in its place (SPEC §8.2 W1, R019).

## Upstream

- Repository: <https://github.com/openssh/openssh-portable>
- Licence: OpenSSH's own BSD-style licence (see the resolved tree's `LICENCE`;
  `shenko-wasm/src/tool.rs` records the SPDX family as `BSD-3-Clause`)
- Target ABI: `wasi_preview1`
- Capability the artifact needs: `cap.network.tcp/1`

## Pin

No commit is recorded here and no hash is guessed. Resolve the release tag from
the project's own repository:

```sh
git ls-remote --tags --refs https://github.com/openssh/openssh-portable 'refs/tags/V_*'

crates/shenko-wasm/ports/resolve-pin.sh \
  https://github.com/openssh/openssh-portable <V_ tag> \
  crates/shenko-wasm/ports/openssh-wasi/pin.json
```

Every later acquisition must reproduce the recorded `tree_sha256`.

## What this runtime already gives the module

- Admitted TCP streams and `sock_shutdown`/readiness (`../wasi-net/`), with the
  run's deadline and cancellation ending a blocked read.
- `random_get` (getrandom) and `clock_time_get` — both already implemented by
  the preview1 host, so key generation and rekey timers have real entropy and
  a real clock.
- The managed home mount for `~/.ssh` files.

## Open port tasks

1. **Client port.** OpenSSH is not written for wasip1: it expects process
   spawning (`ProxyCommand`, `ssh-keysign`, host-key helper), `getpwuid`,
   `termios` on a tty, and a unix-domain agent socket. Each such path must be
   ported or refused explicitly; a refusal is honest, a silent no-op is not.
2. **No unix sockets.** This transport is TCP only, so `SSH_AUTH_SOCK` cannot
   work as-is. Either implement an in-module agent file format, or document
   key-file-only authentication for the first artifact.
3. **Crypto provider.** A wasip1 build of the crypto backend (or the in-tree
   portable provider) is required; the host never signs or decrypts on the
   module's behalf.
4. **No threads.** wasip1 has no threads in this runtime; the client's
   pre-auth `poll`/`select` loops must be driven from single-threaded readiness
   (`poll_oneoff`) instead.
5. **Manifest.** Only after a real artifact exists; measured bytes and sha256
   per binary, installed with `shenko tool install-set`. Until then the target
   stays unresolved.
