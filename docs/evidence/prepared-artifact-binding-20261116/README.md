# Prepared artifact identity source follow-up

This source increment starts from
`68de5ffcbfe6f3e31976cddd1837e37fbc16eb6d`. It follows the
[build-output receipt increment](../s2-build-receipt-20261116/README.md).

## Confirmed defect and repair

`UpdateService::verified_candidate_program` previously compared the current
executable with the current slot manifest. Both could change after preparation,
leaving the old saved smoke report and build receipt to qualify different bytes.
The original recovery closure already contained the prepared `to_artifact`
path and digest, but this admission check did not read them.

The check now verifies the recovery closure and target state-copy CAS references
by digest and byte length and reads their existing schema-1 fields. The sealed
slot, database and generation must match this activation and installation layout.
The current manifest and actual program must match the sealed prepared program.
Both existing final-forward-helper call sites retain the check: before that
path's subject/participant quiescence and immediately before selector CAS.
An incomplete historical binding is
an explicit error, never filled from the mutable manifest.

The saved smoke report's schema and check policy are unchanged in this follow-up.
It does not independently qualify candidate behavior, freeze source inputs or
attest a compiler/dependency closure. A same-user actor can still race filesystem
access after a read; no executable-descriptor or hostile-host isolation claim is
made. Existing state machines, native T1/E1 identities and Unknown outcomes are
unchanged.

## Caller and direction review

- Both callers of `verified_candidate_program` are the forward `activate_inner`
  path. They use the original activation's `to_artifact` and `to_state_ref`.
- The earlier live `quiesce_for_process_handoff` path serves both activate and
  rollback and does not run this predicate. A live caller has already retired
  before the final helper's check. This patch prevents selecting the replacement
  artifact; it does not claim refusal before that earlier retirement.
- Rollback runs the retained previous executable against the imported previous
  copy and creates a new activation. Its recovery object still retains the
  original forward `to_artifact`; this forward-only predicate is not applied to
  that opposite direction. This patch does not claim a new rollback binding.
- Recovery after a committed selection reads the existing selector, route,
  original process-handoff request and launch receipts. It does not reconstruct
  a preparation from today's manifest or rerun the build/smoke.

## Finite verification boundary

One new risk window uses the existing real Store and bounded executable smoke
fixture: prepare B, retain its CAS records, reopen, replace the actual program
with C and rewrite the manifest to C's real digest, then require activation to
refuse before any quiesce request. The selector, subject head, activation record,
original smoke and original recovery closure must remain unchanged. This is a
mechanism fixture, not an actual coding-seat or self-modified Shenko acceptance.

Three existing forward-admission fixtures now seal the actual original program
binding; all of their previous assertions remain. Other old fixtures retain their
original failed/missing-smoke or already-committed recovery scope. No additional
failure matrix is introduced. The previous eight-window build increment plus
this one window total **nine**, not a completion denominator.

No local Cargo, rustc or rustfmt is available. Rust compilation and execution
remain pending candidate CI; no workflow, model, service message or remote ref
was triggered. The accompanying static record covers only source integrity and
whitespace checks. The known unavailable documentation-parser environment was
not retried.
