# PTY write/flush source review and original static observations

The integrated source is the five-commit chain ending at `4229b45b8ab33426986540a9a83d22a41a0c9948`, with production Rust fixed at `e53c1ae6d31fdfa0451e45caa9367ff0c24a4c69`. It implements [the local PTY write/flush contract](../../development/pty-write-flush-receipts.md). Its five new protocol windows have **not been compiled or executed** in this local environment.

Independent source review found two fixture defects. The original first window could close before the reverse-direction output had arrived. The correction preserves 128 KiB and the original assertions, observing the actual output tail on the original screen before continuing. Failed setup/cleanup also needed the original server stream to close before awaiting the actual SSH session and its workers; aborting an outer observer alone was not a session join. The final code retains setup failure, observer failure, timeout and actual session completion as distinct observations. These corrections add no sixth window and do not change the production checkpoint.

## What was actually checked

The [final static record](pty-write-static-final.json) binds the exact source head, tree and file hashes. Its original stdout/stderr files are retained alongside it.

| Check | Actual result |
| --- | --- |
| Production range `5309c6a0..e53c1ae6` whitespace check | Exit 0 |
| Complete source range `5309c6a0..4229b45b` whitespace check | Exit 2: three blank context lines in the original unified patch |
| Vendored incremental patch reverse application check | Exit 0 |
| Earlier local documentation/migration attempt | Exit 1: 1,948 unavailable Rust/CSS parser errors |
| Rust, Cargo, rustfmt, new SSH journey | Not run; tools unavailable locally |

The unified patch's original bytes remain intact. The [initial static record](pty-write-static.json) used a working-tree whitespace check, not the complete committed range; it is retained unchanged with that scope limitation. The original [documentation output](pty-write-docs-check.json) and stderr are also unchanged and do not become a final documentation pass. No new run was performed to replace those observations.

All existing native test functions and the earlier nine admission/outcome windows were preserved by the source branch. The integration also retains the separate fixture-macro correction from the actual 10d4 Clippy failure; that zero-scenario correction is not attributed to this PTY branch.

The [original commit/tree records](../source-checkpoints-20261116/pty-checkpoints.json) preserve the original authors, intermediate flawed fixtures and both corrections. The [archive index](archive-index.json) binds the exact retained static files. No test is reported as passing merely because its source was reviewed. Actual remote OS PTY, coding-seat operation, installation-wide writer exclusion and complete S2 remain separate obligations.
