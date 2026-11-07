# Acquiring gh (GitHub CLI) for wasm32-wasip1

Status: **unported**. No gh WASM artifact exists.

## Upstream

- Repository: <https://github.com/cli/cli>
- Licence: `MIT`
- Target ABI: `wasi_preview1`
- Capability the artifact needs: `cap.network.tcp/1`

## Pin

No commit is recorded here and no hash is guessed:

```sh
git ls-remote --tags --refs https://github.com/cli/cli 'refs/tags/v*'

crates/shenko-wasm/ports/resolve-pin.sh \
  https://github.com/cli/cli <release-tag> \
  crates/shenko-wasm/ports/gh-wasi/pin.json
```

## Open port tasks

1. **Networking.** gh's API access is entirely HTTP, but preview1 offers no
   socket creation call: a guest can only use descriptors the host preopened,
   and the wasi-sdk 34 wasip1 sysroot declares no `socket()` for it to call.
   The port must therefore route `net` through the admitted descriptors
   (`../wasi-net/`): the module speaks HTTP itself, the host only transports
   bytes to an admitted target.
2. **Authentication.** gh stores tokens in a keyring or a config file. The
   artifact either reads the token from the tool admission's stdin credential
   or from the managed home mount; it must not expect a host credential helper,
   and it must not spawn a browser.
3. **No helper processes.** Editor, pager, `git` delegation, extension
   executables and aggregation commands that launch children must be refused
   explicitly for the first artifact rather than silently degraded.
4. **TLS.** gh speaks HTTPS. A wasip1 TLS provider has to be built alongside
   it; claiming API support without it would be a false capability.
5. **Manifest.** Only after a real artifact exists; measured bytes and sha256
   per binary, installed with `shenko tool install-set`. Until then the target
   stays unresolved.
