# Broker and finite-child construction ownership

## Why and exact scope

This increment descends from PR #329 at
`c59feb9ee6fba4aa19525d2af38e60b82a1728c8`. It changes three source files:
`shenko-broker/src/execution.rs`, `shenko-broker/src/broker.rs`, and
`shenko-subject/src/model_child.rs`. The execution helpers now borrow one explicit
set of original worker services. Frozen child construction groups only retained
transport/binding/generation inputs; it does not serialize another authority.

Two assumed-success panics are removed. Recovery returns an error before adding
or releasing physical accounting when an original owner cannot be resolved.
Continuation admission uses the occupied/vacant entry that owns the shared
remaining budget, instead of looking it up again with `expect`.

The worker still owns its original stop token, registry, domain, KV allocator,
receipt sink and request. The original dispatch fence, provider-only panic
boundary, Unknown debt, physical read hold, stream completion, frozen wire
schemas, prompts and grant checks are unchanged. These are concrete ownership
changes, not a lint exemption. The public Broker constructors and executable
fingerprint initialization are not modified by this slice.

## Actual review and limits

`static-structural-review.json` is a tree-sitter source observation, not Rust
compilation. All three edited files parse without syntax errors. Applying the
recorded edits in reverse reproduces the original parent bytes exactly. The
assertion macro sequence is unchanged in all three files and the serialized
`FrozenModelChild` definition is byte-identical. No test case is added or removed;
the legacy contract test helper only packs its existing construction inputs.
The source diff passes `git diff --check`.
The later whole-stage check returns 2 for two original unified-diff context
lines in `source.diff`; its original output is retained without changing those
evidence bytes. That whole-stage check is not claimed passing.

The before/after source bytes are retained as `.blob` evidence, not added to the
compiler input surface. `edits.json` contains the exact matching transformations,
including the new ordinary-error branch. Source inspection: ChatGPT; no
independent worker review is claimed. A previously returned Desktop Commander
process session expired before its result was read; saved files were re-read and
independently checked, rather than replaying the mutation. Initial evidence
preparation failed on a missing sparse-checkout parent; the already-written
contract appendices were checked, not appended again.

Runtime correctness still requires the unchanged pinned-container CI. No local
Rust compilation, runtime test, model inference, atomic source freeze or fully
qualified self-update is claimed. The parent CI's unresolved canonical, CLI,
SSH and MCP journeys remain failures until their own exact-source tests pass.
Inherited constructor/fingerprint panics and the parent's remaining lint debt
are not silently declared fixed by a stacked ratchet.

See [resource ownership](../../specs/quota-resources.md),
[finite model children](../../specs/model-children.md), and the complete
[ship/work contract](../../governance/COMPLETION.md). Nothing here completes the
actual NapCat/coding-seat journey, reference-model behavior, devices/accessibility,
physical fault observations or candidate-bound 7/30-day execution.
