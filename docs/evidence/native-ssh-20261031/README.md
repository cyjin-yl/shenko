# Native SSH adapter review checkpoint — 2026-10-31

Status: **static candidate, not compiled or run; dependency lock unresolved**.
This checkpoint stores an applicable source patch without activating a new Cargo
dependency against a knowingly stale lockfile. The executable tree remains the
`2b91e0a0adb5714a1115f9b5f82a8855709b2e66` version; its 53 passing targeted tests
do not validate this patch.

- [Source patch](native-russh-static-candidate.patch)
- [Exact file hashes, dependency intent and preparation failures](native-russh-static-candidate.json)
- [Upper-layer reuse comparison and upstream source evidence](https://github.com/cyjin-yl/shenko/issues/12#issuecomment-6550432491)
- [Actual preceding 53-test result](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6550361285)

## What is actually written

Seven candidate files replace the unused `russh 0.47/russh-keys 0.47`
declarations with exact `russh =0.65.0` (`ring,rsa`, no default features) and add
one native product module. This is not a new generic SSH SDK. Protocol, crypto,
keys, signing and channels remain russh implementations.

The trusted native caller supplies an owned, already-authorized byte stream and
an explicit key or russh Signer. No process, TCP socket, HOME config, key file or
agent socket is discovered by this adapter. It consumes the exact registered
public-trust CAS bytes, verifies their digest/length and parses a deliberately
small fail-closed raw-public-key pin / exact-key `@revoked` subset. Certificates,
CA markers, wildcard/hashed/negated host expressions are unsupported, not silently
trusted or reduced to raw keys.

All seven known inbound server-channel hooks explicitly reject before acceptance;
unknown channel types also reject. No forwarding is enabled. Input and output
pumps are independent, channel buffers/windows and retained stdout/stderr are
bounded, truncated output continues to drain, and a total command deadline is
separate from bounded local shutdown observation. Native exit 255 is a real
remote exit status, not relabelled legacy OpenSSH process exit 255.

An owned-stream close guard also covers cancellation during KEX. A single
nonblocking stream poll is serialized with closing; Handle Drop alone is not
treated as session shutdown. Session-end observation is **not** a claim that the
remote command stopped, and is not permission to replay an unknown operation.

## Review tests written, all UNRUN

Seven independent, finite scenarios use deterministic public test-only Ed25519
seeds, the real russh client/server protocol, and bounded Tokio duplex streams:
1. signed-key authentication, simultaneous stdin/stdout beyond the SSH window,
   stderr and unambiguous remote exit 255;
2. wrong public host pin rejected before authentication;
3. an exact-key revocation overriding a matching pin;
4. rejection of an unsolicited server session while a normal outbound command
   remains usable;
5. deadline preserving unknown remote outcome while observing local session end;
6. output retention cap while still draining to exit status/channel close;
7. cancellation during KEX closing the owned stream.

Only one hostile channel kind is exercised by the candidate runtime test; the
other hooks have been source-inspected, not runtime-proven. Signer/hardware-agent
integration, encrypted/certificate identity handling and supported-platform
coverage remain untested. There is no real SSH host, daemon, shell execution,
model, account, or production credential in these fixtures.

## Why this seam, and what it does not complete

The reviewed higher-level wrappers already solve useful cases; for simple
explicit pin + memory key they are valid reuse options. Shenko needs the
pre-authentication stream/signer/Handler boundary and full shutdown ownership,
so a small product adapter over fixed russh avoids a wider maintained wrapper
fork. See the source-bound comparison rather than assuming all wrappers unsafe.

This patch does **not** replace current `SshExecutor`, install a production
handler, or migrate `SshTaskRunner`/tmux/node_sync. It does not change #114, #91
or #130 owner paths. Existing transport-independent installed-binding facts,
generation/owner/ancestor fences, T1-before-effect and original-operation recovery
must be reused by that integration. `PolicyObserved` remains not_granted /
not_dispatched. Production policy/resource admission, mapping the explicit signer
identity to its installed local selection, and binding the stream endpoint to
the admitted target remain real missing integration work.

The existing CAS read allocates the complete object before this adapter's length
check; that storage allocation policy is not repaired here. An in-process SSH
library is not a filesystem/network sandbox. The legacy host SSH path and the
separately owned node synchronization path remain present.

## Dependency preparation and next executable checkpoint

Reviewed upstream: russh tag v0.65.0 =
`183a22e8f8f490fa09b6eb7bd044ef99d8c4414f`, Apache-2.0; upstream MSRV 1.89,
not a claim that Shenko's old workspace 1.85 metadata suffices.
Fixed upstream declares ssh-key =0.7.0-rc.11 and RSA =0.10.0-rc.18.
The complete resolved transitive set/checksums/MSRV/license closure is still
unavailable and must be recorded before any new-source execution.

Preparation with the existing fixed Cargo performed metadata resolution only:
- Offline: exit 101, russh absent from the cached registry index.
- Escalated tool invocation: environment filesystem setup failed before Cargo
  started; no online log was created.
- Same Cargo and crates.io route in the ordinary sandbox: exit 101,
  index.crates.io DNS resolution failed.

No compilation, package execution, mirror switch, real-host call or source
workaround was performed. The original lock remains
`7dabe6834599cf5b194bbdc0b726cd2b62ffe96046163e0281f5fbb4f6df5699`.
Do not apply the patch and label `--locked` ready.

Next: restore ordinary metadata access; resolve the smallest new lock delta,
check archives/licenses and MSRV, apply this exact patch on the retained base,
freeze a complete source tree and lock, then run only the specifically authorized
in-memory native scenarios and relevant prior regressions. Failures must remain
evidence, not be hidden by source edits. Production migration then replaces the
old remote command transport rather than preserving it as a silent fallback.
#12 and #127 remain open; this checkpoint is not completion of C07.

