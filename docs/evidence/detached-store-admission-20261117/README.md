# Detached data copies retain physical exclusion

This is a scoped continuation of PR #327, not complete S1–S5 delivery.

## Source and Why

Reviewed product source: `8978816295429b65308e6dd6c8499cca360f8552`, tree
`fd05ecc5a372e8e13c61e5018e0e67eaa3558b00`, descending normally from
`730c17ea6ea1098568e75bf67a81f00bf2f7bd17`. Both original local commits
`801d6ba37a99a305912345c89705e1738b353e91` and `8978816295429b65308e6dd6c8499cca360f8552`
remain in the ancestry with their original authors and timestamps. The first
intermediate approach is superseded by the second; it is not a passing candidate.

The parent CI 44070589009 failed. Its original rollback-migration fixture uses
real SQLite backups under `shared`, `gen-a` and `gen-b`, sharing `cas`, without
publishing a runtime selector. Treating every such copy as a retired software
slot rejects the fixture's first ordinary memory write. Dropping the physical
permit for these copies would instead make shared-CAS exclusion unsound.

The final source retains that permit and separates it from selected software
admission. A detached copy receives ordinary data admission only while the
physical root is unclaimed and the database has no runtime installation bindings.
Both conditions are rechecked under the original permit on every mutation.
A copied runtime or an old handle after installation creation cannot acquire
fresh authority through a directory change. The actual database path is resolved
before classifying the slot. Existing selector/head checks and late factual
receipt admission remain intact. See the [current contract](../../specs/self-update.md).

## Bounded verification scope

The original two rollback-migration journeys and their assertions are unchanged.
One new test checks real lock exclusion and refusal by an already-open data
handle after a root installation appears. The existing activation journey gains
two windows: a real runtime backup moved under an unclaimed layout, and a
symlink alias of the retired database. This is three distinct added windows,
not a new acceptance matrix. No source-policy, lint baseline, timeout, ignore,
original fault injection or original assertion is relaxed.

## Actual observations, including failures

- The inspected checkout was clean at the reviewed source, and `git diff --check`
  against the parent succeeded. Initial before/after observed input hashes match;
  this is not an atomic source freeze.
- The previous development-container driver failed during `git status` with a
  90-second timeout, before Rust compilation or tests. Its original logs and both
  driver versions are retained under `previous-driver`. The prepared r2 driver
  has not run; its presence is not evidence.
- The first host document checks failed with missing declared parsers. Their
  full JSON outputs, not just an error count, are retained under `initial-static`.
  Rustfmt for the installed 1.98.1 toolchain was absent; its failure is retained.
- The declared parser dependencies were then installed in a private project
  virtual environment. `declared-parser-attempt` preserves the actual dependency
  list and empty checker streams. That observer exited with a JSONDecodeError;
  it did not persist the checker's exit status or execute migration verification.
  Neither document check is claimed passing from that attempt.
- Desktop Commander's current device policy refused the `sudo` container entry.
  No privilege workaround or policy change was attempted. Rust execution is left
  to the repository's existing pinned-container CI on this isolated PR.

All copied files retain their original bytes. `inventory.json` binds retained
files with SHA-256. Host paths identify this observation's environment, not a
portable installation recipe. No secret or authentication material is included.

## Remaining obligations

This slice does not fix the parent's canonical replay, CLI restart, real SSH
component or MCP failures, nor all parent Clippy increments. A green stacked
ratchet must not be used to erase those parent failures. PR #328's selected-first
retirement postcondition remains a separate integration obligation; this change
does not touch `handoff.rs` or overwrite that contributor's work.

Arbitrary filenames/hard links, hostile filesystem replacement, all CAS writers,
and installation-wide continuous writer fencing remain outside this proof.
Real reference-model behavior, NapCat to actual coding seat to qualified candidate
and continued runtime, services/accessibility, named Node/Bun/device support,
physical faults and candidate-bound 7/30-day observations remain unverified by
this increment. The full [completion contract](../../governance/COMPLETION.md)
remains active.

Source inspection and evidence preservation: ChatGPT. No independent worker or
runtime pass is inferred from this review.
