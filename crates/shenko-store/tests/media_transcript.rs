//! Durable transcript revision ingress — SPEC §15.2, §20.
//! Nine independent scenarios, registered in eight tests: atomic admission;
//! conflicting revision identity; partial→final; final correction;
//! disconnect→end preserving terminal receipts; open replay after end;
//! verified audio; noncanonical audio rejection; numeric-range rejection.
//! The open-replay scenario is counted separately despite sharing the
//! terminal journey. No ASR/model/audio device is exercised or simulated.

use shenko_store::store::Store;
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::media::*;
use std::sync::Arc;

fn fixture() -> (Arc<Store>, EntityId, MediaStreamDescriptor) {
    let dir = std::env::temp_dir().join(format!("shenko-media-test-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(dir.join("test.db"), dir.join("cas")).unwrap());
    let stream = MediaStreamDescriptor {
        provider: "test-asr".into(),
        account: "acct-1".into(),
        stream_id: "call-9".into(),
    };
    (store, EntityId::new(), stream)
}

fn revision(
    stream: &MediaStreamDescriptor,
    id: &str,
    seg: u64,
    rev: u64,
    text: &str,
    partial: bool,
) -> MediaRevisionInput {
    MediaRevisionInput {
        revision_id: id.into(),
        stream: stream.clone(),
        segment: seg,
        revision: rev,
        text: text.into(),
        partial,
        captured_at_ms: Some(1_700_000_000_000 + rev as i64),
        speaker: Some(MediaSpeakerClaim {
            speaker: Some("speaker-2".into()),
            person_id: Some(EntityId::new()),
            confidence: Some(0.42),
            corrected: rev > 1,
            source: Some("voiceprint-v3".into()),
        }),
        audio: Some(MediaAudioRef::Provider(ProviderObjectRef {
            locator: format!("provider://audio/{id}"),
            digest: Some(format!("sha256:{id}")),
            media_type: Some("audio/opus".into()),
        })),
        provider_facts: serde_json::Map::new(),
    }
}

fn end_notice(
    stream: &MediaStreamDescriptor,
    id: &str,
    state: MediaStreamState,
    at: i64,
) -> MediaStreamEnd {
    MediaStreamEnd {
        end_id: id.into(),
        stream: stream.clone(),
        state,
        ended_at_ms: Some(at),
        reason: Some("provider report".into()),
    }
}

/// 1. One admission commits the raw revision, the effective observation, the
/// durable inbox original (same event id, `media/<provider>` source, complete
/// InboxEvent payload) and the implicit stream row in one commit; provider
/// provenance and speaker claims stay verbatim.
#[test]
fn admission_stores_raw_effective_and_durable_event_atomically() {
    let dir = std::env::temp_dir().join(format!("shenko-media-test-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("test.db");
    let store = Arc::new(Store::open(db.clone(), dir.join("cas")).unwrap());
    let agent = EntityId::new();
    let stream = MediaStreamDescriptor {
        provider: "test-asr".into(),
        account: "acct-1".into(),
        stream_id: "call-9".into(),
    };
    let input = revision(&stream, "r1", 3, 1, "hello world", false);
    let admission = store.admit_media_revision(agent, input).unwrap();

    assert!(!admission.receipt.replayed);
    assert_eq!(
        admission.receipt.disposition,
        MediaRevisionDisposition::Applied
    );
    let event = admission.notify.expect("a new fact notifies the subject");
    let MediaIngressEvent::Revision {
        record,
        event_id,
        prior_effective,
        ..
    } = &event
    else {
        panic!("revision admission notifies a Revision event");
    };
    assert_eq!(*event_id, admission.receipt.event_id);
    assert_eq!(record.text, "hello world");
    assert!(prior_effective.is_none());

    // The bound subject input is durable with the fact itself: recoverable
    // through the pending-inbox lane without any volatile queue send.
    let (source, consumed, data): (String, i64, String) =
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT source, consumed, data FROM inbox_events WHERE event_id=?1",
                [event_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(source, "media/test-asr");
    assert_eq!(consumed, 0);
    let durable: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(
        durable["event_id"].as_str().unwrap(),
        event_id.to_string()
    );
    assert_eq!(durable["kind"].as_str().unwrap(), "media_revision");
    assert_eq!(durable["source"].as_str().unwrap(), "media/test-asr");
    assert!(durable["created_at"].is_string());
    assert_eq!(
        durable["payload"],
        serde_json::to_value(&event).unwrap(),
        "the durable original carries the complete bound event"
    );

    let stream_row = store.media_stream(&stream).unwrap().unwrap();
    assert!(stream_row.implicit);
    assert_eq!(stream_row.state, MediaStreamState::Open);

    let effective = store.media_effective_segment(&stream, 3).unwrap().unwrap();
    assert_eq!(effective.revision_id, "r1");
    assert!(!effective.partial);
    let claim = effective.speaker.as_ref().unwrap();
    assert_eq!(claim.speaker.as_deref(), Some("speaker-2"));
    assert_eq!(claim.confidence, Some(0.42));
    match &effective.audio {
        Some(MediaAudioRef::Provider(provider)) => {
            assert_eq!(provider.locator, "provider://audio/r1");
        }
        other => panic!("provider audio ref recorded verbatim, got {other:?}"),
    }
    let transcript = store.media_transcript(&stream).unwrap().unwrap();
    assert_eq!(transcript.segments.len(), 1);
    assert!(transcript.gaps.is_empty());
}

/// 2. A different fact under the same revision identity is a typed conflict;
/// the original row is never overwritten.
#[test]
fn conflicting_revision_identity_is_typed_conflict() {
    let (store, agent, stream) = fixture();
    store
        .admit_media_revision(agent, revision(&stream, "r1", 1, 1, "original", false))
        .unwrap();
    let mut different = revision(&stream, "r1", 1, 1, "changed", false);
    different.captured_at_ms = Some(42);

    let error = store.admit_media_revision(agent, different).unwrap_err();
    match error {
        ShenkoError::Protocol(protocol) => {
            assert_eq!(protocol.code, "media_revision_conflict");
        }
        other => panic!("expected media_revision_conflict, got {other}"),
    }
    let revisions = store.media_revisions(&stream, 1).unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].text, "original");
}

/// 3. A partial followed by a final revision applies normally: the final text
/// becomes effective and the prior raw revision keeps its superseded_by link.
#[test]
fn partial_then_final_replaces_effective_and_keeps_raw() {
    let (store, agent, stream) = fixture();
    store
        .admit_media_revision(agent, revision(&stream, "r1", 2, 1, "helo", true))
        .unwrap();
    let final_rev = store
        .admit_media_revision(agent, revision(&stream, "r2", 2, 2, "hello", false))
        .unwrap();

    assert_eq!(
        final_rev.receipt.disposition,
        MediaRevisionDisposition::AppliedSupersedes
    );
    let Some(MediaIngressEvent::Revision {
        prior_effective, ..
    }) = final_rev.notify
    else {
        panic!("revision admission notifies a Revision event");
    };
    let prior = prior_effective.expect("superseding reports what it replaced");
    assert_eq!(prior.revision, 1);
    assert_eq!(prior.text, "helo");

    let effective = store.media_effective_segment(&stream, 2).unwrap().unwrap();
    assert_eq!(effective.revision_id, "r2");
    assert!(!effective.partial);
    let revisions = store.media_revisions(&stream, 2).unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].superseded_by_revision, Some(2));
    assert_eq!(revisions[1].superseded_by_revision, None);
}

/// 4. A revision arriving after the segment finalized is an explicit final
/// correction: it applies, and both the correction and the prior final remain
/// readable as contradictory raw facts.
#[test]
fn post_final_revision_is_explicit_final_correction() {
    let (store, agent, stream) = fixture();
    store
        .admit_media_revision(agent, revision(&stream, "r1", 5, 1, "final one", false))
        .unwrap();
    let correction = store
        .admit_media_revision(agent, revision(&stream, "r2", 5, 2, "final two", false))
        .unwrap();

    assert_eq!(
        correction.receipt.disposition,
        MediaRevisionDisposition::FinalCorrection
    );
    let effective = store.media_effective_segment(&stream, 5).unwrap().unwrap();
    assert_eq!(effective.text, "final two");
    let revisions = store.media_revisions(&stream, 5).unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].text, "final one");
    assert_eq!(revisions[0].superseded_by_revision, Some(2));
    let transcript = store.media_transcript(&stream).unwrap().unwrap();
    assert!(transcript.segments[0].had_final_correction);
}

