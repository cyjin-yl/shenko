# Original authored source checkpoints

The public GitHub connector creates commit objects with the connected account's
author and time. The original local commits are therefore preserved here as exact
Git objects, in addition to the source commit IDs and co-author trailers on the
publication commits. This is provenance preservation, not a product test.

`authored-checkpoints.bundle` is a Git v3 incremental bundle. It requires these
already published commits and their reachable objects:

- `7905e8413b96015d96ea301bd1f3b6937dcfff90`
- `262f980e2db13dac7d12e7df861fbf0ba707999d`

Its exact bytes, SHA-256, seven advertised refs, original author/committer times,
parents and trees are recorded in `checkpoints.json`. The original bundle creation
and verification outputs are retained without rewriting their timestamps or paths.
`git bundle verify` exited zero in the source repository with both public
prerequisites available. This is not a standalone repository backup.

The seven refs preserve the host usage description, safe native E1 export,
canonical capture budget repair, Node source-build failure archive, Node archive
recipe repair, a local integration checkpoint, and the **pending** coding-seat
lifecycle candidate. In particular, preserving
`e4d45747d53f7701aea4a293310192dac2126205` does not mean it passed review or was
integrated: independent review subsequently identified cancellation windows in
error retention, and its author is preparing a correction. The bundle retains
that original checkpoint unchanged.

After obtaining the prerequisite commits, inspect or recover only the named refs:

```sh
git bundle verify docs/evidence/source-checkpoints-20261116/authored-checkpoints.bundle
git bundle list-heads docs/evidence/source-checkpoints-20261116/authored-checkpoints.bundle
git fetch docs/evidence/source-checkpoints-20261116/authored-checkpoints.bundle \
  'refs/heads/checkpoint/*:refs/remotes/authored-checkpoints/*'
```

The bundle does not confer execution, send or replay authority. Historical model,
CI and runtime results keep their own candidate and environment bindings. The
new retention/capacity code still requires its corresponding Rust/SSH CI run;
the finite Node archive diagnostic does not establish a WASIX build or Node
runtime capability.

The separately authored CI archive commit
`2502ab04a5e1a2ef1875098a0997a87963ed7954` was created after this bundle.
`ci-archive-author.commit` preserves its exact raw Git commit bytes, verified
against that commit ID. `ci-archive-source-tree.json` records its original base
tree and all 45 added blob paths, modes and identities; those same blob bytes are
retained in the published CI archive. They determine the original source tree
without duplicating both large ZIPs inside another bundle. This record preserves
the original author and timestamp separately from its later integration commit.

## Reviewed seat and E1 successor follow-up

`seat-successor.bundle` separately preserves the reviewed seat follow-up
`95e0a2d68b459f3cfee9561d5211a40b3d6a1499`, the E1 successor source
`74ea34d6c6c7f52875a575d74b869480b63aac4b`, their intermediate original commits,
and the local integration checkpoint. It requires public `83bf3914` and the
original `e4d45747` / `5a1075de` objects available from the first bundle above.
Obtain and verify that first bundle before fetching these follow-up refs.
`seat-successor-checkpoints.json` records the exact hashes, authors, trees,
prerequisites and an independent comparison of every changed Rust/Python source
file after integration. All compared source blob identities match their reviewed
originals; the documentation map was merged with the current public navigation.

The seat correction retains observed close failures before a cancellation can
lose them and puts all seven registrations in their actual owner generation.
The E1 extension preserves the original first Unknown and only admits its one
typed definite successor, without a new execution permission. Independent
source reviews found no remaining confirmed blocker in those bounded changes.
The six seat and seven successor observation windows still need the integrated
candidate's actual Rust/SSH run. Source preservation and review do not establish
that runtime result, actual coding, complete writer fencing or safe shared-CAS GC.

## GC declarations and the observed capture repair

`gc-capture-repair.bundle` preserves original GC source `10188718`, capture
repair `7191f89f`, and the local integration checkpoint `5b1c8a31`. Its exact
hash, original authors and times, prerequisite objects and three source-blob
comparisons are in `gc-capture-checkpoints.json`; create/verify output and
advertised refs are retained. Verification exited zero with public `83bf3914`,
original `2798bb37` from the first bundle, and local `ff771184` from
`seat-successor.bundle` available. Obtain those prerequisites before fetching
this incremental bundle.

The GC source verifies declarations at the callable API; it does not install
Store reclamation or shared-CAS exclusion. The capture repair follows six
actual Rust failures in [CI 83bf](../ci-83bf3914-20261116/README.md). It preserves
the original nine observation windows and budgets. The retained finite C API
investigation used system SQLite 3.46.1, not the pinned project binary. Neither
this bundle nor that investigation establishes the repaired Rust result.

`ci83-archive-author.commit` separately preserves the exact raw commit object
for `cf4474209eed96feadcf0d00e260bb634a87e276`.
`ci83-archive-source-tree.json` binds its original public `83bf3914` base and
all 84 added blob paths to its original tree. Every blob matches the integrated
archive. This preserves the author's original commit without placing another
copy of all four original ZIPs into a source bundle.

## Seat arm admission, Unknown outcomes and CI compilation corrections

