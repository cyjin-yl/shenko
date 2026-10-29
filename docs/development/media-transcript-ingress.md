# Continuous-media transcript revision ingress

SPEC: §15.2, §20, §41.2/§41.3. Work package C16 / issue #21, case A16.

This document describes the durable ingress contract **for real transcript
providers**. It is not an ASR implementation and does not claim one exists:
`text` is always real provider output, the kernel never synthesizes it, and a
synthetic transcript is never rendered as recognition. Real ASR/audio device
prerequisites are listed at the end and remain outside this slice.

## Verification status

This is an **unverified source candidate**, not runtime/ASR evidence. No
build, test, formatter, metadata command, native program, server, model,
production account or audio device was run for this repair. The integrator
must combine C11's shared durable-input drain (`source LIKE 'media/%'` in
both selection and the atomic staged-input consumed update) before native
acceptance: that change is C11 commit `4b492e4d`, included in C11 head
`1e4cb27534d9450bb06340d618984a7634599c20` / PR #117. C12's
`admit_inbox_event` is a separate producer of this SAME existing inbox table,
not a second drain; media inserts its original inside the media transaction
to avoid a split admission. The recipes below are work to run, not results.

## Path

```
provider ─ typed input ─▶ MediaIngress (native service or /api/media/*)
                          │
                          ├─ Store.admit_media_revision / admit_media_stream /
                          │  end_media_stream        (one ledger commit:
                          │   raw fact + effective observation + stream row +
                          │   the durable inbox_events original)
                          │
                          └─ bound InboxEvent (event_id from the receipt)
                              └─ volatile wake → same subject inbox, AND the
                                 durable row the pending-input drain recovers
                                 → S0 staging →
                                 context projection → model step → S1
                                 event_disposition_commit
```

There is no second main loop: a late transcript correction is an ordinary
durable input of the single subject, so the ongoing step stream already
delivers tool delegation, semantic waits and recovery. Tool use during a call
or a class is unchanged — media events are staged like any other input. The
durable `inbox_events` row commits inside the SAME ledger transaction as the
media fact (source `media/<provider>`, data = the complete serialized
InboxEvent), so a full or closed queue, a crash or a missed wake never loses
the input: the subject's shared pending drain delivers the SAME event id on
a later step, and `event_disposition_commit` reports committed consumption.

## Typed contract (`shenko-types/src/media.rs`)

- `MediaStreamDescriptor { provider, account, stream_id }` — the durable
  stream identity. `(provider, account, stream_id)` is unique and never
  reused after a terminal state.
- `MediaRevisionInput { revision_id, stream, segment, revision, text,
  partial, captured_at_ms, speaker, audio, provider_facts }` — one provider
  revision. `revision_id` is the durable idempotency identity.
- `MediaSpeakerClaim { speaker, person_id, confidence ∈ [0,1], corrected,
  source }` — an optional provider claim with confidence/unknown/correction.
  It is never an identity binding or authority (§15.2); `speaker: None`
  records explicit unknown.
- `MediaAudioRef::Sealed { object }` — CAS object ref. Admission validates the
  digest is exactly `blake3:` plus 64 lowercase hex digits **before** any path
  resolution, uses pinned cap-std capability-relative opens confined to this
  CAS even across escaping symlinks, and verifies
  the stored bytes hash to the declared digest at the declared length before
  the metadata commit (§41.2.2). A rewritten algorithm, a phantom digest or
  same-length corrupted content is rejected, never admitted.
  `MediaAudioRef::Provider(…)` records a provider-local locator verbatim —
  provenance, not archive.

- `MediaStreamOpen` / `MediaStreamEnd { state ∈ {cancelled, disconnected,
  ended}, reason }` — session scope and terminal state. Every terminal notice
  is an immutable fact keyed by `end_id` (`media_stream_ends`): a
  `disconnected` stream may later settle to `ended`/`cancelled` under a NEW
  end identity, and replaying `end_id` returns THAT notice's own event id and
  commit — never the stream's latest-state projection. The stream row's
  `end`/`state` columns are only the latest projection; `open_commit_ref`
  preserves the open admission's commit so an open replay keeps its own
  receipt even after the stream ended.

