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
