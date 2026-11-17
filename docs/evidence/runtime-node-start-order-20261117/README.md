# Model-free runtime identity initialization order

Parent: PR #330 at `79291b136c41517578111f5293354cecd552b64d`.

## Why

PR #329's original SSH/bootstrap stderr reports `node identity conflict` before
health readiness. Source inspection found an independent ordering gap:
`establish_runtime` opened Node and advertised presence, but its early
`RecoveryOnly` return had never called the existing identity/Store-binding opener.
Later recovery services and the replication worker could therefore each enter
first initialization. The original strict identity check must not be relaxed.

The serial first blocking startup step now calls the original `open_node` before
advertising presence or spawning owned consumers. No new identity rule, conflict
retry, adoption shortcut, model lease or primary fence is added. The original
stderr does not record the exact competing interleaving, so this is not a claim
that every possible identity conflict has this cause. In fact the unchanged SSH
journey passed in PR #330; a lucky later scheduling outcome is not the fix.

## Verification scope

One added test uses real Store, seed genesis, Node files and `establish_runtime`
with an explicitly unconfigured model profile. A read-only Node observer checks
identity and subject binding before any test-side mutating opener. It checks
absence of model candidacy and primary/dispatch authority, cancels and awaits
the original heartbeat, then closes/reopens the files and requires the same
Node identity. This is one coherent two-pass journey, not two models or a matrix.
The optional original observation export uses a unique sibling of the existing
SSH evidence file; it never overwrites that journey's receipt.

The exact production substitution reverses to parent bytes; the appended test
module is separate. Both Rust files parse without syntax errors. The original
SSH, component, start-boundary, recovery and Node implementation source hashes
remain unchanged. These are static observations only: no local Rust compilation,
new test pass, process-restart or model-behavior result is claimed before CI.
The pre-fix added test has not been executed; the deterministic missing-ID
condition is a source finding, not a fabricated failing test run.

The [node contract](../../specs/nodes.md) retains all model-health, authority,
control-log and identity obligations. See the accompanying exact parent CI
archives; neither those failures nor the full
[completion contract](../../governance/COMPLETION.md) is replaced by this patch.
Actual NapCat/coding-seat qualification, all-node coordination, services/UI,
accessibility, named runtime/device support, physical faults and candidate-bound
7/30-day observations remain outstanding.

Source review and implementation: ChatGPT. No independent-worker review or
unavailable devcontainer execution is claimed.

The whole staged whitespace check returned 2 for retained original logs/diff
bytes; its full original output is included. The separate product/contract
check returned 0. No evidence bytes or repository gate were normalized,
excluded or weakened to manufacture a passing whole-stage check.
