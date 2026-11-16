# SSH client inside WASI preview1

This port builds a real SSH client from `russh 0.65.0`, pinned by its own
`Cargo.lock`. Key exchange, server-key verification, Ed25519 key generation,
public-key authentication, command execution and SFTP v3 run inside the WASM
module. The host supplies the already admitted TCP descriptor, clocks, entropy
and the managed file mounts. No host `ssh`, signing process or SSH RPC stands
in for the client. This is an SSH client port, not a build of OpenSSH itself.

## Build and provenance

Use the repository's pinned Rust toolchain with `wasm32-wasip1` installed and
WASI SDK 34. The SDK archive is `wasi-sdk-34.0-x86_64-linux.tar.gz` from
[the official release](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-34),
SHA-256 `b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4`.
The build script receives a local SDK and retains the exact source hashes,
compiler/SDK identity, complete build log and resulting artifact checksum in a
new output directory. It never overwrites an earlier evidence directory.

```sh
python3 crates/shenko-wasm/ports/ssh-wasi/build.py \
  --sdk /path/to/wasi-sdk-34.0-x86_64-linux \
  --target-dir /path/to/cargo-cache \
  --output-dir /path/to/new-ssh-build
```

The repository's existing `third-party/russh-duplex` is the pinned russh
source. `runtime-support/` contains russh-util 0.52.0 (Apache-2.0), originating
from the crates.io archive with checksum
`668424a5dde0bcb45b55ba7de8476b93831b4aa2fa6947e145f3b053e22c60b6`.
The only runtime adaptation is to select Tokio's single-threaded WASI
executor and `std::time::Instant` on WASI; browser-only wasm-bindgen remains
restricted to non-WASI wasm. This avoids an invented JavaScript host import.
The guest's transport is a bounded, nonblocking adapter over Shenko's actual
preview1 file/socket operations. All other dependencies are checksum-pinned
in the guest lockfile and retain their upstream licenses.

## Install and invoke

After initializing a managed home, install the verified artifact:

```sh
shenko tool install --name ssh --version 0.1.0 --abi wasi_preview1 \
  --file /path/to/new-ssh-build/shenko-ssh-wasi.wasm \
  --source-uri https://github.com/cyjin-yl/shenko/tree/HEAD/crates/shenko-wasm/ports/ssh-wasi \
  --source-release 0.1.0 --license Apache-2.0 --requires-host clock --requires-host random
```

Each stdin request is JSON, bounded to 64 KiB. Paths are guest paths. Key
generation exclusively creates both files, uses host entropy inside the
module and returns only a public fingerprint:

```json
{"action":"keygen","private_key":"/home/id_ed25519","public_key":"/home/id_ed25519.pub"}
```

The host creates new guest files with Unix mode 0600; WASI preview1 has no
chmod call. Existing files keep their permissions. The private key is never
placed on stdout or sent to a signing service. The first version supports
unencrypted Ed25519 user keys. Host keys must be independently obtained and
pinned as exact OpenSSH public-key text; an unknown, different or certified
host key is refused.

```json
{
  "action":"exec",
  "connection":{
    "host":"127.0.0.1","port":2222,"user":"example",
    "private_key":"/home/id_ed25519",
    "host_public_key":"ssh-ed25519 REPLACE_WITH_VERIFIED_PUBLIC_KEY"
  },
  "command":"printf hello"
}
```

```sh
shenko tool exec --tool ssh --read-only --network 127.0.0.1:2222 \
  --timeout-secs 60 --stdin-file request.json
```

`wasi.exec:2` exposes the same explicit grant through its `network` field:

```json
{"host":"127.0.0.1","port":2222,"resolve":false,"operations":[{"operation":"connect"}]}
```

Put that target in the `network` array alongside `tool`, `stdin_b64` and
`timeout_secs`. A package requirement does not create a grant. Empty grants
open no sockets; network descriptors cannot be forged through `env`. Native
execution freezes the grant before T1 and opens connections only afterward.
A read-only file profile still permits remote effects when network is
granted, so their effect status remains unknown and they are never blindly
reissued after interruption.

File transfer actions share `connection`. `put` takes `local`, `remote` and
an explicit `overwrite` boolean; false uses SFTP exclusive creation. `get`
takes `remote`, `local` and `max_bytes`, and exclusively creates the local
file. Each transfer uses actual SFTP OPEN/READ/WRITE/CLOSE packets and emits
the observed byte count. Failed transfers may leave partial files and never
claim success. There is no automatic transfer or command retry. A remote
command that closes without an exit status reports an unknown outcome.

