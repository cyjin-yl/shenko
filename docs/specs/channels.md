# Channels — functional contract

SPEC §15–16, §35.6, §41.6 · C13. This is the contract the channel slice
implements; it names the exact types and runtime paths so a second channel
can be added without touching the core.

## 1. Plugin boundary (`shenko_types::channel`)

Everything platform-specific lives behind `ChannelPlugin`:

| method | owns |
| --- | --- |
| `channel_id` / `sources` / `kinds` / `prompt_names` | the advertised reply grammar and inbox labels |
| `classify(source, raw) -> Option<ChannelEnvelope>` | inbound normalization; `None` = not a message this plugin owns |
| `staged_text(envelope) -> String` | the model's view: `[who · audience] body` |
| `resolve_reply(kind, id) -> Option<PublicationTarget>` | the opaque-id round trip from staged header to sealed target |
| `describe(target) -> Result<ChannelDispatch, ChannelError>` | the platform action, parameters and acceptance predicate |
| `dispatch(dispatch, content) -> BoxFuture<Result<ChannelReceipt, ChannelError>>` | the external call only; the durable record exists before this runs |

The core never parses platform ids, never re-derives acceptance from a status
word, and never owns a second operation ledger. `ChannelRegistry` rejects
duplicate channel ids, source labels and reply tags at registration.

## 2. Publication target (sealed record, `shenko_types::output`)

`PublicationTarget` v2: `{schema_version: 2, channel, kind, id: String,
account: String, tenant: Option<String>, conversation: Option<String>}`.

- `id` is an **opaque string** — QQ decimal ids, QQ-Official OpenIDs, Feishu
  chat ids all fit; the core never parses it.
- `account` is the owning connector scope (`qq:<bot_qq>` for the shipped QQ
  plugin). An **empty** account appears only on records produced by the
  migration of a pre-v2 target; a single-account plugin may serve it and
  records the account it actually used.
- Deserialization is strict (v2 only, `deny_unknown_fields`). Old
  `{"kind":"qq_group","id":N}` records are readable **only** through
  `PublicationTarget::read_legacy`, which is used by the Store migration —
  there is no unversioned alias.
- Sealed-record consequences: `ParsedOutput` schema 4→5, `StepDecision`
  schema 7→8. `Store::open` runs `migrate_publication_targets`, which re-seals
  legacy decision objects (rewriting their targets), updates
  `subject_steps.decision_ref` and `tasks.input_ref`, and records provenance
  in `channel_record_migrations`. It never touches `subject_steps.input_plan_ref`.

## 3. Outbound lifecycle (`shenko_store::channel`)

The bin runtime (`bin/shenko/src/main.rs`, publication consumer) drives every
ready publication through one pipeline, for every registered channel:

1. `Store::ready_publications(limit)` reads the ready index.
2. No plugin serves `target.channel`, or `describe()` errs
   (`UnsupportedTarget`/`UnresolvedReply`): `Store::refuse_publication` —
   durable `blocked` state, effect `none`, the runtime **keeps running**.
   The old "non-QQ target cancels the runtime" bail is gone.
3. `Store::arm_publication(intent_id, &dispatch)` — T1: claims the intent and
   arms the operation+attempt before the network is touched. `Ok(None)` means
   another dispatcher already claimed it: never a second send.
4. The runtime sets `dispatch.echo = operation_id` (durable correlation).
5. `plugin.dispatch(...)`:
   - `Ok(receipt)` → `record_publication_receipt` (E1). `accepted` is the
     plugin's own verdict (`onebot_retcode_zero` for QQ).
   - `Err(Transport | Unknown)` → `record_publication_unknown` — the request
     may have reached the platform; reconciliation settles it, never
     blind-resends.
   - `Err(UnsupportedTarget | UnresolvedReply | Rejected)` after arming →
     deterministic refusal recorded as a failed receipt (effect `none`,
     resolved).
6. Every outcome is mirrored as a subject-readable `channel_runtime_facts`
   row (`kind = publication_outcome`, states `accepted | rejected | unknown
   | refused`).

`Store::reconcile_subject_publications` (moved from `intents.rs`, unchanged
semantics) settles `claimed`/`unknown` publications after a restart: an armed
send whose owner died is local loss-of-control evidence, recorded `unknown`
(`provider_stop_proven: false`), never assumed failed.

## 4. Inbound lifecycle

- WS connector (`NapCatClient::run_connection_loop`) and the optional
  napcat-cli poller both ingest into the **same** `IngressSpool`; the source
  identity `["napcat", account, "onebot_v11", conversation, message_id]`
  dedups a message seen by both transports. napcat-cli is a second **source**
  of the same QQ plugin — one registry entry, one classification, one ledger.
- Admission (bin): `plugin.classify` runs on the raw payload; the envelope
  (not the raw wire frame) becomes the inbox payload, so the model projection
  is identical at admission and replay (an envelope classifies to itself).
  Payloads the plugin does not classify (notices, meta) keep the raw frame
  and stage with a plain `[source]` header.
- Attachments: `parse_message` produces `ChannelAttachment` values whose
  `locator` is confined to the configured NapCat-host `file_root`
  (`NapCatConfig::file_root`, host name `file_host`). At admission the runtime
  resolves each locator **on the owning host** through the plugin's file
  capability and seals the bytes into the store (`ChannelAttachment.object`,
  content-addressed); the remote path never becomes a local file. A locator
  that cannot be resolved stays a locator and is recorded as a
  `attachment_locator` fact (state `pending`) — the message is never dropped.
- No S1 receipt means no ACK, even if delivered: the ack mirror only acks
  spool rows whose disposition commit exists in the control ledger.

## 5. Administration is owner-set

`NapCatConfig` keeps four facts apart: `ws_url` (where), `access_token`
(transport credential — Authorization header only, never in URLs, logs,
receipts or bindings), `bot_qq` (the served account), `admin_qq` (owner-set
administrators, recorded as `admin_grant` channel facts at startup).
Administration is never derived from a group role, the transport token or
familiarity; the envelope's `role_grant_ref` (`qq-admin:<qq>`) is set only
from `admin_qq`. Reachability stays open: anyone who can reach the account
may talk (R036).

## 6. Runtime errors are facts (SPEC §45.14)

At each unable-to-continue boundary — ingress connection loop, admission,
ack mirror, publication consumer, napcat-cli source — the runtime journals
`Store::journal_runtime_error` with the actual `agent_id`, a stable `kind`,
the module path and bounded extra, before cancelling. The journal entry is
not a delivery receipt: it never retries the operation and never opens an
issue by itself. Independent operation unknown/conflict receipts stay on the
operation ledger.

## 7. napcat-cli wire contract (verification prerequisite)

`NapCatCliSource` polls `GET {base}/events?after=<seq>&limit=256` expecting
`{"events": [{"seq": <n>, "event": {<OneBot v11 message>}}]}` with
monotonically increasing `seq`. This shape is documented, not yet verified
against the pinned upstream napcat-cli (SPEC §15.4); a mismatch is a typed
`CliSourceError`, never a guessed parse. **Enabling this source against a
real daemon requires parent verification of the wire shape.**
