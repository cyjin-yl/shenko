# Playground WASI backend: finite native HTTP / reopen recipe

This is a recipe, **not an executed verification receipt**. The integrator runs
builds, tests and this native smoke serially in the managed devcontainer under
the shared heavy-work lock. Use a fresh owned directory on persistent storage;
never a production instance or another seat. No model or QQ service is needed.

## Actual path and limits

- `PUT /playground/{id}` commits immutable content + version metadata, selects
  `meta.json`, then durably imports a stable full `InboxEvent` into the existing
  `inbox_events` table. Startup repairs a crash between head selection and import.
- `PUT .../backend` creates another artifact version. It binds exact WAT/WASM CAS
  bytes and a typed, versioned invocation contract. No shell/interpreter runs.
- `POST .../backend/action` requires `{request_id,backend_digest,action,input}`.
  It atomically inserts the existing native `TaskRecord` and a durable
  `artifact.action` inbox fact. HTTP 202 is a real durable admission, **not guest
  completion**. The existing Cordis `NativeTaskHandlerPlugin`/dispatcher claims
  it, writes T1 `OperationRecord` + original `AttemptRecord`, runs WASI, seals E1
  evidence, and writes T2 `TaskResult` + its native-child durable inbox result.
- Query `GET .../backend/action?request_id=...` after response loss/restart. The
  response includes actual task/commit/input/operation/attempt/evidence/result
  identities. Identical replay returns the original receipt before checking the
  current binding. Different payload under the same identity conflicts; a new
  request from an old page explicitly returns `ARTIFACT_VERSION_STALE`.
- WASI preview1 `_start`; argv `playground-backend <action>`; stdin is the exact
  frozen input JSON. Environment contains only artifact/request IDs. `/home` is
  this artifact's persistent RW state; `/artifact` is RO. No network/host-shell
  capability is granted. Both output streams are bounded to 64 KiB.
- Completed-state receipts snapshot up to 256 entries / 16 MiB, including
  recursive directories, binary file CAS objects and literal symlinks. These
  are **receipt profile bounds, not an OS disk quota**. An incomplete snapshot,
  trap or lost controller leaves the original operation unknown/reconciling;
  it is never silently replayed. No universal exactly-once filesystem/source-DB
  transaction guarantee or arbitrary-guest CPU/memory limit is claimed.

The real guest fixture is
[`crates/shenko-ui/tests/fixtures/playground-counter.wat`](../../crates/shenko-ui/tests/fixtures/playground-counter.wat).
It reads and increments one binary byte in `/home/counter.bin`, then emits
`{"ok":true}` through WASI `fd_write`. It is a WAT command, **not Python, Node or
an interpreter surrogate**. Exact binary state versions and the change explanation
are in the sealed state manifests attached to the native result.

## Setup and first start (terminal A, maximum 90 seconds)

Set `SHENKO_BIN` to the integrator's actually built candidate binary, and choose
an unused local `PORT`. `DATA` and `EVIDENCE` must be fresh owned persistent paths.

```sh
export DATA=/tank/shared/projects/shenko/build/c12-native-smoke-owned/data
export EVIDENCE=/tank/shared/projects/shenko/build/c12-native-smoke-owned/receipts
export PORT=17862
mkdir -p "$EVIDENCE"
timeout --signal=INT 90s env -u ANTHROPIC_API_KEY -u SHENKO_USE_LOCAL_MODEL \
  -u NAPCAT_WS_URL -u NAPCAT_BOT_QQ \
  "$SHENKO_BIN" --data-dir "$DATA" start --port "$PORT"
```

An already configured model-free bootstrap/genesis is real deterministic mode,
not a claim the thought loop is thinking. The native task dispatcher still runs.
Do not call a readiness failure or a timeout a passing smoke.

## Real HTTP actions (terminal B, every request bounded)

