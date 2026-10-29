# Acquiring inputs for the toybox→WASI port

Everything below is pinned; verify every checksum before building. The port
downloads nothing itself — all inputs are local artifacts you place here.

## 1. Upstream source

- **File:** `toybox-0.8.15.tar.gz`
- **URL:** https://landley.net/toybox/downloads/toybox-0.8.15.tar.gz
  (mirror: https://github.com/landley/toybox/releases/tag/0.8.15)
- **SHA-256:** `52452b68a6cf50f9a9728b0f844b03c4dd1f70463675b202e6cf0475bda3934f`

Unpack into this directory so `toybox-0.8.15/` sits next to `build.sh`:

```sh
tar xf toybox-0.8.15.tar.gz
# Verify hash first:
sha256sum toybox-0.8.15.tar.gz
```

## 2. Apply the port patches

```sh
cd toybox-0.8.15
for p in ../patches/*.patch; do patch -p1 < "$p"; done
```

All seven patches must apply with zero fuzz; `build.sh` refuses to run
against an unpatched tree. The patches add `#ifdef __wasi__` guards only —
upstream behavior on other platforms is unchanged.

## 3. WASI SDK

- **File:** `wasi-sdk-34.0-x86_64-linux.tar.gz`
- **URL:** https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34
- **Release asset:** id `528366474`, size `192383077` bytes
- **SHA-256 (release-asset metadata digest):**
  `b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4`

The release publishes no `SHA256SUMS` file; verify the downloaded tarball
byte count and `sha256sum` against the asset metadata above (bound to tag
`wasi-sdk-34`), not a signed manifest.

Extract and pass the top-level directory to `build.sh`:

```sh
tar xf wasi-sdk-34.0-x86_64-linux.tar.gz
./build.sh /path/to/wasi-sdk-34.0-x86_64-linux
```

The SDK must contain `bin/clang` and `share/wasi-sysroot`; `build.sh`
compiles a probe binary first and exits clearly if the SDK is wrong.

## 4. Host build tools

Toybox generates its config/flags headers with a *host* compile of
`kconfig/` and `scripts/` helpers. Requires: `cc` or `gcc`, `make`, `bash`,
`sed`, `egrep`, `xargs`, `tr`. No network access is used.

## 5. Output

`build.sh` writes `out-wasi/<applet>.wasm` — one standalone WASI preview1
module per enabled applet. Each is installable with:

```sh
shenko tool install --name toybox-<applet> --version 0.8.15 \
  --abi wasi_preview1 --file out-wasi/<applet>.wasm
```

Run `SMOKE.md`'s checks before publishing artifacts.
