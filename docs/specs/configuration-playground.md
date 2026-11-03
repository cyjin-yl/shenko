# Configuration Service & Playground — Functional Contract

SPEC §21 (single subject timeline / Proto-UI app), §23, §41.9, §41.10, §43
(configuration & playground), §44.4 (ownership). Runtime modules:
`shenko-ui::{config_service,owner,playground,playground_backend}`; shared
composition root: `bin/shenko/src/lib.rs`.

## Ownership — §44.4

- Before home setup, connectors or subject work, startup prepares the local
  installation control channel. `<data_dir>/bootstrap/claim-secret.json`
  delivers the capability to the installer; `claim-capability.json` in the
  same directory contains only its verifier digest. On supported Unix native
  targets the directory is 0700 and files are 0600, owned by the runtime user;
  opens reject symlinks, hardlinks, unsafe permissions and special files
  before reading. The native runtime does not fall back to unprotected
  plaintext storage on unsupported platforms.
- The data root itself is created with mode 0700 at creation time, not by
  tightening it afterwards. A shared parent may carry a default ACL that
  grants group or named-user access; inheriting it would leave the root
  writable by others, and owner-control would then refuse the installation.
  New directories therefore carry their own restrictive mode, and an existing
  data root keeps the permissions it already had: a root that is writable by
  others is reported as such instead of being silently rewritten or repaired
  behind the owner's back.
- Plaintext never enters stdout/stderr, logs, model context, CAS or replicated
  tables. Startup reports only the control-file path. Delivery publishes
  atomically without replacement, before its verifier; restarting between
  those publications completes the same claim. Corrupt or foreign-installation
  records fail visibly without rotation. Existing digest-only installations
  retain their previously delivered capability, rather than issuing another.
- `POST /api/owner/claim` with the capability creates the durable owner grant
  (`owner_grants` table), issues the admin bearer (`sat_*`, stored as a
  digest in the local-only `owner_admin_tokens` table — never replicated),
  and consumes the plaintext delivery file before committing the grant. The
  digest-only verifier remains local to recognize retries; claim identity is
  stable for that capability. Replaying it, including after restart, returns
  the original grant (`replayed` / `owner_already_claimed`), never a second
  grant or another bearer. A lost bearer requires explicit owner recovery;
  replay does not silently rotate administrative credentials.
- `human_principal_ref` is derived from the capability/grant, never from a
  request body. An HTTP visit is never proof. The default HTTP listener is
  `127.0.0.1:<port>`; remote access requires an explicitly authenticated
  deployment path, not an implicit wildcard listener.
- After the claim, administrative actions resolve the actor from
  `x-shenko-admin: <bearer>` (`OwnerState::actor` → `AuthenticatedActor`).
  Ordinary chat stays open: claiming installs an owner, it does not close
  the default conversation.
- `GET /api/owner/state` reports `{claimed, claim_pending}` only — no
  credential material. A consumed verifier does not mean a pending claim;
  storage errors are not reported as an unclaimed installation. The same
  ownership service is installed in model-free and model-connected startup.
- The CLI bootstrap view recognizes the committed `owner_grants` row, not
  just a separately provisioned `bootstrap_records.owner_grant_ref`. Its CAS
  reference contains the complete original grant, not the principal or
  authority object. This view is derived from the existing replicated fact;
  recovery does not require an additional grant record or any local token.

## Configuration service — §43.1

- One `ConfigService` per process (`shenko_ui::config_service`), constructed
  in the Start arm and shared into `UiServer` (`with_config`). Without a
  store the routes answer `available: false`.
- Namespaces register a `ConfigNamespaceHandler` (descriptor / read /
  consumers / apply). A namespace with no handler is not advertised.
- Key shape: `namespace` (e.g. `shenko.context-policy`), `logical_key`
  (e.g. `compaction_preference`), JSON `value`.
