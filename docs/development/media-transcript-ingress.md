# Continuous-media transcript revision ingress

SPEC: §15.2, §20, §41.2/§41.3. Work package C16 / issue #21, case A16.

This document describes the durable ingress contract **for real transcript
providers**. It is not an ASR implementation and does not claim one exists:
`text` is always real provider output, the kernel never synthesizes it, and a
synthetic transcript is never rendered as recognition. Real ASR/audio device
prerequisites are listed at the end and remain outside this slice.

## Path

```
provider ─ typed input ─▶ MediaIngress (native service or /api/media/*)
                          │
                          ├─ Store.admit_media_revision / admit_media_stream /
                          │  end_media_stream        (one ledger commit:
                          │   raw fact + effective observation + stream row)
                          │
                          └─ bound InboxEvent (event_id from the receipt)
                              └─ same subject inbox → S0 staging →
                                 context projection → model step → S1
                                 event_disposition_commit
```

There is no second main loop: a late transcript correction is an ordinary
durable input of the single subject, so the ongoing step stream already
delivers tool delegation, semantic waits and recovery. Tool use during a call
or a class is unchanged — media events are staged like any other input.

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
- `MediaAudioRef::Sealed { object }` — CAS object ref. Admission verifies the
  object exists with the declared digest and size **before** the metadata
  commit (§41.2.2). `MediaAudioRef::Provider(…)` records a provider-local
  locator verbatim — provenance, not archive.
- `MediaStreamOpen` / `MediaStreamEnd { state ∈ {cancelled, disconnected,
  ended}, reason }` — session scope and terminal state. A `disconnected`
  stream may later settle to `ended`/`cancelled` as a new explicit fact;
  `ended`/`cancelled` are immutable.

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
- `Store::media_effective_segment(descriptor, segment)` — the current
  effective revision.
- `Store::media_revisions(descriptor, segment)` — the retained raw trail.
- `Store::media_transcript(descriptor)` — effective segments in order plus
  the honest `gaps` list (missing segment numbers between admitted ones).
- `Store::event_disposition_commit(agent, event_id)` — whether the subject
  committed consumption of the bound event; the reconciliation key after a
  crash between admission and inbox delivery.

## HTTP surface (`shenko-ui`, mounted only when ingress is wired)

- `PUT /api/media/ingress/stream` — body `MediaStreamOpen`.
- `PUT /api/media/ingress/revision` — body `MediaRevisionInput`.
- `PUT /api/media/ingress/stream/end` — body `MediaStreamEnd`.
- `GET /api/media/stream?provider=&account=&stream_id=` — stream + effective
  transcript + gaps.

Responses carry `{schema_version, receipt, delivery}` where `delivery` is
`queued | pending | rejected`: durable admission and live queue delivery are
reported separately and `pending` is never hidden.

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
- The provider's own retry/reconcile loop: on `pending`/`rejected` delivery
  or a crash, resend the same `revision_id` and reconcile via
  `event_disposition_commit`.
- Recording/stop-control visibility and retention policy are provider
  configuration, recorded in `provider_facts`/`descriptor`.

## Bounded native consumer recipe (for the integrator)

All inside the dev container, under the shared heavy-work lock:

1. `cargo test -p shenko-store --test media_transcript` — the 9 durable
   behavior scenarios of this slice.
2. `cargo test -p shenko-subject` — the projection/staging change must not
   regress existing S0/S1 and recovery tests.
3. Native end-to-end smoke (manual): start `bin/shenko` with a real data
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
   reads return the same facts (reopen/replay durability).

4. With a running subject, confirm the bound `event_id` resolves through
   `Store::event_disposition_commit` after the step that consumed it — the
   same reconciliation key NapCat ingress uses.
