# Playground backend smoke (native HTTP + reopen)

Finite recipe for the §43.2 backend path: bind a **real WAT guest** to an
artifact, invoke an action through native HTTP, prove durable admission,
dedup and state persistence across a restart. Run inside the devcontainer
with `shenko` serving the UI (`http://127.0.0.1:<ui-port>`); substitute the
actual port. Not a Python/interpreter claim — the guest below is WASI.

## 0. Fixture

The same WAT the unit tests use lives in
`crates/shenko-ui/src/playground.rs` (`BACKEND_WAT`): it writes `state.txt`
into its RW `/home` and a JSON receipt to bounded stdout, then exits 0.
Save it verbatim as `/tmp/backend.wat` for the recipe (see the test for the
exact source), or use any WAT that exports `_start` and obeys the contract:

- argv: `playground-backend`, `<action>`
- env: `SHENKO_ARTIFACT_ID`, `SHENKO_ACTION`, `SHENKO_REQUEST_ID`
- stdin: the frozen `input` JSON bytes
- preopens: `/home` (rw → `state/`), `/artifact` (ro → artifact dir)
- stdout/stderr: bounded to 64 KiB

## 1. Create + bind

```sh
ART=<new entity uuid>
curl -X PUT "http://127.0.0.1:$PORT/playground/$ART?title=smoke&mime=text/plain" --data-binary 'v1 content'
curl -X PUT "http://127.0.0.1:$PORT/playground/$ART/backend" \
     -H 'Content-Type: application/octet-stream' --data-binary @/tmp/backend.wat
# → artifact.backend.module_digest pins the guest bytes
```

## 2. Invoke the action (real WASI execution)

```sh
curl -X POST "http://127.0.0.1:$PORT/playground/$ART/backend/action" \
     -H 'Content-Type: application/json' \
     -d '{"request_id":"smoke-1","action":"tick","input":{"n":1}}'
# → ok:true, action.state:"executed", exit_code:0,
#   output:{ok:true,actor:"guest"}, changed_files:["state.txt"]
```

Check the guest's own business state on the host:

```sh
cat $SHENKO_DATA_DIR/playground/$ART/state/state.txt   # → guest-data
cat $SHENKO_DATA_DIR/playground/$ART/actions/smoke-1.json  # durable receipt
```

## 3. Dedup, conflict, stale

```sh
# Same request_id + same payload → original receipt, no re-execution:
curl -X POST .../backend/action -d '{"request_id":"smoke-1","action":"tick","input":{"n":1}}'

# Same request_id + different payload → 409 ACTION_CONFLICT:
curl -X POST .../backend/action -d '{"request_id":"smoke-1","action":"tick","input":{"n":2}}'

# Rebind a different module, then replay smoke-1 → 409 ARTIFACT_VERSION_STALE:
curl -X PUT .../backend --data-binary @/tmp/backend2.wat
curl -X POST .../backend/action -d '{"request_id":"smoke-1","action":"tick","input":{"n":1}}'
```

## 4. Versions, crash shape, rollback

```sh
curl -X PUT ".../playground/$ART?title=v2" --data-binary 'v2 content'
curl ".../playground/$ART/versions"            # → revisions [1,2,…]
curl -X POST ".../playground/$ART/rollback" -d '{"revision":1}'
# → new revision whose bytes are exactly rev-1; history kept
```

## 5. Reopen / recovery

Restart `shenko`. `PlaygroundState::new` reloads `meta.json` + `versions/`
+ `actions/`: `GET /playground/$ART` serves the same revision bytes,
`GET /playground/$ART/backend` still reports the bound module digest, and
replaying `smoke-1` returns the stored receipt without re-executing.

## Not covered by this recipe

Full §43.2 manifest fields (source/dependency/recovery refs), non-WASI
runtimes, actor authentication, event/outbox export beyond the subject
inbox, and PG01–PG05 full acceptance — those remain open C12 work.
