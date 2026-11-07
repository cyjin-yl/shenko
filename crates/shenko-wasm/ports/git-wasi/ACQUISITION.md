# Acquiring git for wasm32-wasip1

Status: **unported**. No git WASM artifact exists, so nothing here is claimed
to build, and building this repository's host git is never a substitute.

## Upstream

- Repository: <https://git.kernel.org/pub/scm/git/git.git>
  (review mirror: <https://github.com/git/git>)
- Licence: `GPL-2.0-only`
- Target ABI: `wasi_preview1`
- Capability the artifact needs: `cap.network.tcp/1` (provided by this
  runtime; guest side in `../wasi-net/`)

## Pin

No commit is recorded in this directory yet, and no hash is guessed. Resolve
the exact revision from the project's own repository once, on a machine with
network access:

```sh
# list the project's own release tags; pick the one this port targets
git ls-remote --tags --refs https://git.kernel.org/pub/scm/git/git.git 'refs/tags/v2.*'

crates/shenko-wasm/ports/resolve-pin.sh \
  https://git.kernel.org/pub/scm/git/git.git <release-tag> \
  crates/shenko-wasm/ports/git-wasi/pin.json
```

`pin.json` then carries the tag, the released commit and the sha256 of
`git archive` of that tree. Every later acquisition must reproduce that
`tree_sha256`; a different value means a different revision, not a build
detail. Re-pinning is a new file with a documented reason, never an edit of a
pin a build already used.

## Open port tasks

Verified against the resolved revision at acquisition time; each item is work,
not a claim that it is impossible.

1. **Socket layer.** The wasi-sdk 34 wasip1 sysroot declares no `socket()`. The
   handful of calls git's transport actually uses must be implemented over
   `libshenko_net.a` (see `../wasi-net/`): take the admitted descriptor, then
   `read`/`write`/`sock_shutdown`. Keep the change in git's portability layer;
   never shell out to a host git.
2. **No fork/exec.** wasip1 has no `fork`/`execve`/`posix_spawn`, so git's
   run-command path, dashed-builtin dispatch and the local
   `upload-pack`/`receive-pack` helpers cannot spawn. Builtins must run
   in-process, and anything that genuinely needs a child must fail with an
   explicit error instead of silently succeeding.
3. **Credentials.** Helper executables cannot run. Use the tool admission's
   stdin credential (`SHENKO_TARGET_CREDENTIAL=stdin`) or a URL without
   credentials; do not add a host-side credential path.
4. **HTTPS.** No wasip1 libcurl or TLS provider ships here. Either build both
   for wasip1 or restrict the first artifact to `git://`/plain `http` remotes
   and say so in the manifest. TLS support must not be claimed before it
   exists.
5. **Missing POSIX surface.** Build with git's documented `NO_*` knobs rather
   than patching around absent headers: no `mmap`, no threads, no iconv, no
   gettext, no Perl/Python/Tcl. Which knobs the resolved revision actually
   needs is read from its `Makefile`/`config.mak.uname` at port time.
6. **Manifest.** Only after a real artifact exists, write a tool-set manifest
   with measured bytes and sha256 per binary (schema:
   `../toybox-wasi/toybox-wasi.manifest.json`) and install it with
   `shenko tool install-set`. Until then this port stays unresolved — source
   presence is not an installed tool.
