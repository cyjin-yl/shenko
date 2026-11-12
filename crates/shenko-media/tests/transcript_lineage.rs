//! Ordered / corrected transcript lineage of a live media session — docs/specs/media.md#spec-20
//! (输入事件包含时间戳、片段序号、partial/final 修订、说话人候选、转写/原音频
//! 引用和会话 scope；partial 修订不能产生重复承诺；最终文本与原音频对应可追溯).
//!
//! Six independent scenarios driven through the real path — `MediaRuntime`
//! over a scripted realtime provider, the real `MediaIngress` and the real
//! ledger/CAS (never a mocked store): provider-owned segment indices survive
//! a correction instead of fabricating a second utterance while the original
//! sealed bytes stay attached; a late partial never displaces a final; a
//! declared scope with `store_audio: false` stores no byte; a voiceprint claim
//! is computed on the ORIGINAL sealed bytes and admitted on the same segment
//! with the retained provider text; a lost transport records a durable
//! `disconnected` terminal fact; and the derived segment boundary an
//! index-less provider still gets is unchanged. Provider bindings are refused
//! when duplicated, driven under an unknown protocol or declared at sample
//! rate 0.
//!
//! No ASR engine, model, audio device or external call is exercised: the
//! provider-side texts below are the scripted provider's claims, admitted
//! verbatim exactly as a real engine's would be.

use std::sync::Arc;

use async_trait::async_trait;
use shenko_media::provider::{
    MediaEventSink, RealtimeSession, RealtimeSessionProvider, SpeakerIdentity,
};
use shenko_media::runtime::{MediaProviders, MediaRuntime};
use shenko_media::{MediaIngress, SubjectDelivery};
use shenko_store::store::Store;
use shenko_types::inbox::{InboxEvent, InboxEventKind};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::media::*;
use shenko_types::object::ObjectRef;

fn stream() -> MediaStreamDescriptor {
    MediaStreamDescriptor {
        provider: "test-asr".into(),
        account: "acct-1".into(),
        stream_id: "class-1".into(),
    }
}

fn realtime_binding() -> MediaProviderBinding {
    MediaProviderBinding {
        provider: "test-asr".into(),
        account: "acct-1".into(),
        capability: MediaProviderCapability::RealtimeSession,
        endpoint: "ws://127.0.0.1:0".into(),
        credential_env: None,
        model: None,
        sample_rate: 16000,
        options: serde_json::Map::new(),
    }
}

fn speaker_binding() -> MediaProviderBinding {
    MediaProviderBinding {
        provider: "test-sv".into(),
        account: "acct-1".into(),
        capability: MediaProviderCapability::SpeakerIdentity,
        endpoint: "ws://127.0.0.1:1".into(),
        credential_env: None,
        model: Some("cam++".into()),
        sample_rate: 16000,
        options: serde_json::Map::new(),
    }
}

fn scope(store_audio: bool) -> MediaRecordingScope {
    MediaRecordingScope {
        kind: "classroom".into(),
        store_audio,
        retention: "session".into(),
        provider_facts: serde_json::Map::new(),
    }
}

fn event(
    text: &str,
    partial: bool,
    segment: Option<u64>,
    speaker: Option<MediaSpeakerClaim>,
) -> MediaTranscriptEvent {
    MediaTranscriptEvent {
        text: text.to_owned(),
        partial,
        segment,
        speaker,
        captured_at_ms: None,
        provider_facts: serde_json::Map::new(),
    }
}

fn claim(speaker: &str, confidence: f64) -> MediaSpeakerClaim {
    MediaSpeakerClaim {
        speaker: Some(speaker.to_owned()),
        person_id: None,
        confidence: Some(confidence),
        corrected: false,
        source: Some("test-asr:v1".into()),
    }
}

/// Scripted realtime session: keeps the sink the runtime wired at open, so a
/// test reports provider output exactly where a real engine task would.
struct FakeSession {
    descriptor: MediaStreamDescriptor,
    sink: Arc<dyn MediaEventSink>,
}

