# C19 directory-lock integration and actual local checks

This increment adopts the recorder changes from [PR #314](https://github.com/cyjin-yl/shenko/pull/314)
on the continuing integration branch. Its original commits are
`c69389f1f1106b9bb15f4157e7f1a0629806a270` and
`245a846ba0109a0916bad7a4a30ee7f66e2c2d01`, based on main
`84ae19404f2a17d259362c99091db408b7262e14`. The source PR and the alternative
[PR #313](https://github.com/cyjin-yl/shenko/pull/313) remain open and unchanged.

## Adopted behavior and choice

The complete read/check/append operation for a sample shares the persistent
Linux run-directory inode lock with initialization, checkpoint and stop.
Reports acquire a shared lock for a pure read and an exclusive lock when
writing a report. A long sampling loop releases the lock between samples.
The recorder checks that the directory still names the locked inode when
the lock is acquired; unsupported locking or substitution fails explicitly.

Both independently proposed implementations address local competing writers.
The directory-inode version is adopted here because it supports reporting a
read-only archive and does not create a second, replaceable lock-file inode.
The alternative file-lock implementation and its successful independent check
are retained as comparison evidence; its production code is not combined with
the directory protocol. Neither protocol governs remote hosts, old recorder
versions or writers that bypass it.

The original recorder, test and development-document blobs matched PR #314 at
integration. Only its added C19 check was merged into the current CI workflow;
the candidate's existing build/runtime gates remain unchanged. The following
small local correction adds a 15-second timeout to each existing test subprocess.
It retains both scenarios and every assertion, including the explicit late
sample after stop, and adds no new scenario.

## Actual observations

| Source actually consumed | Observation | Scope |
| --- | --- | --- |
| PR #313 `fa990555`, recorder `9d6f2a32`, test `c49afe66` | 20 scenarios passed at 20:56:43–20:56:45 UTC | Exact recorder/test blobs on an a1d3 context checkout; not a full source-PR checkout or its CI. |
| Earlier PR #314 `c69389f1`, recorder `fff3423d`, test `e2ede43c` | 17 scenarios passed at 20:56:43–20:56:44 UTC | Exact earlier recorder/test blobs; does not cover the later explicit late-sample assertion. |
| Integrated `169e18bd`, tree `fb6673a254a5e826e05da919a491c86276969b9b` | **17 scenarios passed**, exit 0, at 21:19:14–21:19:15 UTC | Exact adopted recorder plus the later source assertion and bounded subprocess correction, on Python 3.12.14. |

The integrated invocation was
`python -I scripts/observe_longrun_test.py`. It completed in 1.488 seconds,
with 54 stdout bytes and empty stderr; the outer 60-second timeout was not
reached. The 17 scenarios consist of 15 existing scenarios and the two original
PR #314 concurrency scenarios. Synthetic clocks/process shapes in the older
consumer scenarios remain synthetic; the concurrency scenarios use actual
independent local CLI processes.

`integrated-check/observation.json` binds the command, interpreter, source/tree,
consumed Git blobs, timestamps and stream hashes. `stdout.log` and `stderr.log`
retain the original streams. `prior-independent-comparison/` preserves both
earlier observations and their original input identities without rewriting
them to the selected candidate.

## Original source preservation

`source/original-gitdata.json` retains both public source commits and all four
changed source blob identities. The `.commit` files are exact raw Git commit
bytes independently reconstructed and checked against those original commit
IDs. Five `.blob` files preserve the unmodified original bytes, including both test revisions and the
source workflow before integration. `original-tree-deltas.json` records the
per-commit paths needed to reconstruct both original trees from their main base. The API change patch is retained losslessly
as `pr314-api-changes.patch.gz`.

`source-comparison-before-bounded-test-fix.json` records which blobs matched
exactly and why the merged workflow differs. The final executed test blob is
`cb03beffcc01d0a03bd8a4a626c1fdd24e7b89f1`, with SHA-256
`9a1947caa1b99fdc319d171c2abf10eb9003a153b87d92d3bf3daef6e9f2957d`.
The recorder remains the exact original `fff3423dfb6fb999261e690e4f02dc766707b99d`.

## Limits that remain

This is evidence-tool serialization and its actual finite self-check. No
Shenko model/runtime journey, cross-host exclusion, physical power-loss
durability or real 7/30-day window was executed by this check. Changing the
recorder digest requires a new observation directory/run ID. Previous raw
observations are not rebound or upgraded. Full S1–S5 delivery remains open.
