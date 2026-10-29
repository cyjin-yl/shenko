//! Durable transcript revision ingress — SPEC §15.2, §20.
//! Each test is one independent consumer-behavior scenario of the real store:
//! raw facts + effective observation in one commit, idempotent replays,
//! explicit contradictions, honest terminal state and sealed-audio binding.
//! No ASR/model/audio device is exercised or simulated.

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
        speaker: None,
        audio: Some(MediaAudioRef::Provider(ProviderObjectRef {
            locator: format!("provider://audio/{id}"),
            digest: Some(format!("sha256:{id}")),
            media_type: Some("audio/opus".into()),
        })),
        provider_facts: serde_json::Map::new(),
    }
}

/// 1. One admission commits the raw revision and makes it effective, the
/// stream row is created implicitly, and provider provenance is kept verbatim.
#[test]
fn admission_stores_raw_and_effective_atomically() {
    let (store, agent, stream) = fixture();
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

    let stream_row = store.media_stream(&stream).unwrap().unwrap();
    assert!(stream_row.implicit);
    assert_eq!(stream_row.state, MediaStreamState::Open);

    let effective = store.media_effective_segment(&stream, 3).unwrap().unwrap();
    assert_eq!(effective.revision_id, "r1");
    assert!(!effective.partial);
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

/// 2. Replaying the same durable identity returns the original receipt and
/// bound event id, and commits no second raw fact.
#[test]
fn idempotent_replay_returns_original_receipt() {
    let (store, agent, stream) = fixture();
    let input = revision(&stream, "r1", 1, 1, "你好", false);
    let first = store.admit_media_revision(agent, input.clone()).unwrap();
    let second = store.admit_media_revision(agent, input).unwrap();

    assert!(second.receipt.replayed);
    assert_eq!(second.receipt.event_id, first.receipt.event_id);
    assert_eq!(second.receipt.commit_ref, first.receipt.commit_ref);
    assert!(second.notify.is_none());
    assert_eq!(store.media_revisions(&stream, 1).unwrap().len(), 1);
}

/// 3. A different fact under the same revision identity is a typed conflict;
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

/// 4. A partial followed by a final revision applies normally: the final text
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

/// 5. An out-of-order revision stays retained as an explicit superseded fact
/// and never retroactively replaces the newer effective text.
#[test]
fn out_of_order_revision_stays_superseded_raw_fact() {
    let (store, agent, stream) = fixture();
    store
        .admit_media_revision(agent, revision(&stream, "r2", 4, 2, "newer", false))
        .unwrap();
    let late = store
        .admit_media_revision(agent, revision(&stream, "r1", 4, 1, "older", true))
        .unwrap();

    assert_eq!(
        late.receipt.disposition,
        MediaRevisionDisposition::Superseded
    );
    let Some(MediaIngressEvent::Revision {
        prior_effective, ..
    }) = late.notify
    else {
        panic!("revision admission notifies a Revision event");
    };
    assert_eq!(prior_effective.unwrap().revision_id, "r2");

    let effective = store.media_effective_segment(&stream, 4).unwrap().unwrap();
    assert_eq!(effective.revision_id, "r2");
    assert_eq!(effective.text, "newer");
    let revisions = store.media_revisions(&stream, 4).unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(
        revisions[0].disposition,
        MediaRevisionDisposition::Superseded
    );
}

/// 6. A revision arriving after the segment finalized is an explicit final
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

/// 7. Cancelling a stream preserves committed segments and the honest gap
/// list, notifies once, replays idempotently, and rejects later revisions.
#[test]
fn cancellation_preserves_final_state_gaps_and_blocks_revisions() {
    let (store, agent, stream) = fixture();
    store
        .admit_media_revision(agent, revision(&stream, "r1", 1, 1, "first", false))
        .unwrap();
    store
        .admit_media_revision(agent, revision(&stream, "r3", 3, 1, "third", false))
        .unwrap();

    let end = MediaStreamEnd {
        end_id: "end-1".into(),
        stream: stream.clone(),
        state: MediaStreamState::Cancelled,
        ended_at_ms: Some(1_700_000_100_000),
        reason: Some("provider hangup".into()),
    };
    let ended = store.end_media_stream(agent, end.clone()).unwrap();
    assert!(!ended.receipt.replayed);
    let Some(MediaIngressEvent::StreamState { segments, .. }) = ended.notify else {
        panic!("terminal admission notifies a StreamState event");
    };
    assert_eq!(segments, 2);

    let replay = store.end_media_stream(agent, end).unwrap();
    assert!(replay.receipt.replayed);
    assert!(replay.notify.is_none());
    assert_eq!(replay.receipt.event_id, ended.receipt.event_id);

    let transcript = store.media_transcript(&stream).unwrap().unwrap();
    assert_eq!(transcript.stream.state, MediaStreamState::Cancelled);
    assert_eq!(transcript.gaps, vec![(2, 2)]);
    assert_eq!(transcript.segments.len(), 2);
    assert_eq!(transcript.segments[1].effective.text, "third");

    let error = store
        .admit_media_revision(agent, revision(&stream, "r4", 4, 1, "late", true))
        .unwrap_err();
    match error {
        ShenkoError::Protocol(protocol) => {
            assert_eq!(protocol.code, "media_stream_closed");
        }
        other => panic!("expected media_stream_closed, got {other}"),
    }
}

/// 8. Speaker claims are recorded with confidence/correction/unknown and
/// never become more than a claim; validation rejects out-of-range scores.
#[test]
fn speaker_claim_keeps_confidence_unknown_and_correction() {
    let (store, agent, stream) = fixture();
    let mut claimed = revision(&stream, "r1", 1, 1, "recognized", true);
    claimed.speaker = Some(MediaSpeakerClaim {
        speaker: Some("speaker-2".into()),
        person_id: Some(EntityId::new()),
        confidence: Some(0.42),
        corrected: true,
        source: Some("voiceprint-v3".into()),
    });
    store.admit_media_revision(agent, claimed).unwrap();
    let mut unknown = revision(&stream, "r2", 2, 1, "unattributed", true);
    unknown.speaker = Some(MediaSpeakerClaim {
        speaker: None,
        person_id: None,
        confidence: None,
        corrected: false,
        source: None,
    });
    store.admit_media_revision(agent, unknown).unwrap();

    let revisions = store.media_revisions(&stream, 1).unwrap();
    let claim = revisions[0].speaker.as_ref().unwrap();
    assert_eq!(claim.speaker.as_deref(), Some("speaker-2"));
    assert_eq!(claim.confidence, Some(0.42));
    assert!(claim.corrected);
    let unknown_claim = store.media_revisions(&stream, 2).unwrap()[0]
        .speaker
        .clone()
        .unwrap();
    assert!(unknown_claim.speaker.is_none() && unknown_claim.confidence.is_none());

    let mut bad = revision(&stream, "r3", 3, 1, "bad score", true);
    bad.speaker = Some(MediaSpeakerClaim {
        speaker: Some("s".into()),
        person_id: None,
        confidence: Some(1.5),
        corrected: false,
        source: None,
    });
    match store.admit_media_revision(agent, bad).unwrap_err() {
        ShenkoError::Protocol(protocol) => {
            assert_eq!(protocol.code, "media_invalid_input");
        }
        other => panic!("expected media_invalid_input, got {other}"),
    }
}

/// 9. A `sealed` audio ref must already exist in this ledger's CAS with the
/// declared size; provider handles admit without CAS presence.
#[test]
fn sealed_audio_requires_committed_object() {
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

    let mut ghost = sealed.object.clone();
    ghost.bytes += 1;
    let mut missing = revision(&stream, "r2", 2, 1, "phantom audio", false);
    missing.audio = Some(MediaAudioRef::Sealed { object: ghost });
    match store.admit_media_revision(agent, missing).unwrap_err() {
        ShenkoError::Protocol(protocol) => {
            assert_eq!(protocol.code, "media_audio_object_missing");
        }
        other => panic!("expected media_audio_object_missing, got {other}"),
    }
    assert!(store.media_effective_segment(&stream, 2).unwrap().is_none());
}