#[async_trait]
impl RealtimeSession for FakeSession {
    fn descriptor(&self) -> MediaStreamDescriptor {
        self.descriptor.clone()
    }

    async fn push_audio(&self, _chunk: &[u8]) -> Result<(), ShenkoError> {
        Ok(())
    }

    async fn finish(&self) -> Result<(), ShenkoError> {
        Ok(())
    }

    async fn close(&self) -> Result<(), ShenkoError> {
        Ok(())
    }
}

struct FakeProvider {
    binding: MediaProviderBinding,
    sessions: tokio::sync::Mutex<Vec<Arc<FakeSession>>>,
}

#[async_trait]
impl RealtimeSessionProvider for FakeProvider {
    fn binding(&self) -> &MediaProviderBinding {
        &self.binding
    }

    async fn open(
        &self,
        request: MediaSessionRequest,
        sink: Arc<dyn MediaEventSink>,
    ) -> Result<Arc<dyn RealtimeSession>, ShenkoError> {
        let session = Arc::new(FakeSession {
            descriptor: request.stream.clone(),
            sink,
        });
        self.sessions.lock().await.push(session.clone());
        Ok(session)
    }
}

/// Scripted voiceprint engine: it records the digest it was asked about and
/// answers with one engine claim, never with an identity.
struct FakeSpeaker {
    binding: MediaProviderBinding,
    calls: tokio::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl SpeakerIdentity for FakeSpeaker {
    fn binding(&self) -> &MediaProviderBinding {
        &self.binding
    }

    async fn identify(&self, audio: &ObjectRef) -> Result<MediaSpeakerClaim, ShenkoError> {
        self.calls.lock().await.push(audio.digest.to_string());
        Ok(MediaSpeakerClaim {
            speaker: Some("spk-teacher".into()),
            person_id: None,
            confidence: Some(0.82),
            corrected: false,
            source: Some("test-sv:v1".into()),
        })
    }
}

struct Harness {
    store: Arc<Store>,
    runtime: Arc<MediaRuntime>,
    provider: Arc<FakeProvider>,
    speaker: Arc<FakeSpeaker>,
    stream: MediaStreamDescriptor,
    inbox: tokio::sync::mpsc::Receiver<InboxEvent>,
}

impl Harness {
    async fn emit(&self, event: MediaTranscriptEvent) {
        let session = self.provider.sessions.lock().await[0].clone();
        session
            .sink
            .on_transcript(&self.stream, event)
            .await;
    }

    async fn provider_finished(&self, reason: Option<String>) {
        let session = self.provider.sessions.lock().await[0].clone();
        session
            .sink
            .on_session_finished(&self.stream, reason)
            .await;
    }

    fn transcript(&self) -> MediaTranscript {
        self.store
            .media_transcript(&self.stream)
            .unwrap()
            .expect("admitted stream")
    }

    fn effective_audio(&self, segment: u64) -> Option<MediaAudioRef> {
        self.store
            .media_effective_segment(&self.stream, segment)
            .unwrap()
            .expect("admitted segment")
            .audio
    }

    fn captured_pcm(&self, segment: u64) -> Vec<u8> {
        match self.effective_audio(segment) {
            Some(MediaAudioRef::Sealed { object }) => {
                let wav = self.store.cas.read_object(&object.digest).unwrap();
                assert_eq!(&wav[..4], &b"RIFF"[..], "sealed audio is a canonical WAVE");
                wav[44..].to_vec()
            }
            other => panic!("segment {segment} must carry sealed original audio, got {other:?}"),
        }
    }

    fn inbox_events(&mut self) -> Vec<InboxEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.inbox.try_recv() {
            events.push(event);
        }
        events
    }
}

