# tea-wasi — Gitea CLI (`tea`) WASM port

Status: **UNRESOLVED** — no verified wasm32-wasip1 artifact exists.

## Upstream

- Project: `go-gitea/gitea` (CLI binary: `tea`)
- License: MIT
- Language: Go
- Repository: <https://gitea.com/gitea/tea>

## Why unresolved

`tea` is a Go CLI that calls the Gitea REST API over HTTP(S). A
`GOOS=wasip1 GOARCH=wasm` build links but produces a module whose
net/http calls fail at runtime: Go's wasip1 port stubs out sockets
and the standard `net` package returns "not supported" errors.

For `tea` to work inside this runtime the module must route its HTTP
calls through the host-provided admitted preview1 transport sockets
(`crates/shenko-wasm/src/net.rs`). Two approaches exist:

1. **Go `GOOS=wasip1` with patched `net` transport** — replace
   `net.Dial` / `net/http.Transport` at build time with a WASI shim
   that reads the `SHENKO_NET_*` preopened descriptors instead of
   calling `socket(2)`. This is what the `wasi-net` C port provides
   as a reference for C guests; a Go equivalent requires patching Go's
   `net` package or vendoring a WASI-aware dialer.

2. **TinyGo WASI target** — TinyGo's wasip1 target has more limited
   stdlib, but its net package can be replaced with less surgery.

Neither approach has a verified artifact today.

## Required capabilities

- `cap.network.tcp/1` — host-admitted TCP sockets (preview1 transport)
- `cap.network.dns/1` — host-side DNS resolution for API endpoints
- `cap.process.deny/1` — no child-process creation inside the module

## Acquisition steps (when attempted)

1. Build `tea` with `GOOS=wasip1 GOARCH=wasm` from a pinned gitea/tea
   release tag.
2. Patch net transport to use WASI preview1 sockets from
   `SHENKO_NET_*` descriptors (or link the `wasi-net` shim).
3. Smoke-test: `tea login --url ... && tea repos ls` against a local
   Gitea instance with admitted transport.
4. Record SHA-256 of the resulting `.wasm`, runtime version, and
   compatibility class in `PORT-STATUS.json`.

## Dependencies

- `crates/shenko-wasm/ports/wasi-net/` — host transport layer
- Admitted target declaration in the host runtime for the Gitea API
  endpoint(s).