`seat-admission-ci-repairs.bundle` preserves original arm source `2110ca5c`,
outcome source `a17127c3`, the two CI corrections through `86892957`, and the
pre-archive integration checkpoint `95451283`. Obtain public `a1d3fa6e`,
original `5b1c8a31` from `gc-capture-repair.bundle`, and original `74ea34d6`
from `seat-successor.bundle` before fetching these refs. Exact bundle bytes,
hashes, original authors and times, prerequisites and source comparisons are
recorded in `seat-admission-checkpoints.json`; bundle verification exited zero.
The outcome integration merges the arm correction's module documentation; its
code from the first `use` item onward is byte-identical to the original source.

The combined seat increment has nine distinct scenarios, including the explicit
NotSent control. It rechecks committed answer bindings when arming and preserves
Unknown after a possible write, including a failed durable close. It does not
establish continuous writer exclusion, actual coding-seat I/O or activation
fencing. The two CI corrections borrow an existing test value and box the
manifest page; they add no scenario or relaxed gate. [CI a1d3](../ci-a1d3fa6e-20261116/README.md)
failed before workspace tests or the pinned guest build could execute. These
new sources require their own actual candidate run.

`cia1-archive-author.commit` preserves the exact original archive commit
`0578856d`. Its base, original tree and 30 changed blob identities are in
`cia1-archive-source-tree.json`. All 29 new archive files are unchanged after
integration. The evidence index was merged with current entries, so its
original bytes are separately retained as `cia1-archive-source-index.blob`.
The merged index is not claimed byte-identical to the original source index.

## C19, Node source repair, successor query and S2 build receipts

`s2-c19-node-checkpoints.json` preserves 21 exact original commit objects and
their complete tree deltas, including the intermediate integration commits and
the original archive indexes. Each raw `.commit` is SHA-1 checked against its
original Git identity. Every tree delta was independently recomposed in a
temporary Git index and matched the original tree. Six additional original blob
payloads retain bytes not present in the named published trees. No original
author, committer time, parent, failure or public evidence path was replaced.

Obtain public `10d4a325` and `d2dc8ee4`, original `5b1c8a31` from the earlier
GC bundle, and original `86892957` from the seat/CI repair bundle before
reconstructing these objects. The JSON identifies each original parent/base
tree, changed path/mode/blob and separate original payload. Raw commits retain
the complete original metadata. This is incremental provenance, not a standalone
repository backup. The initially generated bundle duplicated the already
retained original CI ZIPs; the final representation uses these raw commit/tree
records instead. Its initial argument-order failure and subsequent bundle
creation/verification observations are preserved in
`s2-c19-node-bundle-attempts.json` and the original command streams.

The eight named checkpoints cover the zero-scenario seat fixture correction,
C19's source and actual local 17-scenario check, the complete second Node source
failure, the one-predicate E1 query correction, the relative Node header-link
source, the complete failed 10d4 CI archive, the S2 owned-build/receipt source,
and their local integration. C19's source PRs remain intact. Node's three finite
source checks do not establish a Node build; S2's five new plus three existing
refusal windows still require actual Rust execution. The original failed runs
retain their original candidate bindings.

## PTY transport receipts and reviewed fixture corrections

`pty-checkpoints.json` separately preserves six original commits through
`4229b45b`, including the `5309c6a0` source base and the full five-commit PTY
chain. Its prerequisite `95451283` is in the earlier seat/CI repair bundle;
its other blob sources are public `10d4a325` and `68de5ffc`. Four retained
additional blob payloads preserve intermediate source versions. Raw commit
hashes and every reconstructed tree were independently checked.

Production source remains `e53c1ae6`. The subsequent four commits introduce
five real-protocol fixture windows and repair the observed output/cleanup
ordering issues in their source; no Rust execution is claimed. The exact local
static observations, their incomplete parser environment and original patch
whitespace are retained in the [PTY review archive](../pty-write-review-20261116/README.md).
None of these records grant a send/replay permission, convert Unknown to NotSent,
or establish installation-wide fencing, real coding or complete S2 acceptance.

## Retained objects, prepared identity and original observation exports

`retention-preparation-export-checkpoints.json` preserves 31 further exact raw
commit objects, 260 tree deltas and 21 additional original blobs. Raw Git SHA-1
identities and every recomposed original tree were independently checked. The
479,661 additional bytes preserve intermediate code and documentation versions
without duplicating the complete CI ZIPs inside another bundle.

Obtain public `3e62c0e5`, original `c50a0eb7` from the S2/C19/Node records above,
and original `4229b45b` from the PTY records above. The new publication also
retains the reviewed pre-provenance tree
`fee7302ed99e2453dc67f620b42781bbe1169337`; the JSON records it as a blob source
through original local integration `b4e192c`. Its normalized publication commit
has the same tree. The full base-tree/path/mode/blob deltas and separate payloads
allow each original tree to be reconstructed. This remains incremental source
provenance, not a standalone repository backup.

The named original checkpoints cover the PTY binding correction, sealed
prepared-artifact identity, native command cleanup, Store read-only retention
reader and its carrier/alias fixes, PTY/update original exports, installation
permit design, unresolved Clippy input-scope audit and the integration/archive
commits. Their raw objects retain the original authors, committer times, parents
and messages. Original intermediate versions and failures are not overwritten.

[The source and integration review archive](../retention-preparation-export-20261116/README.md)
records exact file comparisons and each finite scope. The new source still
needs its own Rust/Clippy execution. The earlier
[3e62 CI result](../ci-3e62c0e5-20261116/README.md) is retained as observed:
Rust passed, Clippy failed, and some successful-test raw originals were not
exported. These provenance records cannot fill that gap or make the later
sources passed. They grant no execution, replay, deletion or installation-wide
writer authority.