## Verification

CI and Nightly build this artifact and provision OpenSSH before running this
required journey as part of the normal workspace suite. To run it locally,
install `sshd` and `ssh-keygen`, build the artifact, and use absolute paths:

```sh
SHENKO_SSH_WASI_ARTIFACT=/path/to/new-ssh-build/shenko-ssh-wasi.wasm \
SHENKO_SSH_WASI_EVIDENCE=/path/to/new-evidence.json \
cargo test -p shenko --test ssh_wasi_client --locked \
  -- --nocapture
```

The existing single journey now lives in
`bin/shenko/tests/ssh_wasi_client.rs`, where Cargo supplies the real CLI
executable. It starts that CLI with an empty inherited environment (apart
from the command search path, locale and local-only Git configuration),
waits for its real health endpoint, and stops it through SIGINT. This uses
the product's no-model bootstrap to create the ledger, subject, node and
managed home; the fixture does not initialize those records itself.

The CLI then installs the actual SSH artifact under the name `ssh`. The
journey verifies its SHA-256 provenance, BLAKE3 identity, out-of-line CAS
storage, recovery-closure binding and activated environment, and reads the
selected tool back through `tool list`. For an out-of-line artifact,
`tools[].digest` identifies the unit's small pointer document, while
`tools[].artifact.digest` identifies the WASM module. The journey checks the
pointer's actual bytes and module reference, the installed module's BLAKE3,
SHA-256 and byte count, and the exact unit/version/environment member and
selector that name lookup resolves. It deletes its private copy of the
installation input before any client request. Every subsequent request
uses a new `tool exec --tool ssh` process against that same installation,
so execution must resolve the selected unit and reconstruct verified CAS
chunks. A final read verifies that ledger, subject and branch identities
still match the bootstrap.

All original protocol assertions remain: the module generates the client's
key, authenticates to a loopback-only OpenSSH server, preserves independent
stdout/stderr and exit 7, round-trips 100,003 binary bytes with SFTP, and
refuses a wrong host key and an absent network grant. The server host key
is fixture setup; it does not perform client authentication. The network
grant is explicit for each action; installing a package never grants it.

Each guest request has a 120-second WASI budget, including cold compilation
with fuel and epoch instrumentation, and a 180-second CLI process deadline
that also covers startup and ABI inspection. Short runtime deadline behavior
is covered separately by the runtime tests; this journey checks SSH behavior.
Every owned command has a deadline and a bounded stop/join path. Public
CLI, guest and server logs, including failures, are copied beside the
evidence JSON under its `.diagnostics` path. Private keys, stdin requests,
owner-control files and the installation tree are never copied there.
Temporary private keys are removed after process cleanup; a cleanup failure
retains its private temporary tree and records the unresolved process.

Missing inputs fail the journey. Compilation alone is not evidence that
this installation path ran; an earlier direct-module result remains scoped
to that earlier path and candidate. This journey does not prove native
`wasi.exec` T1/E1/T2, a remote-device deployment, mobile, Node/Bun, or
long-running acceptance.

Candidate `6782627ea89e17774bc65ba54d459ad85804fef1` passed the earlier direct-module
journey in [CI job 133289317901](https://github.com/cyjin-yl/shenko/actions/runs/43864453626/job/133289317901)
in 176.32 seconds. The [retained observation](../../../../docs/evidence/ssh-wasi-ci-6782627-20261116/observation.json)
binds its 2,552,689-byte module, public client/server receipts and all 97 source
input hashes. The original build receipt's global dirty flag remains true;
each recorded input independently matches the candidate's Git blob. The
workspace job still failed on an unrelated broker test assertion, and this
module journey does not prove the CLI installation path or a real remote
deployment.

Candidate `bb27f6542e3c37c693a973b5cc26ca8d2ec8fccf` reached the actual CLI
bootstrap, installation and inventory in
[CI run 43872078834](https://github.com/cyjin-yl/shenko/actions/runs/43872078834),
then failed because the journey compared the unit pointer digest with the
module digest. The original `rust-evidence` artifact `14227924196` retains
`ssh-wasi/openssh-journey.diagnostics/install.stdout` and `inventory.stdout`;
its ZIP SHA-256 is
`74055c8b74bb5e0fce2c18f80d3a5d934690abcb6600c7d1506d496802659b02`.
Those receipts record the activated 2,552,689-byte module and both distinct
identities. This failure occurred before the first SSH request, so that run
does not prove the installed-tool protocol journey. The follow-up assertion
repair keeps the original protocol checks and still requires a complete run.