/// Open a realtime session over the scripted provider. The open notice and
/// the recording notice are drained so each test starts from the revisions it
/// reports.
async fn open(recording: Option<MediaRecordingScope>) -> Harness {
    let dir = std::env::temp_dir().join(format!("shenko-media-lineage-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(dir.join("test.db"), dir.join("cas")).unwrap());
    let (sender, mut inbox) = tokio::sync::mpsc::channel(64);
    let agent_id = EntityId::new();
    let ingress = Arc::new(MediaIngress::new(Arc::clone(&store), agent_id, sender));
    let provider = Arc::new(FakeProvider {
        binding: realtime_binding(),
        sessions: tokio::sync::Mutex::new(Vec::new()),
    });
    let speaker = Arc::new(FakeSpeaker {
        binding: speaker_binding(),
        calls: tokio::sync::Mutex::new(Vec::new()),
    });
    let runtime = MediaRuntime::new(
        ingress,
        Arc::clone(&store),
        MediaProviders {
            realtime: vec![provider.clone()],
            speech: None,
            speaker: Some(speaker.clone()),
        },
    );
    let stream = stream();
    runtime
        .open_session(MediaSessionRequest {
            stream: stream.clone(),
            descriptor: serde_json::Map::new(),
            recording,
        })
        .await
        .unwrap();
    while inbox.try_recv().is_ok() {}
    Harness {
        store,
        runtime,
        provider,
        speaker,
        stream,
        inbox,
    }
}

/// 1. A provider that reports its own segment index keeps that identity, and
/// a revision that re-reports an already finalized segment is a CORRECTION of
/// the same utterance — same segment, same original sealed bytes, no
/// fabricated second utterance and no mislabelled audio.
#[tokio::test]
async fn provider_segment_correction_keeps_one_utterance_and_its_original_audio() {
    let mut harness = open(Some(scope(true))).await;
    let stream = harness.stream.clone();

    harness.runtime.push_audio(&stream, &[1, 2, 3, 4]).await.unwrap();
    harness
        .emit(event("我们讲", true, Some(3), None))
        .await;
    harness.runtime.push_audio(&stream, &[5, 6]).await.unwrap();
    harness
        .emit(event("我们讲转写", false, Some(3), None))
        .await;
    let first_audio = harness.effective_audio(3);
    assert!(
        matches!(first_audio, Some(MediaAudioRef::Sealed { .. })),
        "the segment final seals the audio heard while recording was live"
    );

    // The engine re-decodes the same utterance: a correction, not a new one.
    harness.runtime.push_audio(&stream, &[7, 8, 9, 10]).await.unwrap();
    harness
        .emit(event("我们讲转写修订", false, Some(3), None))
        .await;
    // The next utterance is a different segment and owns the newer bytes.
    harness
        .emit(event("下一句", false, Some(4), None))
        .await;

    let transcript = harness.transcript();
    assert_eq!(
        transcript.segments.len(),
        2,
        "a correction never fabricates a second utterance"
    );
    let corrected = &transcript.segments[0];
    assert_eq!(corrected.segment, 3);
    assert_eq!(corrected.effective.text, "我们讲转写修订");
    assert!(!corrected.effective.partial);
    assert_eq!(corrected.revisions, 3);
    assert!(corrected.had_final_correction, "the correction is retained as such");
    assert_eq!(
        corrected.effective.audio, first_audio,
        "the correction keeps the original recording reference"
    );
    assert_eq!(
        harness.captured_pcm(3),
        vec![1, 2, 3, 4, 5, 6],
        "the corrected utterance's audio is the interval it was heard in"
    );
    let next = &transcript.segments[1];
    assert_eq!(next.segment, 4);
    assert_eq!(harness.captured_pcm(4), vec![7, 8, 9, 10]);
    assert!(transcript.gaps.is_empty());

    // Every admitted revision is one durable fact of the SAME subject inbox.
    let admitted = harness.inbox_events();
    assert_eq!(admitted.len(), 4);
    assert!(admitted
        .iter()
        .all(|event| event.kind == InboxEventKind::MediaRevision
            && event.source == "media/test-asr"));
    let mut ids = admitted
        .iter()
        .map(|event| event.event_id)
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4, "each revision has its own inbox identity");
}

/// 2. A late partial never displaces a final: the effective observation stays
/// final, and the interim claim is still retained and delivered raw.
#[tokio::test]
async fn late_partial_never_displaces_the_final_observation() {
    let mut harness = open(None).await;
    let stream = harness.stream.clone();

    harness
        .emit(event("课堂开始", false, Some(1), None))
        .await;
    harness.emit(event("课堂", true, Some(1), None)).await;

    let transcript = harness.transcript();
    assert_eq!(transcript.segments.len(), 1);
    let segment = &transcript.segments[0];
    assert_eq!(segment.effective.text, "课堂开始");
    assert!(!segment.effective.partial, "a final stays final");
    assert_eq!(segment.revisions, 2);

    let revisions = harness.store.media_revisions(&stream, 1).unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[1].disposition, MediaRevisionDisposition::Superseded);
    assert_eq!(revisions[1].text, "课堂");
    assert!(revisions[1].partial);

    // The late claim is still its own durable inbox fact, not a silent drop.
    let admitted = harness.inbox_events();
    assert_eq!(admitted.len(), 2);
    assert!(admitted
        .iter()
        .all(|event| event.kind == InboxEventKind::MediaRevision));
}