```sh
export ART=6c70885f-a21e-4fb3-80c6-9e71a958db29
export BASE="http://127.0.0.1:$PORT/playground/$ART"
export FIXTURE=crates/shenko-ui/tests/fixtures/playground-counter.wat
curl --max-time 5 --fail-with-body -X PUT "$BASE?title=Counter&mime=text/plain" \
  --data-binary 'counter application' > "$EVIDENCE/content.json"
curl --max-time 5 --fail-with-body -X PUT "$BASE/backend" \
  --data-binary @"$FIXTURE" > "$EVIDENCE/binding.json"
jq -n --arg digest "$(jq -r '.artifact.backend.module_digest' "$EVIDENCE/binding.json")" \
  '{request_id:"counter-1",backend_digest:$digest,action:"increment",input:{amount:1}}' \
  > "$EVIDENCE/request-1.json"
curl --max-time 5 --fail-with-body -X POST "$BASE/backend/action" \
  -H 'content-type: application/json' --data-binary @"$EVIDENCE/request-1.json" \
  > "$EVIDENCE/admitted-1.json"
```

Poll a finite number of times; an unknown/failed receipt is evidence to inspect,
not permission to create another request or label the action successful.

```sh
for n in $(seq 1 100); do
  curl --max-time 2 --fail-with-body "$BASE/backend/action?request_id=counter-1" \
    > "$EVIDENCE/result-1.json" || break
  state=$(jq -r '.receipt.state' "$EVIDENCE/result-1.json")
  case "$state" in executed|failed|unknown) break;; esac
  sleep 0.1
done
jq -e '.receipt.state=="executed" and .receipt.result.result.output.ok==true' \
  "$EVIDENCE/result-1.json"
od -An -tu1 "$DATA/playground/$ART/state/counter.bin"   # expected: 1
```

Retain `.receipt.result.result.state_before/state_after`, stdout/stderr refs,
operation/attempt/receipt IDs, accepted commit and exact candidate SHA. The
structured result explains binary `counter.bin` changes and retains exact CAS
bytes instead of silently excluding non-text changes. Native T2 is the same
subject's durable input; C11's pending drain handles ordinary `playground` facts
even if the volatile wake queue was full, absent or paused.

## Response loss / reopen

Let terminal A stop (or interrupt only this owned instance), then run the **same
bounded start command on the same DATA**. Repeat the original POST using the exact
`request-1.json`; query the result again. Require the original task, operation,
attempt and receipt IDs, and byte value **still 1**, not 2. Check a changed input
under `counter-1` returns HTTP 409 `ACTION_CONFLICT` (do not hide the response).
An already unknown attempt must remain attached to its original operation and
attempt after recovery; no second dispatch is authorized.

## Code/content rollback without erasing newer business data

Save the original `.artifact.revision` from `binding.json`. PUT new frontend
content, then bind a new module version (copy the fixture to an owned file and
append a WAT comment to change its exact module digest). A **new** request ID
with the new digest increments the byte to 2. Both independent clicks survive;
identical content is not deduplicated across different request identities.

A new request with the old digest must return 409 `ARTIFACT_VERSION_STALE`.
Then `POST "$BASE/rollback"` with `{"revision":<original binding revision>}`.
Require original content/MIME/title/backend binding restored as a **new** revision,
old/new revisions retained by `GET "$BASE/versions"`, and binary business byte
still **2**. The content-first crash shape is also covered by the permanent
consumer test: an orphan write stays on disk, is not published as history, and
is never overwritten by the next version or rollback.

## Deliberately still open

This slice is the local finite WASI backend path, not all C12/PG01–PG05:
per-authenticated-actor action identities/authorization, complete manifest and
source/dependency/migration/health/recovery closure schemas, arbitrary runtimes,
cooperative external business-DB atomic outboxes, dynamic UI/DSL/compiler work,
continuous backend services/background schedules, and conversation-wide config
still require their own implementation and native evidence. Imported original
flat artifacts preserve their exact surviving bytes/metadata; versions already
lost by the old overwriting implementation cannot be invented.
