# Shenko source-pinned russh dependency

This is the complete published `russh 0.65.0` crate source, with a small local
read/write-progress change. It is **not an upstream release or upstream fix**.

- Upstream revision: `183a22e8f8f490fa09b6eb7bd044ef99d8c4414f`.
- Published crate SHA-256: `3bb77a4b259312b2abbc541bca1c13cb4e9b61c560a7fd894068c3b92e0c9dc3`.
- Source: <https://github.com/Eugeny/russh/tree/183a22e8f8f490fa09b6eb7bd044ef99d8c4414f>.
- Apache-2.0 license and original authorship are preserved in `LICENSE.txt` and
  all original source files. `Cargo.toml` is the complete upstream normalized
  manifest; `Cargo.toml.orig` is preserved unchanged. No example-only development
  dependency has been removed from this installed copy.
- The reviewable code/test delta against the published archive is
  `SHENKO-UPSTREAM-DELTA.patch`. The three production-file changes retain the
  reviewed production patch SHA-256
  `415c29b50ca302557a644bda46ccc516a7e77e56dd933d18e8333041afac277b`.

## Why this local change exists

Two original native transport test generations stalled with both endpoints using
russh 0.65.0 over a 1 KiB full-duplex memory stream. The fixed local candidate
allows pending wire reads and writes to make progress together, rather than
waiting for an ordinary write flush outside the read selection. It stores the
accepted ciphertext offset synchronously after each successful partial write so
canceling a pending flush does not replay the accepted prefix. No cipher, MAC,
packet-sequence, KEX, signature or key-verification primitive is changed.

Writer errors, `WriteZero`, flush errors and encoded-output admission overflow
terminate that writer/connection. The existing `Error::Pending` variant is used
for the new local encoded-buffer admission failure; it is **not permission to
retry the same damaged connection**. A new local 1 MiB limit bounds retained
encoded ciphertext including framing, padding and authentication tag. It is not
an upstream guarantee, a plaintext limit, a bound on temporary allocations or a
bound on total process memory. An application still has to drain its channels;
this change is not a general solution to every per-channel head-of-line block.

## Installation and validation boundaries

The Shenko workspace uses this source through `[patch.crates-io]`. This directory
is explicitly excluded from workspace membership: normal Shenko checks must not
implicitly enable upstream's default features or run third-party tests/examples.
The native adapter explicitly selects `ring,rsa`. Full upstream dev dependencies
and original TCP test sources remain present for a separately scoped standalone
upstream test invocation; their standalone dev-dependency lock is not asserted
by the Shenko root lock.

The prior local **validation-only** tree
`22a15aec844ca25a7498fef5e077b27c799148e1` passed 84 distinct selected tests in two
configurations: 79 with `ring,rsa`, then five original server rekey cases with
`ring,rsa,flate2`. It used a separate dev-dependency manifest and five explicit
TCP-to-memory fixture adaptations. Those adaptations are **not substituted for
the original upstream TCP tests in this installed copy**. Six focused local
pending-write unit controls remain under `cfg(test)`.

The installation source tree `f8b268618b97009fce9ed51d5ecf51504b68e2fc` has now
actually passed the original 62 Shenko tests (9 native transport, 26 selected SSH,
3 selection handler, 3 query handler, 2 startup, 19 node). The root lock contains
493 registry packages, with no new versions or normal/build/dev feature changes
for these six commands. All actual compiled-artifact target/feature/profile sets
match the previous run; this run reused verified build cache, rather than freshly
recompiling the world. See the [source-bound evidence](../../docs/evidence/native-russh-installed-dependency-20261101.json).

This published checkpoint adds only documentation/evidence to that tested tree.
It does not imply current-main integration, whole-workspace/upstream verification
or production authorization/child installation. The separate prior 84-case
validation tree is not relabeled as this tree. The original two 58-pass/4-fail
runs, fixture compile failure and zero-test filters remain in the evidence history.

- [79-case result and five gated cases](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6552531628)
- [Five cases actually executed after explicit feature approval](https://github.com/cyjin-yl/shenko/pull/127#issuecomment-6552585194)

No real SSH hosts, account credentials, production deployment, all-feature or
whole-upstream suite, current-main integration, cross-platform/WASM behavior, or
production policy/target/signer/endpoint/child installation is established by
these tests. The adapter's explicit trust and signer inputs do not themselves
supply a policy grant or host sandbox.
