# Native managed-node journey / 原生受管节点旅程

## Purpose and limits / 用途与边界

`shenko-node` is an optional Rust 2024 installation-control adapter. It is not
linked into `bin/shenko`, not an ElectionProvider, not a replacement subject,
and not a model-facing tool. Its only handler is `node.blake3.v1`: <=64KiB bytes
to a deterministic digest, with no external effect. This deliberately makes
unknown computation transferable without pretending arbitrary effects are safe.

这是 C17 首条有限旅程，不是完整 A42/A47/A48/A49。SSH 使用已合入 C07 原生
OpenSSH/receipt API，不冒称 WASM。节点独立目录、身份和进程不等于物理两机容灾。
没有已核验模型 profile 时 capability 返回 `not_configured_or_probed` 和
`smart_node_evidence=false`，不以节点名赋予智能资格。

## Authority / 权限来源

The control CLI requires a private OS account/data directory or its already
authorized strict-host-key SSH channel. `register`, `join`, `create`, `transfer`
and `accept` are trusted installation/controller operations. Do not expose their
JSON directly to chat/model arguments or an unauthenticated network endpoint.
`from_node` must be supplied by that authenticated transport, never a peer's
unverified message. The eventual registry/IPC adapter must derive invocation
and source identity itself; it is not implemented here.

`Identity.origin_proof` references an **already verified** earlier self supplied
by provisioning. It does not discover trust or prove age by itself. Prepare
checks that this identity equals the actual controller store. First adoption
archives a virgin independent identity; an already joined/evolved branch is
rejected for explicit merge, never silently erased. No competing subject loop
or daemon is started. Provider settings, private keys, model weights and secret
bytes are absent from the node synchronization schema.

## Build and inspect / 构建与确认实际工件

Use a clean product commit and the shared build budget in the managed container.
For a standalone caller, the equivalent native command is

```sh
SHENKO_SOURCE_REVISION=$(git rev-parse HEAD) cargo build -p shenko-node
/path/to/shenko-node --manifest
```

Record the actual source SHA used by the build. `--manifest` binds the running
executable's blake3 digest, compile-time source, OS, architecture and schema 1.
If stripping/copying it, inspect the final executable again. Unbound source or
mismatched bytes/platform/schema cannot join. This is a builder provenance
contract, not remote attestation or a substitute for verifying the source tree.

## Local control wire / 本地控制协议

`shenko-node PRIVATE_DATA_DIR` reads one <=1MiB JSON request on stdin and returns
one JSON response after committed work. Existing unrelated data is rejected;
never use a live main-app directory. New counters on the wire are canonical
unsigned decimal strings. Node UUIDs, agent/branch UUIDs and task UUIDs are distinct.

- `initialize`: stable `node_id`, optional provisioning `identity`; reopen never
  creates or resets an identity. `join`: stable `operation_id`, `expected_node`,
  verified `identity` and actual `manifest`; payload mismatch is a conflict.
- `status`: optional join `operation_id`; returns the old immutable join receipt
  separately from a fresh `running_manifest`, observation time and capabilities.
- `register`: immutable `node_id`, `target_id`, public-trust/binding digest.
- `create`: stable `task_id`, registered `node_id`, bounded `input` byte array;
  returns the same ticket on duplicate input and rejects conflicting creation.
- `execute`: `ticket` and exact `input`; arm before compute, then persist one
  original result/event. Duplicate tickets query; an armed-without-result record
  stays unknown. `query`: original `task_id` and string `epoch`, no redispatch.
- `transfer`: `task_id`, expected string record revision/epoch and registered
  `to_node`; this pure contract alone allows explicit reassignment. It increments
  execution epoch and semantic/record revisions without changing TaskRecord owner.
- `accept`: authenticated `from_node` and original `receipt`. Import stable
  source/event/digest facts, but only the current ticket may create one terminal
  result. Late/stale results remain evidence, not new authority.
- `export`: string `after` cursor and `limit` 1..128; native-journey raw events,
  independent of any LLM. This is not the complete main-app history exporter.

Objects are bounded and stored in the **same SQLite transaction** as references,
so this adapter has no separate filesystem CAS publication window. Assignments,
original work receipts, source events and conflict observations are retained.
The experimental profile uses DELETE journal and read-back FULL/foreign_keys;
it claims process-crash continuity only. It does not claim the SPEC default
WAL/power-loss profile or upgrade the baseline SQLite 3.46.0 WAL guarantees.

## SSH deployment / 部署与断线恢复

`scripts/managed_node.py` consumes an **already persisted trusted C07 parent
TaskInvocation**, not a fabricated production admission. Its config names
`artifact`, `controller_binary`, `controller_data`, `node_id`, `remote_root`,
`identity`, `ssh_driver`, and `parent` (the existing `task_ssh` JSON including
target/receipt-root/context). Key **paths/handles** stay in the local private
plan, never put credentials or these live configs in the repository.

```sh
python3 -I scripts/managed_node.py prepare NEW_PRIVATE_PLAN CONFIG_JSON
python3 -I scripts/managed_node.py deploy PRIVATE_PLAN
python3 -I scripts/managed_node.py query PRIVATE_PLAN install
python3 -I scripts/managed_node.py call PRIVATE_PLAN STABLE_LABEL REQUEST_JSON
```

Prepare freezes artifact chunks and a deployment UUID and registers the target
before SSH. Each upload/install/join stage has a stable derived operation ID and
logical key. Installation verifies compressed artifact SHA256 and private file
creation. A retry queries existing C07 operations; changed inputs conflict.
Transport loss means unknown, not failure/permission to install again. Query the
original plan/label; if its receipt remains unknown, retain that responsibility
rather than generate another plan to bypass uncertainty. A fresh deliberate
read-only status uses its own observation operation, not an old cached reply.

## Reproduce the two journeys / 两条真实隔离组合场景

Build the existing `task_ssh` example, then run

```sh
python3 -I crates/shenko-node/tests/managed_journey.py \
  REPO NODE_BINARY SSH_DRIVER SSHD LIB_DIR NEW_PRIVATE_ROOT
```

The fixture requires a native Linux container, root account, OpenSSH server/keygen
and Python3. It creates only a new loopback sshd/proxy with generated test keys;
its temporary authorized-key fixture uses `StrictModes no`. This does not alter
production sshd configuration. The test-only C07 `fixture-init` parent and
identity anchor are synthetic and explicitly not main-app integration.

1. Upload the real artifact through C07, actually close SSH after its installation
   receipt exists, query that original operation, resume adoption/capability
   readback and repeat deployment without another install attempt or identity.
2. Query the original durable task result, transfer the pure assignment, reject
   old-owner settlement, execute on the new owner and accept once. Preserve both
   raw sources after separate process reopen; never claim old work was stopped.

No physical machine, actual model inference, election, partition quorum,
production child scheduler, arbitrary resource fencing, image resurrection or
long-run acceptance follows from these two scenarios. See the machine-readable
[evidence](../evidence/c17-native-first-journey-20261025.json) and [C17](../tasks/C17.md)
for exact candidate and remaining obligations.
