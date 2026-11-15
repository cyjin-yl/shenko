# Optional external history research

**Position.** `history.obelisk` is a reference plugin over the existing Node
installation authority, Cordis lifecycle and native task registry. It adds
`history.external.query/1`; the current-branch `history.query/1` remains the
same capability. The plugin is absent from the subject's available tools
until its Node unit is explicitly enabled.

## Why

| Element | Contract |
| --- | --- |
| Intent | Answer questions about previous external harness work with evidence that still exists after the original log rotates or its host goes offline. |
| Premise | An index hit, a summary, a model answer and the original observation are different things. Upstream indexes may truncate bodies and reopen mutable local paths. |
| Observable result | A selected full original has an immutable source version, record identities, authenticated byte pages and source-bound search hits. Queries state their coverage and unresolved freshness. |
| Flexible boundary | Obelisk is an independently installed query program. The index and structural source view are rebuildable; Shenko's raw facts, CAS and ordinary cognition replication carry the evidence. |
| Failure | Missing, changing, oversized, corrupt, retired or unscanned sources remain explicit. CLI failure or a lost process outcome cannot become an empty successful research result. |
| Sources | User request of 2026-11-15, [tracking issue #300](https://github.com/cyjin-yl/shenko/issues/300), [Obelisk at the audited revision](https://github.com/tommy0103/obelisk/tree/cc75c2dd1aead4f4374758a7536385307934e177), [memory](memory.md), [durable state](durable-state.md), [plugins](plugins.md). |

The motivation is stronger raw-evidence retrieval, not an assertion that
research accuracy is solved. The cited [Raw-RAG paper](https://arxiv.org/abs/2610.30755)
reports answer accuracy across selected evaluation settings, not universal
recall. [Memory Tax](https://arxiv.org/abs/2610.34073) compares against an
additional retrieval round and has exceptions. The reported 38% token
reduction in [Context as Infrastructure](https://arxiv.org/abs/2610.41172)
belongs to that paper's evaluation; it is not an Obelisk or Shenko measurement.
No such performance number is an acceptance criterion already achieved here.

## Installation and ownership

`shenko history obelisk configure --file <config.json>` validates and saves
node-local source/executable configuration and records an ordinary disabled
Node plugin unit if it does not already exist. Existing enable state is
preserved. `enable`, `disable` and `status` use the same Node authority as
other optional adapters. No separate registry, credentials flow, package
installer or memory-approval layer is introduced.

The running subject and `shenko capabilities` use the same installation
observer. Enable publishes the native definition; disable withdraws it and
cancels/joins its owned workers. A missing local CLI does not prevent a node
from enabling the plugin to inspect already replicated archives. Configuration
is loaded per request; modifying it does not rewrite an already frozen attempt.
The CLI `query` and `archive` commands are explicit local diagnostics/imports;
subject use goes through the durable native task path.

The compatibility target is the independent upstream **source** CLI at
`cc75c2dd1aead4f4374758a7536385307934e177`, run with an explicit Node executable.
The npm `0.3.0` distribution does not provide the same independent-home and
index schema contract. Its identical version label is insufficient. A bundled
manifest authenticates the audited upstream source files before starting a
query; incompatible or modified source is refused before index preparation.
Shenko does not bundle or install Obelisk. Upgrading this contract requires
reviewing the upstream delta and rerunning the compatibility scenario.

Example configuration (replace every path with the actual absolute path):

```json
{
  "executable": "/opt/node/bin/node",
  "source_checkout": "/opt/obelisk",
  "arguments": ["/opt/obelisk/packages/cli/src/obelisk.ts"],
  "index_home": "/srv/shenko-local/obelisk-index",
  "source_instance": "owner-work-history",
  "sources": [
    {"provider": "codex", "root_label": "primary-codex", "path": "/home/owner/.codex"},
    {"provider": "pi", "root_label": "primary-pi", "path": "/home/owner/.pi/agent/sessions"}
  ],
  "limits": {"timeout_ms": 30000, "stdout_bytes": 1048576, "stderr_bytes": 65536, "drain_timeout_ms": 2000}
}
```

This increment supports explicit **Codex, OMP and Pi** roots. A configured
provider has one root per index. Other upstream provider adapters are not
silently advertised as Shenko-verified integrations. The dedicated index
disables implicit roots; existing different settings are refused. Live SQLite
and WAL files are not cognition replication artifacts.

## Evidence identity and capture

A source is `{provider, source_instance, source_key, session_id?}`. The first
three fields are its portable logical identity. A native session identifier
is preferred for `source_key`; when unavailable, a stable root label and
relative path are explicitly a locator-based identity. Current working
directory, absolute path, wall clock and SQLite rowid are not global IDs.

Archiving reads the complete selected file twice through the same descriptor,
checks size, modification observation and hash, then seals its bytes. A
changing source is refused rather than recording a prefix as complete. Native
imports default to a 64 MiB maximum and never truncate an over-budget source.
Larger histories need an explicitly identified stable source partition; no
automatic partitioning is claimed. This observes one file version, not an
atomic snapshot of every harness file.

Source-version identity binds the subject, logical source and full raw digest.
Record identity adds source position. Equal text at different positions stays
two events; byte deduplication never asserts event equality. Old versions and
their source metadata remain visible. Source timestamps order observations
only when meaningful; no wall-clock last-writer-wins truth rule is applied.

The archive uses the existing `raw_sessions`/`raw_session_events` fact classes
and CAS. The Store atomically admits a complete session snapshot, checks
immutable replays and object tombstones, and records deterministic unknown
event creation time where the source has none. The import's real observation
time belongs to its E1 receipt. This avoids a fabricated conflict when two
nodes independently capture identical source facts.

Projection keys such as role, native ID, timestamp, parent, branch and tool
call ID are mechanical aids. Original JSON/JSONL bytes remain authoritative.
Malformed or opaque material is preserved with parse status; its metadata is
not silently invented. Nothing is admitted to the subject inbox as a new user
instruction, and no imported role grants present authority.

## Query contract

The subject delegates `[delegate:history.external.query:1]` with one JSON
object. All source lookups are restricted to that task's subject archive.

| `op` | Parameters | Result and limits |
| --- | --- | --- |
| `overview` | `after_session?`, `limit?` | Bounded source/version distribution, object availability, retirement and freshness status; a continuation means coverage is incomplete. |
| `search` | `query`, `source?`, `cursor?`, `limit?` | Literal full-text search, including CJK and content beyond upstream text prefixes. Match-centered bounded previews retain source digest, record and byte span; scan budgets return a continuation even for an empty page. |
| `read` | `event_id`, `offset_chars?`, `limit_chars?`, `include_retired?` | An authenticated record projection page; defaults to 4096 Unicode scalar values, maximum 16384. Original source bytes remain available separately. |
| `original` | `archive_session_id`, `offset?`, `limit?`, `include_retired?` | Base64 byte page with authenticated chunk hashes, original digest, total size and next offset; native page maximum 65536 bytes. |
| `archive` | `source`, `file`, `max_bytes?` | Complete selected file version, subject to configured provider roots and matching source instance. Returns source version and full raw CAS reference. |
| `obelisk` | `request` | Explicit independent CLI execution: `manifest`, `sessions`, `search`, `messages`, `raw_page` or composable `script`. Upstream hit coverage and local source availability remain separate from archival completeness. |

Example:

```json
{"op":"obelisk","request":{"op":"manifest","limit":20}}
```

Select a returned source manifest, then archive it using its portable identity
and local path. The explicit capture step lets the subject choose a useful
source and budget; index hits alone do not attest that all originals have been
archived. Source manifests preserve identity basis, upstream index frontier
when known, file availability and bounded metadata. A large opaque upstream
cursor is described by size/digest instead of filling the result budget.

```json
{"op":"search","query":"权限认证","limit":10}
```

Searches retain distinct historical versions. Repeated versions of the same
logical event are not independent corroboration. `freshness=not_revalidated`
does not mean current; missing CAS bytes differ from no hits. Raw original
offsets are **bytes**, record projection offsets are **Unicode scalar values**,
and upstream `raw_page` offsets are **UTF-16 code units**.

## Execution, cancellation and recovery

The handler freezes input and configuration digest, arms T1, then executes
the selected archive/query operation and records its observed E1. Native
dispatch owns T2. Receipts retain a `learning_source` compatible with existing
`cognition.unit.write`; the plugin does not write semantic conclusions or
policy on the subject's behalf.

External execution uses argument vectors and a temporary query file, never
shell concatenation. An outer deadline, independent stdout/stderr budgets and
joined readers bound the invocation. Cancellation signals owned processes,
waits for the child and drains or explicitly reports reader cleanup. Unix
process groups cannot attest that a deliberately detached descendant or an
already issued external effect stopped. Custom JavaScript has host authority;
Node's `vm` and read-only SQL helpers are not a security sandbox.

An observed successful query completes the query operation, not a research
claim. A missing, cancelled, malformed or lost process outcome remains
unknown when effects may have occurred. Recovery reads the original attempt
and E1 without executing the CLI again. Re-enabling or restarting does not
invent another attempt for a completed task. Generic distributed authority
fencing and control-log quorum work tracked separately in PR #299 is not
claimed to be solved by this plugin.

## Replication, retention and views

Ordinary cognition export/import carries raw session facts and their nested
CAS closure, including full originals and searchable projections. Receiving
nodes can inspect the archive without Obelisk or the original filesystem.
Source configuration, credentials and disposable indexes stay node-local.
Repeated imports are idempotent; conflicting facts are retained/rejected by
the existing immutable admission rules, never resolved by the current path
or local import timestamp.

Object tombstones suppress default search, reads and re-import. Explicit
`include_retired` is an audit read; it does not restore eligibility. The
existing `retention.apply` erase mode records a tombstone and does not promise
physical CAS erasure. This work preserves that boundary instead of claiming
deletion merely because a projection is hidden.

The source/version overview is a rebuildable structural view with bounded
coverage. A semantic claim-evidence graph, semantic embeddings, Nowledge Mem
integration, periodic synthesis and a measured token-saving advantage are
not part of this implementation. Authored research conclusions use existing
versioned cognition units with source references, correction and retirement.
An optional future view must show its source frontier, stale/partial/conflict
state and a path back to these originals before it can be relied upon.

## Verification and license provenance

The [delivery TODO and evidence record](../development/obelisk-history.md)
separates mechanisms, actual upstream CLI compatibility, model behavior and
unverified long-running deployment. A green deterministic test is not a
model-quality result.

Upstream Obelisk declares **AGPL-3.0-only**. This integration stores an audited
source compatibility manifest and independently authored query/bridge code;
it does not redistribute the upstream implementation or use the same process
as a linked library. Users obtain the separate upstream program and its
license from [its repository](https://github.com/tommy0103/obelisk). Any later
redistribution or upstream modification needs its own license review; this
technical separation is not a legal conclusion about every deployment.