- Routes:
  - `GET /api/config/namespaces` — discover (open to every session).
  - `GET /api/config/entries/{namespace}/{key}` — read + adoption state.
  - `POST /api/config/preview` — preview a change (open; looking is never
    privileged). Returns stale base, sealed candidate ref, diff, affected
    consumers.
  - `POST /api/config/apply` — durable change. The actor is taken from the
    request credentials (`Owner` requires the admin bearer; `Subject`
    requires a delegated task), never from the body. There is **no routine
    approval gate**: the durable change receipt is the record, and consumer
    adoption is observed separately.
  - `GET /api/config/adoptions/{namespace}/{key}` — what consumers adopted.
- Durable schema (`crates/shenko-store/src/config.rs`): `owner_grants`,
  `config_entries`, `config_changes` with `UNIQUE(origin, logical_key)`
  (cross-node divergence on the same origin+key fails import honestly),
  `config_adoptions`, `owner_admin_tokens` (local-only, excluded from
  cognition TABLES). All replicated tables are `Verbatim` with
  `local_seq = updated_commit_seq`.
- Optimistic base: `expected_base_revision` mismatch → `BASE_CONFIG_STALE`,
  never a silent overwrite.

## Playground — §43.2, §41.9, §41.10

### Content vs business truth

- `ArtifactVersion` (file-backed, immutable version chain: bytes +
  `{revision}.json` + selected `meta.json`) is the artifact's *content*
  store. `shenko_store::ArtifactRecord` / `ArtifactManifest` /
  `RuntimeInstanceRecord` are the durable business facts; they replicate
  through cognition TABLES so a peer receives the same closure and a lost
  workspace rebuilds from facts, not from disk.

### Manifests, staging, publishing

- `PlaygroundState::sync_durable` builds the versioned manifest on every
  content write: `source_refs`, `build_and_dependency_lock_ref`, optional
  `frontend` bundle binding, optional `backend` (entry ref, sealed
  `runtime_ref`, sealed `service_contract_ref`), `state_bindings_ref`,
  `action_schemas_ref`, `operation_contracts_ref`, `health_contract_ref`,
  `recovery_closure_ref`. The manifest digest binds to its canonical JSON.
- `POST /playground/{id}/stage` (`stage_artifact`) — current head becomes a
  durable **candidate** manifest; lifecycle `staged`; the active manifest is
  unchanged.
- `POST /playground/{id}/publish` (`publish_artifact`) — one staged candidate
  (by digest) becomes active; other candidates are demoted, not deleted.
  Publishing a digest that was never staged is refused.
- `GET /api/playground` — the Proto-UI surface projection: enumerable roots
  (`PlaygroundState::roots()` from durable records — a stray playground
  directory is never mistaken for an artifact), artifacts with lifecycle,
  and the registered config namespaces.

### Backend runtimes

`BackendBinding` binds a backend to exactly one runtime provider:

- `wasi-command` — one isolated WASI preview1 guest per action; bounded
  stdout/stderr; `/home` isolated persistent rw state (16MiB/256-entry
  profile); `/artifact` read-only. Existing receipt path unchanged.
- `host-process` — a long-lived host-managed process speaking one JSON
  request/response per line on stdin/stdout (`jsonl-1`). The executable
  descriptor (`HostProcessDescriptor{program, argv, env, protocol}`) is
  sealed into CAS; the launcher re-reads and verifies it from CAS — the
  frozen input is identity, never parameters.

`ArtifactServiceController` is the launcher/owner:

- Every instance is a real selected root (`canonicalize`d artifact dir +
  dev/ino `root_identity`) plus a durable `RuntimeInstanceRecord` with
  `ProcessIdentity{pid, process_start_token, executable_digest}`.
- `recover_on_boot` (called before any launch, and in `UiServer::run`):
  durable `running` rows are re-verified against `/proc/<pid>/stat` field 22
  and the exe digest; a stale row is marked `unknown`, never silently reused,
  so a restart never starts a second copy of a live service.
- Without the controller, host-process backends answer
  `BACKEND_UNAVAILABLE` — no fail-open.

### Same task/effect operations, same launcher with Upgrade

- Every backend action (both runtimes) runs through the existing native
  task/operation/T1/E1/T2 ledger with the same `playground.backend` kind,
  idempotent `request_id`, original-receipt-wins replay, and the same
  evidence semantics (stream overflow/failure ⇒ INCONCLUSIVE, never a
  verified success from exit code alone).