/// 5–6. Terminal notices are immutable facts keyed by end identity: a
/// disconnect settling into `ended` keeps BOTH original notices, each replay
/// returns its own event id and commit, the open replay keeps its own
/// original receipt (never the terminal commit), and the stream row carries
/// only the latest state.
#[test]
fn disconnect_settlement_keeps_every_terminal_notice_and_open_receipt() {
    let (store, agent, stream) = fixture();
    let opened = store
        .admit_media_stream(
            agent,
            MediaStreamOpen {
                open_id: "open-1".into(),
                stream: stream.clone(),
                descriptor: serde_json::Map::new(),
            },
        )
        .unwrap();
    store
        .admit_media_revision(agent, revision(&stream, "r1", 1, 1, "first", false))
        .unwrap();
    store
        .admit_media_revision(agent, revision(&stream, "r3", 3, 1, "third", false))
        .unwrap();

    let disconnect = end_notice(&stream, "end-d", MediaStreamState::Disconnected, 1_700_000_100_000);
    let disconnect_admission = store.end_media_stream(agent, disconnect.clone()).unwrap();
    assert_eq!(
        disconnect_admission.receipt.state,
        MediaStreamState::Disconnected
    );
    let ended = end_notice(&stream, "end-e", MediaStreamState::Ended, 1_700_000_200_000);
    let ended_admission = store.end_media_stream(agent, ended.clone()).unwrap();
    assert_eq!(ended_admission.receipt.state, MediaStreamState::Ended);
    let Some(MediaIngressEvent::StreamState { segments, .. }) = ended_admission.notify
    else {
        panic!("terminal admission notifies a StreamState event");
    };
    assert_eq!(segments, 2);
    assert_ne!(
        disconnect_admission.receipt.event_id, ended_admission.receipt.event_id,
        "each terminal notice binds its own durable event"
    );

    // Replays return EACH notice's own receipt — never the latest state.
    let d_replay = store.end_media_stream(agent, disconnect).unwrap();
    assert!(d_replay.receipt.replayed);
    assert_eq!(
        d_replay.receipt.event_id, disconnect_admission.receipt.event_id,
        "disconnect replay returns its original bound event"
    );
    assert_eq!(
        d_replay.receipt.commit_ref, disconnect_admission.receipt.commit_ref,
        "disconnect replay returns its original commit, not the settlement's"
    );
    assert_eq!(d_replay.receipt.state, MediaStreamState::Disconnected);
    let e_replay = store.end_media_stream(agent, ended).unwrap();
    assert!(e_replay.receipt.replayed);
    assert_eq!(
        e_replay.receipt.event_id, ended_admission.receipt.event_id
    );

    // The open replay still returns the OPEN admission's own receipt.
    let open_replay = store
        .admit_media_stream(
            agent,
            MediaStreamOpen {
                open_id: "open-1".into(),
                stream: stream.clone(),
                descriptor: serde_json::Map::new(),
            },
        )
        .unwrap();
    assert!(open_replay.receipt.replayed);
    assert_eq!(
        open_replay.receipt.commit_ref, opened.receipt.commit_ref,
        "open replay keeps the open commit, not the terminal one"
    );
    assert_eq!(
        open_replay.receipt.event_id, opened.receipt.event_id
    );

    // The durable notices are separate from the stream's latest state.
    let ends = store.media_stream_ends(&stream).unwrap();
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[0].end_id, "end-d");
    assert_eq!(ends[0].state, MediaStreamState::Disconnected);
    assert_eq!(ends[0].event_id, disconnect_admission.receipt.event_id.unwrap());
    assert_eq!(ends[1].end_id, "end-e");
    assert_eq!(ends[1].state, MediaStreamState::Ended);

    let transcript = store.media_transcript(&stream).unwrap().unwrap();
    assert_eq!(transcript.stream.state, MediaStreamState::Ended);
    assert_eq!(transcript.gaps, vec![(2, 2)]);
    assert_eq!(transcript.segments.len(), 2);
    assert_eq!(transcript.segments[1].effective.text, "third");
    assert_eq!(
        transcript.stream.end.as_ref().unwrap().end_id,
        "end-e",
        "the stream row projects only the latest terminal state"
    );
}

