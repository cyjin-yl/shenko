# Broker resource truth — issues #272 / #273 (2026-11-11)

Worktree `worktrees/wt-broker-resource`, branch `feat/broker-resource-truth-20261111`, rebased onto `4f24740` (origin/main; the change's static evidence was audited against `375238a`, identical code).
Files: `crates/shenko-broker/src/resource.rs`, `crates/shenko-broker/src/broker.rs`, `docs/specs/inference.md` (behavior doc). `execution.rs`, `SPEC.md`, `docs/requirements/*.json` untouched.

## Static evidence (baseline, verified before editing)

**#272 — enforceable external occupancy never reached the dispatch path**

- `resource.rs` (baseline): `register_external_client` only appended to the `external` ledger; `effective_compute_capacity` subtracted enforceable units purely for the diagnostic snapshot.
- `broker.rs` (baseline, `Broker::run` refill loop): `let Some(guard) = self.domain.admission().try_admit()` — admission looked only at the `AdmissionController` counter; external reservations were invisible. `hard_guarantee_available` returned `true` with a fully-reserved domain because it only checked `all(client.enforceable)`.

**#273 — `resolve` early return bypassed the incarnation conflict check**

- `resource.rs` (baseline): `ResourceDomainRegistry::register` compared `engine_identity` only for the *same* `domain_id`, so two different `domain_id`s could register the same `engine_incarnation` under the same `engine_identity` and hold two independent budgets.
- `resource.rs` (baseline `resolve`): both the alias hit (`self.aliases.get(endpoint_origin)`) and the already-registered `profile.resource_domain_id` branch `return`ed before the declared-incarnation conflict check — a profile resolving through an alias never ran the check, contradicting the method's own doc comment.
- `ResourceDomain` had no way to adopt a later-observed incarnation without `register`, so `observe_incarnation` could not exist.

## What changed

### ResourceDomain owns the one compute budget

- New fields `admission_gate: Mutex<()>` and `budget_changed: Notify` on `ResourceDomain`.
- New `ComputeBudget { configured, active, unknown_charged, external_enforceable, external_advisory, available }`; `compute_budget()` is the single place the figure is computed: `available = configured − max(active, unknown_charged) − external_enforceable`. The `max` prevents double-billing an unknown charge that still holds its guard (guard retained occupancy in `active`, and `debt` records it again).
- `ResourceDomain::try_admit()` locks `admission_gate`, checks `occupied + external_enforceable < configured` *inside the gate*, then delegates to `admission.try_admit()` — a concurrent external registration cannot squeeze past a checked admission.
- `register_external_client` / `release_external_client` update the ledger and `budget_changed.notify_one()`. `external_reserved_units` (enforceable only) and `external_advisory_units` expose the ledger.
- `effective_compute_capacity()` returns `budget.available` — the displayed bound and the enforced bound are the same number.
- `recover_unknown()` delegates to `admission.recover_unknown()` (retained charge may exceed cap, blocks new work). `handoff_snapshot` now embeds `compute_budget`.
- `Broker::run` calls `self.domain.try_admit()` (not `admission().try_admit()`) and `select!`s on `self.domain.budget_changed().notified()` so a released external reservation wakes queued work. `BrokerHandle::recover_unknown`/`register_recovered` route through `domain.recover_unknown()`; `handle.admission` is `pub(crate)` and only for reads (`active_count`/`capacity`). `handle.effective_capacity()` doc updated.

### Physical identity scope = `(engine_identity, engine_incarnation)`

- `ResourceDomainRegistry::register` refuses a second `domain_id` claiming the same incarnation **within the same engine_identity scope** — different engines may legitimately share a vendor-scoped incarnation string. All registration-state fields are checked; the `insert` is inside the write lock, so concurrent registrations cannot race.
- `ResourceDomain::observe_incarnation(&str)` (pub(crate)) records a later observation; `ResourceDomainRegistry::observe_incarnation` refuses if the incarnation is owned by another domain in the same scope. An incarnation is never erased (the setter takes `&str`, not `Option`), and the registry sees every adoption.
- `resolve` restructured: pick `candidate` (alias `endpoint_origin` → `profile_id` → `profile.resource_domain_id`), then run the declared-incarnation check on **all** paths — no early return. Two checks: (a) is the declared incarnation owned by a different domain in the candidate's engine scope → bail; (b) does the candidate record a *different* incarnation than the profile observes → bail. Only the candidate itself is excluded from (a) — excluding the declared domain id would let an alias resolve into a conflicting domain and still pass. `incarnation_conflicts` reports scoped pairs. `resolve`'s doc now matches behavior.

## Reproduction & judgment

Command (CT102 dev container, shared target):

```
ssh -o BatchMode=yes root@10.10.10.12 "docker exec shenko-dev-persistent-20261026 bash -lc \
 'cd /tank/shared/projects/shenko/shenko/worktrees/wt-broker-resource && \
  export PATH=/usr/local/cargo/bin:$PATH CARGO_TARGET_DIR=/var/idw-target && \
  cargo test --offline --locked -p shenko-broker --lib -j4'"
```

Result: `18 passed; 0 failed` for `--lib` (6 pre-existing, 12 new). The whole
package was also run: `cargo test --offline --locked -p shenko-broker` →
18 lib + 17 `kv_capability` + 1 `kv_unknown_read` + 2 `provider_usage` = **38
passed, 0 failed**; `cargo check --offline --locked -p shenko-broker
--all-targets` compiles examples and integration tests.

New tests and what they prove:

| Test | Proves |
|---|---|
| `resource::tests::an_enforceable_external_reservation_gates_admission_and_diagnostics` | C=2 + external=1 → exactly one `try_admit` succeeds; `effective_compute_capacity` == real availability (1 → 0 → 1); releasing the reservation frees the slot; terminal release frees it too. |
| `resource::tests::unknown_debt_is_reported_once_and_never_deducted_twice` | One quarantined request with guard held: `active=1`, `unknown_charged=1`, `available=1` (not 0) — no double deduction. |
| `resource::tests::one_observed_engine_cannot_register_two_budgets` | Two domain_ids, same `(engine, incarnation)`: second `register` is an error; `domains().len()==1`; resolving the refused id fails. |
| `resource::tests::an_alias_of_one_engine_resolves_to_one_budget` | Alias and declared paths `resolve` to `Arc::ptr_eq` domains; the shared budget is exhausted for both bindings. |
| `resource::tests::a_registered_id_claiming_an_incarnation_owned_elsewhere_is_refused` | Profile naming `domain-b` while observing `inc-1` (owned by `domain-a`) fails and names the real owner. |
| `resource::tests::an_alias_hit_still_runs_the_incarnation_check` | Alias-hit path to `domain-b` observing `inc-1` (owned by `domain-a`, same scope) is refused — the early-return bypass is closed. |
| `resource::tests::truly_independent_engines_keep_independent_budgets` | Same incarnation string, different `engine_identity`: two independent budgets; one full does not block the other. |
| `resource::tests::observe_incarnation_owns_the_observation_or_refuses_it` | Adopting an owned incarnation is refused; an unowned one is adopted. |
| `resource::tests::concurrent_registration_of_one_incarnation_yields_one_budget` | 8 threads racing `register` on the same `(engine, incarnation)`: exactly one `Ok`, `domains().len()==1`. |
| `resource::tests::an_unknown_incarnation_is_not_claimed_as_identity` | `engine_incarnation: None` is not an identity: two unknown-instance domains register; `hard_guarantee_available` is false; resolving the declared id still works. |
| `broker::tests::an_enforceable_external_reservation_limits_real_dispatch` | **Real dispatch path**: C=2 + external=1, two submitted requests → `ControlledProvider` is entered exactly once (`max_active == 1`), second start times out while `active_count()==1` and diagnostic `Some(0)`; releasing the in-flight guard lets the queued request start — no re-submission. |
| `broker::tests::an_external_reservation_release_resumes_queued_dispatch` | C=1 + external=1 → provider not entered within 200 ms; `release_external_client` → queued request dispatches via `budget_changed` wake and completes. |

The `ControlledProvider` fixture is fully scripted (semaphore gate, `starts` channel, `max_active` counter); no network is involved.

## What this file does not prove

- **Whole-product regression.** The full `shenko-broker` package (lib + all integration tests) passes; other crates (`shenko-subject`, `shenko-store`, binaries) were not rebuilt or re-tested here; the integrator re-runs on the merged tree.
- **Real provider behavior.** `ControlledProvider` is a scripted fixture: tests prove the dispatch/budget mechanism, not that a real vLLM/HTTP engine actually reports an incarnation or that any real external client exists. No external network was used.
- **`engine_identity` correctness.** The scoped-uniqueness rule assumes `engine_identity` is a faithful deployment-supplied identity. If two profiles report the same engine under *different* `engine_identity` strings (or a restart rotates `engine_identity`), the registry cannot detect they are one physical engine — that is the honest-degradation boundary, not a claimed guarantee.
- **Adoption of these APIs.** `ResourceDomainRegistry::resolve`, `register_external_client`, `bind_alias`, `observe_incarnation` have no production callers in this snapshot (verified by workspace-wide grep): the defects were contract-level wrongness in APIs awaiting multi-profile/external-client wiring. The tests prove the mechanism end-to-end through the real `Broker::run` loop and the real registry, not that some existing call site now behaves differently — none exists yet.
- **Incarnation uniqueness semantics of any vendor.** Vendor-scoped strings are taken at face value; whether a vendor ever reuses an incarnation string across physical instances is out of scope.
- **`hard_guarantee_available` semantics unchanged** beyond the doc: it remains a predicate over recorded facts (known incarnation + all-enforceable externals + known KV), not a runtime promise.
- **`Broker.domains` dead-code warning** predates this change (verified against `HEAD:broker.rs`); likewise the `CapacitySnapshot` ambiguous-glob warning (`resource.rs` already defined its own at baseline line 218).