/// 3. A recording whose admitted scope declares `store_audio: false` is a
/// real, visible recording that stores no byte: the transcript revision
/// carries no audio reference and the usage read reports zero stored audio.
#[tokio::test]
async fn declared_scope_without_audio_stores_nothing() {
    let mut harness = open(Some(scope(false))).await;
    let stream = harness.stream.clone();
    let recording_id = format!("rec-{}", stream.stream_id);

    harness.runtime.push_audio(&stream, &[1, 2, 3, 4]).await.unwrap();
    harness
        .emit(event("只转写不存音频", false, None, None))
        .await;

    let recording = harness
        .store
        .media_recording(&stream, &recording_id)
        .unwrap()
        .expect("the recording is a durable fact");
    assert_eq!(recording.state, MediaRecordingState::Recording);
    assert!(
        !recording.scope.store_audio,
        "the committed scope is what the subject reads"
    );

    let transcript = harness.transcript();
    assert_eq!(transcript.segments.len(), 1);
    assert!(
        transcript.segments[0].effective.audio.is_none(),
        "no byte may be sealed outside the declared scope"
    );
    let usage = harness
        .store
        .media_recording_usage(&stream, &recording_id)
        .unwrap();
    assert_eq!(usage.audio_bytes, 0);
    assert_eq!(usage.audio_segments, 0);
    assert_eq!(usage.revisions, 1);

    let status = harness
        .runtime
        .session_status(&stream)
        .await
        .expect("live session");
    assert!(status.recording, "the recording itself is live and visible");
    assert!(!status.storing_audio);
}

