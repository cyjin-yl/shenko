# Bounded local Node-WASIX source builds

The developer entry is `scripts/build_node_wasix_journey.py`. It uses the
unchanged isolated `scripts/real_node_wasix_journey.py` for owned processes,
resource admission, network denial and the original three cells. The existing
`node-wasix-source-build.yml` workflow still creates a fresh hosted build.

Keep the local work directory and every attempt's separate evidence directory
on persistent project storage. Run inside the project development container,
through the current `agent-heavy` foreground guardian and its actual admission
policy. Prerequisites remain Linux x86_64, Python with `tarfile.data_filter`
and `hashlib.file_digest`, libseccomp, Git, GNU Make, GCC/G++, GNU ar, visible
cgroup v2 ancestors and the original resource headroom. Keep the same container
and host compiler installation across attempts. The receipt binds host tool
executable bytes; it is not a reproducible image of host shared libraries,
headers or the operating system.

From the reviewed checkout, the inner commands are:

```sh
python3 -B scripts/build_node_wasix_journey.py \
  --source-revision "$SHENKO_NODE_WASIX_SOURCE_REVISION" \
  --work-dir /tank/shared/projects/shenko/build/node-wasix-local-r1/work \
  --evidence-dir /tank/shared/projects/shenko/build/node-wasix-local-r1/attempt-1

python3 -B scripts/build_node_wasix_journey.py \
  --source-revision "$SHENKO_NODE_WASIX_SOURCE_REVISION" \
  --work-dir /tank/shared/projects/shenko/build/node-wasix-local-r1/work \
  --evidence-dir /tank/shared/projects/shenko/build/node-wasix-local-r1/attempt-2 \
  --resume-from /tank/shared/projects/shenko/build/node-wasix-local-r1/attempt-1
```

Set `SHENKO_NODE_WASIX_SOURCE_REVISION` to the exact reviewed checkout commit
that contains this builder. The second command is appropriate only after the
first attempt records `resume_available=true`. A further eligible stop produces a new
receipt; a later invocation names that latest attempt and a fresh evidence
directory. No driver edit, source checkout, configure, asset download, wrapper
installation, object replacement or feature change occurs on resume. GNU Make
continues its original `-j1 node` graph, including the final link and Binaryen
asyncify step. Each attempt retains the original 7,200-second source-stage
bound and all existing resource guards; there is no automatic retry loop.

A receipt is written only after a compile `wall_deadline` or
`observed_memory_reserve` stop with GNU Make reaped by handled SIGTERM and the
entire owned group confirmed absent, with no execution or cleanup errors.
Both reasons use the same owned stop/join path; the memory-reserve guard still
stops the current attempt at its existing threshold. The normal GNU Make
interrupted-target cleanup is required. A KILLed leader, unsettled group,
ordinary compiler failure, other guard reason or failed custody capture does
not grant resume authority. This is not abrupt-crash or power-loss recovery.
Continuation is a manually authorized new invocation with fresh shared-resource
admission and the same existing capacity checks; there is no automatic retry
or reduction of the reserve to keep a build running.

Before compilation, the builder binds the full source inputs, extracted
toolchains and installed wrappers. After an eligible interruption it verifies those
inputs stayed unchanged, then records the whole actual work tree, including
objects, dependency files, generated configuration, literal symlink targets,
modes, device/inode identities and Make timestamps. Resume verifies that same
work tree, all preceding evidence indexes, original attempt identities and host
tool executable hashes. Moving, copying or editing the build is rejected;
links escaping its custody are rejected. These local unsigned records assume
exclusive owned storage; they are not attestations against a hostile filesystem
or a writer able to replace the records themselves.

Successful Make completion still passes through the original tracked-source
diff, immutable input, new core-WASM output and old-artifact rejection checks.
The builder retains the actual output bytes and build record before running
the original Wasmer 7.5.0 journey. Its 1517 filesystem witness, same-process
1518 lexical state and conditional 1519 checkpoint reopen remain required.
The build record distinguishes a fresh output directory at the origin from
subsequent compile attempts. A resumable interruption remains a failed attempt.

Attempts recorded by an earlier builder with `resume_available=false` retain
that outcome. This policy extension cannot create missing custody after the
fact or admit an attempt made with another builder identity. Future attempts
must run this builder from their origin; only a custody receipt actually
sealed by that attempt can authorize resume.

The archived run 43968644282 has no resume receipt and no retained complete
work tree or object directory. Its partial source/configuration evidence cannot
seed this mechanism. Start one new bound local build; preserve the old run as
the original failed observation. The source remains fixed at Node-WASIX
`20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f` plus the two existing recorded
patches and the four existing hash-pinned build assets. No released or tracked
prebuilt `node.wasm` is substituted. This tooling does not install a Shenko
backend or establish R081, R2, Bun or whole-Issue acceptance.
