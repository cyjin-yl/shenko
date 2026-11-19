# Native E1 selected-original retention checks — 2026-11-16

This records the evidence-export follow-up to `11e4bf400a2a0bd83ada58bb4689354e4b4fa59d`, whose parent is the actually tested first-E1 candidate `7905e8413b96015d96ea301bd1f3b6937dcfff90`. The retention code is a new candidate; the earlier nine-window SSH pass does not validate it.

The earlier CI artifact retained diagnostics and reference metadata, but not the original Store rows or CAS bytes in the deleted private fixture directories. The new fixture hook exports **selected original row values and a typed CAS closure**, before cleanup on both success and failure. It is not a full SQLite backup or a cross-Store atomic snapshot. No additional fact is sealed to fill a missing object. Node databases, SSH credentials/configuration, unselected tables, credential-bearing originals, and CAS namespaces outside the fixture allowlist are excluded explicitly.

This follow-up makes absent databases, missing required Store tables, and absent original commit rows fail the exporter while retaining available originals and a failure manifest. It also follows object/list JSON strings inside `Cell::Text` without rewriting original data, marking that decode step as `/<json>` in the reference route. The production fixture uses `private/{source,receiver,parked}/shenko.db` and `cas/<first-two>/<next-two>/<digest>`; these were checked against the fixture and `ContentStore::object_path`.

## Actual checks

One continuous Python runner executed two bounded exporter scenarios:

1. Selected originals: original row text, source commit columns and CAS bytes remain unchanged; a typed reference nested in an encoded row is retained; missing CAS is listed; credential-bearing row/object bytes and an unselected owner-token table are excluded.
2. Partial failure: one Store database is absent and another lacks the required commit table. The exporter exits 1, retains available source/receiver originals, records both failures, and does not create the absent database.

The runner exited **0**, including the expected failure-path exporter exit **1**. Full stdout/stderr, the two synthetic archives, and command/exit/archive-SHA observations are in `exporter-checks/`; top-level stdout/stderr are also retained. These fixtures use explicitly synthetic digest labels. They do not prove BLAKE3 validation, native Store transitions, SSH authentication, or model behavior.

Exact command:

```sh
python3 -B docs/evidence/native-e1-retention-20261116/verify_exporter.py --exporter bin/shenko/tests/support/native_e1_snapshot.py --output-dir docs/evidence/native-e1-retention-20261116/exporter-checks
```

Source binding at that check: parent `11e4bf400a2a0bd83ada58bb4689354e4b4fa59d`, exporter SHA256 `a2f20872ff438234842ef0f64e77b67ad9c6d6a68ebc21798215836ef510fea4`, runner SHA256 `66364ddb09b10a1765d4562d8b343cb267ec6157015b87f0c426eac6f944ed1d`. Neither file changed after execution. `git diff --check` passed. Rust/Cargo are absent in the restored environment; no Rust compilation or new real SSH journey is claimed. The existing nine-window assertions remain unchanged, pending the next actual CI run with this retention hook.
