# CI observations for 262f980e — 2026-11-16

This archive retains the original Rust and Clippy artifacts and complete Rust
and kernel job logs for [CI run 43937620098](https://github.com/cyjin-yl/shenko/actions/runs/43937620098).
The final run observation is `completed` / `success`; all six jobs report
`success`. The run started at `2026-11-16T19:16:33Z` and its final update is
`2026-11-16T19:30:23Z`. No workflow, test, or documentation check was rerun to
create this archive.

## Exact source and comparison

| Identity | Original value |
| --- | --- |
| Public PR head | `262f980e2db13dac7d12e7df861fbf0ba707999d` |
| Actual CI checkout | `aa58209c068ccce6ec3ed7f2441e8538989db34b` |
| Public head and checkout tree | `1740cd95768523613fd36d8da46c2ad91a7b60f2` |
| Independent Clippy baseline | `84ae19404f2a17d259362c99091db408b7262e14` |
| Run | `43937620098`, attempt 1 |

`actual-checkout.json` records the actual merge checkout, its tree and both
parents. `clippy-base.json` binds the independent baseline and the candidate;
`clippy-ratchet.json` retains the toolchain, exact flags, production and
all-target comparisons, and the empty `errors` list. Ratchet success means
the recorded comparisons passed; it does not mean the repository has no
historical lint diagnostics. Both files are byte-identical to their members
in the retained Clippy ZIP.

| Job | Job ID | Final conclusion |
| --- | --- | --- |
| kernel-boundary | `133546284642` | success |
| docs | `133546285228` | success |
| docs-migration | `133546285243` | success |
| clippy-ratchet | `133546285400` | success |
| web | `133546285438` | success |
| rust | `133546285546` | success |

The raw `run-final.json`, `jobs-final.json` and `artifacts-final.json` retain
the original returned metadata. The jobs response does not contain per-job
start/end timestamps; this archive does not invent them. The artifact list
also names kernel and web artifacts. Their ZIPs are not in this archive:
the complete kernel job log is retained, and the web conclusion is the
original job result.

## Observed journeys and their limits

The original `workspace-test.log` inside `rust-original.zip` contains 152
test summaries, totaling **1,079 passed, 0 failed, 11 ignored, 0 measured and
88 filtered out**. `workspace-counts.json` retains the prior derivation and
`archive-validation.json` records an independent recount of those original
lines. These are test-run counts, not a completion denominator, percentage,
or claim that ignored or filtered tests passed.

The recorded process journeys include:

| Original test | Result | Original duration |
| --- | --- | --- |
| `three_real_start_processes_agree_over_a_pinned_ssh_star` | passed | 86.90 s |
| `original_native_e1_arrives_over_authenticated_ssh_without_t2_or_reexecution` | passed | 27.99 s |
| `ssh_wasi_auth_exec_sftp_and_refusal_journey` | passed | 203.52 s |
| `public_start_pause_survives_process_reopen_and_resume_alone_admits_inference` | passed | 1.69 s |

The native E1 journey is the original nine-window test in this source,
not nine additional test functions. Its 27 exported files are retained
verbatim under `native-e1/` and checked against the ZIP. The original
`transport.json` binds the actual CLI and SSH executables and states the
boundary: one real pinned OpenSSH route, with Store control grants supplied
by the existing retained-value fixtures. This is not an additional quorum
or process-election acceptance claim. The original observation records two
physical effects, zero chosen T2, applied/replayed receipts, retained
reconciliation debt for the gap, and the open local unknown-effect case.
`cleanup.json` reports the original clean/joined outcome. This archive adds
no reconstructed receipts or successor delivery evidence.

The same-publication metadata also preserves three cancelled runs:
`43937619360`, `43937617032` and `43937616236`. They are not passes or
independent successful executions. `same-publication-runs.json` is the
earlier returned snapshot in which the retained run was still in progress;
`run-final.json` and `jobs-final.json` provide its final state. The earlier
snapshot has not been rewritten.

## Original artifacts and byte preservation

| Artifact | GitHub artifact ID | ZIP bytes | Members | SHA-256 |
| --- | --- | ---: | ---: | --- |
| `rust-original.zip` | `14262528295` | 8,344,922 | 1,868 | `0349bd440955cfd4368ba2f4a5b7c60bde2ac5dfbbd44e487c169f29af3708fa` |
| `clippy-original.zip` | `14263180135` | 2,735,317 | 18 | `a11d455bdf1d6b91b8f64e593dcd638e878bf1a318f659c3c38a7c69aec5c99c` |

Both original ZIPs are complete. The Rust ZIP contains the workspace check
and test logs and the original compat, component-process, SSH-WASI,
start-boundary and native-E1 artifact trees. The ZIPs, every member's CRC and
SHA-256, and the previously retained member manifests were independently
checked during archiving. The large extracted trees are not duplicated in
Git; they remain fully available in the original ZIPs.

The complete job logs are compressed losslessly:

- `rust-job-133546285546.log.gz`: 473,392 original bytes, 473,390 UTF-8
  characters, 5,796 LF bytes.
- `kernel-job-133546284642.log.gz`: 62,097 original bytes, 62,095 UTF-8
  characters, 591 LF bytes.

Each gzip file decompresses to the exact original bytes. No line endings
were normalized; the original standalone job logs have no CRLF pairs.
Original member line endings inside the ZIPs and the separately retained
native output files are also unchanged. No signed download URL is retained.

`file-index.json` lists all 41 retained original files, their byte sizes,
SHA-256 hashes and expected Git blob IDs. For compressed logs it also lists
the original uncompressed hashes and sizes. The original verification
records are retained as `rust-verification.json` and
`clippy-verification.json`. The archive's additional checks are separately
identified in `archive-validation.json`; `git-index-verification.json`
records the staged-blob byte checks, including files that ignore rules
might otherwise omit.

## Scope of the result

This run covers the source tree above, including its then-current native E1,
three-process component, SSH-WASI and public startup paths. It does not
cover the subsequent E1 export-retention, bounded-capture, managed-seat
lifecycle or Node command-length patches. The hosted model experiment is
a separate run; this CI result does not demonstrate new model behavior,
the 27B baseline, the complete S2 self-development loop, named Node/Bun
runtime availability, or the required 7-day/30-day physical fault journeys.