/// 4. A voiceprint claim is computed on the ORIGINAL sealed bytes — never a
/// re-capture, re-download or re-transcription — and admitted on the same
/// segment with the retained provider text, keeping the audio reference.
#[tokio::test]
async fn voiceprint_claim_binds_to_the_original_sealed_audio() {
    let mut harness = open(Some(scope(true))).await;
    let stream = harness.stream.clone();
    let recording_id = format!("rec-{}", stream.stream_id);

    harness.runtime.push_audio(&stream, &[1, 2, 3, 4]).await.unwrap();
    harness
        .emit(event("老师提问", false, Some(1), None))
        .await;
    harness.runtime.push_audio(&stream, &[5, 6, 7, 8]).await.unwrap();
    harness
        .emit(event("学生回答", false, Some(2), None))
        .await;
    let original = harness.effective_audio(1).expect("segment 1 sealed audio");
    let usage_before = harness
        .store
        .media_recording_usage(&stream, &recording_id)
        .unwrap();

    let outcome = harness.runtime.identify_speaker(&stream, 1).await.unwrap();
    assert_eq!(
        outcome.admission.receipt.disposition,
        MediaRevisionDisposition::FinalCorrection
    );
    assert_eq!(outcome.delivery, SubjectDelivery::Queued);

    let effective = harness
        .store
        .media_effective_segment(&stream, 1)
        .unwrap()
        .expect("segment 1 still admitted");
    assert_eq!(effective.text, "老师提问", "the engine's words are untouched");
    assert!(!effective.partial);
    assert_eq!(
        effective.audio,
        Some(original.clone()),
        "identification never detaches the original recording"
    );
    let claim = effective.speaker.expect("engine claim admitted");
    assert_eq!(claim.speaker.as_deref(), Some("spk-teacher"));
    assert_eq!(claim.confidence, Some(0.82));
    assert_eq!(claim.source.as_deref(), Some("test-sv:v1"));
    assert!(!claim.corrected, "the segment had no earlier claim to correct");

    let MediaAudioRef::Sealed { object } = original else {
        panic!("segment 1 audio must be sealed");
    };
    assert_eq!(
        *harness.speaker.calls.lock().await,
        vec![object.digest.to_string()],
        "the voiceprint engine reads the bytes this ledger already sealed"
    );
    let usage_after = harness
        .store
        .media_recording_usage(&stream, &recording_id)
        .unwrap();
    assert_eq!(
        usage_after.audio_bytes, usage_before.audio_bytes,
        "identification stores no new audio, so the cost read is unchanged"
    );

    // A segment with no original audio cannot be identified at all.
    let missing = harness.runtime.identify_speaker(&stream, 9).await.unwrap_err();
    assert!(matches!(missing, ShenkoError::NotFound(_)));
    assert_eq!(
        harness.inbox_events().len(),
        3,
        "two finals plus the identification claim, one fact each"
    );
}

/// 5. A lost transport is a durable terminal fact of the same subject inbox,
/// and a later explicit end settles it under a NEW end identity.
#[tokio::test]
async fn transport_loss_records_a_durable_disconnect() {
    let mut harness = open(None).await;
    let stream = harness.stream.clone();

    harness
        .emit(event("one line", false, None, None))
        .await;
    harness
        .provider_finished(Some("websocket error: connection reset".into()))
        .await;

    let row = harness
        .store
        .media_stream(&stream)
        .unwrap()
        .expect("durable stream row");
    assert_eq!(row.state, MediaStreamState::Disconnected);
    let end = row.end.expect("terminal notice recorded");
    assert_eq!(end.state, MediaStreamState::Disconnected);
    assert_eq!(
        end.reason.as_deref(),
        Some("websocket error: connection reset")
    );
    assert!(harness.runtime.session_status(&stream).await.is_none());

    let admitted = harness.inbox_events();
    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0].kind, InboxEventKind::MediaRevision);
    assert_eq!(admitted[1].kind, InboxEventKind::MediaStream);

    // The provider's own cause may still settle the stream explicitly.
    harness
        .runtime
        .close_session(&stream, Some("operator ended the class".into()))
        .await
        .unwrap();
    let row = harness.store.media_stream(&stream).unwrap().unwrap();
    assert_eq!(row.state, MediaStreamState::Ended);
    let ends = harness.store.media_stream_ends(&stream).unwrap();
    assert_eq!(ends.len(), 2, "both terminal notices stay immutable");
    // Same-millisecond admissions are ordered by end identity, so the two
    // notices are asserted as a set of retained facts, not an arrival order.
    assert!(ends
        .iter()
        .any(|end| end.state == MediaStreamState::Disconnected));
    assert!(ends.iter().any(|end| end.state == MediaStreamState::Ended));
}

