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