/// 7. Real sealed audio is admitted only after the bytes verify in this CAS.
#[test]
fn sealed_audio_is_verified_before_mutation() {
    let (store, agent, stream) = fixture();
    let bytes = b"real audio bytes";
    let sealed = store
        .cas
        .seal_object(bytes, "audio/opus", "media-audio", "raw", 1)
        .unwrap();

    let mut with_audio = revision(&stream, "r1", 1, 1, "verified audio", false);
    with_audio.audio = Some(MediaAudioRef::Sealed {
        object: sealed.object.clone(),
    });
    store.admit_media_revision(agent, with_audio).unwrap();
    let effective = store.media_effective_segment(&stream, 1).unwrap().unwrap();
    assert!(matches!(
        effective.audio,
        Some(MediaAudioRef::Sealed { .. })
    ));
}

/// 8. Noncanonical audio identity is rejected before any CAS lookup.
#[test]
fn noncanonical_sealed_audio_identity_is_rejected() {
    let (store, agent, stream) = fixture();
    let sealed = store.cas.seal_object(b"audio", "audio/opus", "media-audio", "raw", 1).unwrap();
    // A real object's digest rewritten to another algorithm is not a
    // canonical supported digest: rejected as invalid input BEFORE the CAS
    // path is ever resolved.
    let mut rewritten = sealed.object.clone();
    rewritten.digest = shenko_types::object::Digest(
        rewritten.digest.0.replacen("blake3:", "sha256:", 1),
    );
    let mut foreign = revision(&stream, "r2", 2, 1, "foreign digest", false);
    foreign.audio = Some(MediaAudioRef::Sealed { object: rewritten });
    match store.admit_media_revision(agent, foreign).unwrap_err() {
        ShenkoError::Protocol(protocol) => {
            assert_eq!(protocol.code, "media_invalid_input");
        }
        other => panic!("expected media_invalid_input, got {other}"),
    }
    assert!(store.media_effective_segment(&stream, 2).unwrap().is_none());
}

/// 9. Out-of-range input is not a storage failure and cannot close the writer.
#[test]
fn numeric_range_is_rejected_before_mutation_gate() {
    let (store, agent, stream) = fixture();
    // Numeric identities outside the supported persistent range fail as
    // typed input errors BEFORE the mutation gate; the shared ledger writer
    // stays open, so the next real admission still commits.
    let mut overflow = revision(&stream, "r3", 3, 1, "too big", false);
    overflow.revision = i64::MAX as u64 + 1;
    match store.admit_media_revision(agent, overflow).unwrap_err() {
        ShenkoError::Protocol(protocol) => {
            assert_eq!(protocol.code, "media_invalid_input");
        }
        other => panic!("expected media_invalid_input, got {other}"),
    }
    let after = store
        .admit_media_revision(agent, revision(&stream, "r4", 4, 1, "still open", false))
        .unwrap();
    assert!(!after.receipt.replayed);
}