/// 6. An index-less provider keeps the derived boundary (a final opens the
/// next segment), and a speaker claim change is a correction only inside the
/// segment the claim belongs to.
#[tokio::test]
async fn derived_boundary_and_per_segment_speaker_correction() {
    let mut harness = open(None).await;

    harness
        .emit(event("第一句", false, None, Some(claim("spk-a", 0.9))))
        .await;
    harness
        .emit(event("第二句", false, None, Some(claim("spk-b", 0.7))))
        .await;
    // A correction of segment 1 keeps spk-a: unchanged claim, no correction.
    harness
        .emit(event("第一句修订", false, Some(1), Some(claim("spk-a", 0.9))))
        .await;
    // A correction of segment 2 changes the claim: corrected stays visible.
    harness
        .emit(event("第二句修订", false, Some(2), Some(claim("spk-a", 0.6))))
        .await;

    let transcript = harness.transcript();
    let derived = transcript
        .segments
        .iter()
        .map(|segment| segment.segment)
        .collect::<Vec<_>>();
    assert_eq!(derived, vec![1, 2], "a final opens the next derived segment");
    assert!(transcript
        .segments
        .iter()
        .all(|segment| segment.effective.audio.is_none()));
    let first = transcript.segments[0].effective.speaker.clone().unwrap();
    assert!(!first.corrected, "the same speaker claim is not a correction");
    let second = transcript.segments[1].effective.speaker.clone().unwrap();
    assert!(
        second.corrected,
        "a changed claim inside the same segment is a correction"
    );
    assert_eq!(second.speaker.as_deref(), Some("spk-a"));
}

/// 7. Provider bindings are a real plugin selection: a duplicate slot, an
/// unknown protocol or a zero sample rate is refused instead of silently
/// overriding the earlier engine or driving it with the wrong wire format.
#[tokio::test]
async fn provider_bindings_are_validated_at_load() {
    let dir = std::env::temp_dir().join(format!("shenko-media-bindings-{}", EntityId::new()));
    std::fs::create_dir_all(dir.join("media")).unwrap();
    let store = Store::open(dir.join("test.db"), dir.join("cas")).unwrap();
    let cas = Arc::clone(&store.cas);
    let write = |bindings: serde_json::Value| {
        std::fs::write(
            dir.join("media").join("providers.json"),
            serde_json::to_vec(&bindings).unwrap(),
        )
        .unwrap();
    };

    // No file: no engine configured, and that is not an error.
    assert!(MediaProviders::load(&dir, Arc::clone(&cas)).unwrap().realtime.is_empty());

    let mut duplicate = serde_json::to_value(realtime_binding()).unwrap();
    duplicate["provider"] = serde_json::Value::String("speech-1".into());
    duplicate["capability"] = serde_json::Value::String("speech_output".into());
    duplicate["endpoint"] = serde_json::Value::String("http://127.0.0.1:9".into());
    let mut second_voice = duplicate.clone();
    second_voice["provider"] = serde_json::Value::String("speech-2".into());
    write(serde_json::json!([duplicate, second_voice]));
    let error = MediaProviders::load(&dir, Arc::clone(&cas)).unwrap_err();
    assert_eq!(error_code(&error), Some("media_binding_duplicate"));

    let mut unsupported = serde_json::to_value(realtime_binding()).unwrap();
    unsupported["options"] = serde_json::json!({ "protocol": "whisper-ws" });
    write(serde_json::json!([unsupported]));
    let error = MediaProviders::load(&dir, Arc::clone(&cas)).unwrap_err();
    assert_eq!(
        error_code(&error),
        Some("media_binding_protocol_unsupported")
    );

    let mut no_rate = serde_json::to_value(realtime_binding()).unwrap();
    no_rate["sample_rate"] = serde_json::Value::from(0);
    write(serde_json::json!([no_rate]));
    let error = MediaProviders::load(&dir, Arc::clone(&cas)).unwrap_err();
    assert_eq!(error_code(&error), Some("media_binding_invalid"));

    write(serde_json::json!([serde_json::to_value(realtime_binding()).unwrap()]));
    let providers = MediaProviders::load(&dir, Arc::clone(&cas)).unwrap();
    assert_eq!(providers.realtime.len(), 1);
    assert!(providers.speaker.is_none() && providers.speech.is_none());
}

fn error_code(error: &ShenkoError) -> Option<&str> {
    match error {
        ShenkoError::Protocol(protocol) => Some(protocol.code.as_str()),
        _ => None,
    }
}