The pinned capability API's confined-open contract is documented in the
[cap-std 3.4.6 source](https://docs.rs/crate/cap-std/3.4.6/source/src/fs/dir.rs)
and its [sandboxing example](https://docs.rs/crate/cap-std/3.4.6).

## Dispositions

| disposition | meaning |
| --- | --- |
| `applied` | first admitted revision of the segment |
| `applied_supersedes` | applied over a non-final effective revision |
| `final_correction` | applied over an already-final revision — an explicit provider contradiction, rendered as a correction |
| `superseded` | arrived after a newer revision applied; retained raw, never effective |

Replays of `(stream, revision_id)` return the stored receipt, the SAME bound
`event_id`, `replayed: true` and `notify: null`. A different fact under a
taken identity or `(segment, revision)` slot is `media_revision_conflict`
with no persistent effect.

## Reads for reconciliation

- `Store::media_stream(descriptor)` — the committed stream row (state,
  implicit flag, end record).
- `Store::media_stream_ends(descriptor)` — every immutable terminal notice
  ordered by retained admission timestamp and end identity, each with its
  own bound event id and original commit (not a cross-ledger global clock).
- `Store::media_effective_segment(descriptor, segment)` — the current
  effective revision.
- `Store::media_revisions(descriptor, segment)` — the retained raw trail.
- `Store::media_transcript(descriptor)` — effective segments in order plus
  the honest `gaps` list (missing segment numbers between admitted ones).
- `Store::event_disposition_commit(agent, event_id)` — whether the subject
  committed consumption of the bound event; the reconciliation key after a
  crash between admission and inbox delivery.

`segment`/`revision` above the SQLite-integer range and other invalid input
are typed `media_invalid_input` rejections decided BEFORE the shared mutation
gate, so provider junk never closes the ledger writer.

## HTTP surface (`shenko-ui`, mounted only when ingress is wired)

- `PUT /api/media/ingress/stream` — body `MediaStreamOpen`.
- `PUT /api/media/ingress/revision` — body `MediaRevisionInput`.
- `PUT /api/media/ingress/stream/end` — body `MediaStreamEnd`.
- `GET /api/media/stream?provider=&account=&stream_id=` — stream + effective
  transcript + gaps.

Responses carry `{schema_version, receipt, delivery}` where `delivery` is
`queued | pending | rejected`: durable admission and live queue delivery are
reported separately and `pending` is never hidden — it means only that the
volatile wake did not land; the durable `inbox_events` row still delivers the
same event through the pending drain.

## Recovery replication

`media_streams`, `media_stream_ends` and `media_segment_revisions` are
registered replicated tables, so `export_cognition` carries stream identity
and latest state, every immutable terminal notice with its bound event id,
and every raw revision — and `collect_references` pulls each sealed audio
`ObjectRef` in `audio_json`/`data` into the verified CAS closure. The archive
schema is **4**: old receivers must reject rather than silently discard new
facts. Schema 2/3 inputs are admitted only after the same current closure
checks, including rejecting staged media whose admission tables are absent.
Import stamps only local bookkeeping `commit_seq`; `open_commit_ref` and
`admitted_commit_ref` retain the ORIGINAL ledger ID and sequence verbatim,
so repeated imports and later stream settlement cannot rewrite receipts.
Each stream and its pending inbox originals reconcile as one conflict unit;
a conflicting fact cannot leak a ready delivery or advance its baseline.
Missing/inconsistent fact↔delivery identities or audio closure lengths block
import before publication and therefore block a verified ACK.

## Native surface

`shenko_media::MediaIngressPlugin` publishes `MediaIngress`
(`shenko.media.ingress/v1`) on the shared Cordis root. Providers call
`admit_revision` / `admit_stream` / `end_stream`; `transcript`/`stream` serve
reconciliation reads. `bin/shenko` spawns it during bootstrap and wires the
same instance into the HTTP routes.

## Subject projection

`MediaRevision` / `MediaStream` inbox events stage as durable S0 facts and
render through `staged_media_text`: header carries provider/stream/segment/
revision/finality/capture-time/speaker-claim; superseded and correcting
revisions say what they replaced or what stayed effective — never a second
contradictory utterance. Streams that cancel or disconnect produce a
`StreamState` event preserving segment counts; missing segments stay gaps.

## What a real provider still needs (not delivered here)

- An audio capture device/path (SIP/WebRTC endpoint, LAN audio source, or a
  recorder) and a real ASR engine producing partial/final text.
- Optional speaker/voiceprint model producing `MediaSpeakerClaim`; sharing
  the LLM's GPU must be coordinated through the resource budget, §20.
- The provider's own upstream continuity/gap reporting and receipt
  reconciliation. `pending`/`rejected` no longer requires a provider resend:
  the subject recovers the committed row. If the transport lost an admission
  response, resend the same id and reconcile via `event_disposition_commit`.
- Recording/stop-control visibility and retention policy are provider
  configuration, recorded in `provider_facts`/`descriptor`.

## Bounded native consumer recipe (for the integrator)

All inside the dev container, under the shared heavy-work lock:

1. `cargo test -p shenko-store --test media_transcript` — the durable
   behavior scenarios of this slice (8 tests / 9 independent cases, counting
   open replay after terminal settlement separately from terminal-notice
   replay; no parameter-table or helper expansion hides extra scenarios).
2. `cargo test -p shenko-subject` — the projection/staging change must not
   regress existing S0/S1 and recovery tests.
3. `cargo test -p shenko-store --test cognition_replication` — regression of
   `local_seq` handling (every replicated table) plus the new media
   registrations. Then one bounded manual check: admit a stream open, one
   revision with sealed audio and a disconnect→settle pair; export
   cognition; import into a fresh store; verify `media_stream`/
   `media_stream_ends`/`media_transcript`/`media_revisions` read
   identically, the audio object arrived in the closure, and replays of
   `open_id`/`end_id`/`revision_id` return their ORIGINAL event ids and
   commits on the receiver. Reimport a newer terminal snapshot after the
   initial open-only snapshot and confirm the open receipt is unchanged.
4. Native end-to-end smoke (manual): start `bin/shenko` with a real data
   dir, then replay this exact sequence against the UI port:

   ```
   PUT /api/media/ingress/stream    {"open_id":"o1","stream":{"provider":"test","account":"a","stream_id":"s1"},"descriptor":{"kind":"call"}}
   PUT /api/media/ingress/revision  {"revision_id":"r1","stream":{…same…},"segment":1,"revision":1,"text":"我们来讲","partial":true}
   PUT /api/media/ingress/revision  {"revision_id":"r2","stream":{…same…},"segment":1,"revision":2,"text":"我们来讲转写","partial":false}
   PUT /api/media/ingress/revision  {"revision_id":"r1", …byte-identical resend…}   # replayed:true, same event_id
   PUT /api/media/ingress/stream/end {"end_id":"e1","stream":{…same…},"state":"disconnected"}
   GET /api/media/stream?provider=test&account=a&stream_id=s1        # transcript + gaps
   ```

   Expected: each admission returns a commit_ref-bearing receipt; the subject
   inbox receives one event per new fact; a replay adds none; the transcript
   reports the final effective text and honest gaps; after restart, the same
   reads return the same facts (reopen/replay durability). With the subject
   inbox stopped or filled before the revision PUT, `delivery` reports
   `pending`/`rejected` while the durable `inbox_events` row still exists —
   after reopen it reaches the subject through the pending drain with the
   SAME event id, and `event_disposition_commit` resolves it.

5. With a running subject, confirm the bound `event_id` resolves through
   `Store::event_disposition_commit` after the step that consumed it — the
   same reconciliation key NapCat ingress uses.

Use only sandbox data under the persistent project tree. Additional bounded
repair probes, not new permanent cases: segment and revision overflow
separately; changed terminal state under the SAME `end_id`; malformed digest
suffix/absolute path and rewritten algorithm before any lookup; same-length
CAS corruption; an escaping CAS symlink; an archive with a media fact,
delivery identity or audio payload removed; and a stream conflict that must
not publish its inbox originals. These are unexecuted acceptance work.