- WASI completion retains the actual process exit code. A zero exit plus a
  valid bounded result and verified state snapshot may succeed; a nonzero
  exit remains failure even if stdout is valid JSON. A trap, cancellation,
  or missing conclusive observation does not acquire an invented exit code.
  A host-service JSON response is not a process-exit observation.
- Generation hand-off: the update seam stops long-lived services through
  `ArtifactServiceController::quiesce_for_handoff()` (returns exactly the
  live processes it stopped; the handoff receipt must name those) and
  restarts them after an abandoned activation via
  `resume_after_abandoned_activation()` under the current selected root,
  re-verifying root identity so a copied slot is never adopted. Handle is
  reachable via `UiServer::artifact_services()`. (The seam that calls these
  during the §41.12 phase machine is the update slice's; this slice provides
  the launcher contract it drives.)

### Frontend — §21

- The one served UI is the Proto-UI app. The Playground is a view of it:
  `projection.ts` adds the `playground` view + `playground_surface` node;
  `react-renderer.tsx` renders `PlaygroundSurface` (roots, artifacts,
  config namespaces — only what the API returned); `live-controller.ts`
  fetches `/api/playground` + `/api/config/namespaces` on view select.
  Unavailable is rendered as unavailable; no placeholder data.

## Native proof recipe (for the integration owner)

1. Obtain the claim capability from the local private `claim-secret.json`
   control file without copying it into command arguments or logs;
   `POST /api/owner/claim` over the protected local/access channel; keep the
   returned bearer out of transcripts and evidence archives. Startup stderr
   is not an acceptable credential-delivery channel.
2. `GET /api/owner/state` → `{claimed: true, claim_pending: false}`; a
   second claim with the same capability → `replayed` with the original
   grant and no bearer. Restart and repeat; no new delivery file appears.
3. Register one namespace handler; `discover` → read → `preview` (stale
   base check) → `apply` with and without `x-shenko-admin` (denied vs
   receipted); restart; read again — the entry and its change receipt
   survive; adoption row appears only after the consumer reports.
4. Playground: `PUT /playground/{id}` content → `PUT .../backend` (wasi) →
   `POST .../backend/action` with a `request_id`; repeat with the same
   `request_id` → original receipt, `replayed: true`. Bind a host-process
   descriptor; invoke twice; `ps` shows one service process; kill the
   process; restart the node; the durable row says `unknown` (or is
   re-adopted only after the live check passes); no second copy runs.
5. `POST /playground/{id}/stage` then `/publish` with the candidate digest;
   `GET /api/playground` shows lifecycle `published` and the manifest under
   the artifact's record.
6. UI: open the Playground nav view; roots, artifacts and namespaces render
   from the API; disconnect the store path and the surface reports
   unavailable instead of placeholder rows.

## Honest boundaries

`ArtifactServiceController` handoff closes admission before taking its bounded
service lock. Linux host services launch in a dedicated process group; quiesce
captures the exact PID/start-token/executable identity before signalling and
waits for group termination before releasing the runtime claim. A failed stop
or unproved group scope retains the owned handle/claim and returns an error.
This is a process-group contract, not proof of containment for a service that
deliberately escapes that group; stronger providers must supply their actual
container/cgroup boundary. Resume retains unfinished restart identities and
fails on missing runtime rows, unreadable descriptors or CAS digest mismatch.
No post-wait PID lookup is used to manufacture an empty stop receipt.

- Builds/tests/native runs were not executed in this slice (parent
  serializes integrated verification); the proof recipe above is the
  acceptance path.
- `owner_admin_tokens` is local-only by design (credential); config and
  artifact tables replicate, so a same-origin config change diverging
  across nodes fails import instead of merging silently.
- Workspace replication of the playground *directory* into a peer's
  cognition export uses the existing peer `workspaces` config
  (`node_sync`); the enumerable-roots contract itself is the durable
  `artifact_records`, not the directory listing.
