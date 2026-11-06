//! Admitted recording controls reach the live recorder — SPEC §20 录音状态、
//! 正在存储的范围和停止控制必须真实可见, and 最终文本与原音频对应可追溯
//! (§15.2/§20: the segment-final revision's sealed audio is what the provider
//! heard while the recording was live).
//! Three independent scenarios driven through the real path — MediaRuntime
//! over a scripted realtime provider, never a mocked ledger: an admitted
//! `stop` stops capture so post-stop pushes are not sealed into the final
//! transcript; an admitted `start` on a session opened without recording
//! creates the recorder; the durable receipts still record exactly what was
//! admitted (replay returns the original receipt, never a second capture
//! change). No ASR/model/audio device is exercised or simulated.

use std::sync::Arc;

use async_trait::async_trait;
use shenko_media::provider::{
    MediaEventSink, RealtimeSession, RealtimeSessionProvider,
};
use shenko_media::runtime::{MediaProviders, MediaRuntime};
use shenko_media::{MediaIngress, SubjectDelivery};
use shenko_store::store::Store;
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::media::*;

fn stream() -> MediaStreamDescriptor {
    MediaStreamDescriptor {
        provider: "test-asr".into(),
        account: "acct-1".into(),
        stream_id: "call-1".into(),
    }
}

fn binding() -> MediaProviderBinding {
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

fn scope() -> MediaRecordingScope {
    MediaRecordingScope {
        kind: "call".into(),
        store_audio: true,
        retention: "session".into(),
        provider_facts: serde_json::Map::new(),
    }
}

/// Scripted realtime session: records pushed bytes, emits transcript events
/// through the real `MediaEventSink` the runtime wired at open.
struct FakeSession {
    descriptor: MediaStreamDescriptor,
    sink: Arc<dyn MediaEventSink>,
    pushed: tokio::sync::Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl RealtimeSession for FakeSession {
    fn descriptor(&self) -> MediaStreamDescriptor {
        self.descriptor.clone()
    }

    async fn push_audio(&self, chunk: &[u8]) -> Result<(), ShenkoError> {
        self.pushed.lock().await.push(chunk.to_vec());
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

impl FakeProvider {
    fn new(binding: MediaProviderBinding) -> Self {
        Self {
            binding,
            sessions: tokio::sync::Mutex::new(Vec::new()),
        }
    }
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
            pushed: tokio::sync::Mutex::new(Vec::new()),
        });
        self.sessions.lock().await.push(session.clone());
        Ok(session)
    }
}

async fn fixture(
    recording_at_open: Option<MediaRecordingScope>,
) -> (
    Arc<Store>,
    Arc<MediaRuntime>,
    Arc<FakeProvider>,
    MediaStreamDescriptor,
    EntityId,
    tokio::sync::mpsc::Receiver<InboxEvent>,
) {
    let dir = std::env::temp_dir().join(format!("shenko-media-recording-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(dir.join("test.db"), dir.join("cas")).unwrap());
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let agent_id = EntityId::new();
    let ingress = Arc::new(MediaIngress::new(
        Arc::clone(&store),
        agent_id,
        sender,
    ));
    let provider = Arc::new(FakeProvider::new(binding()));
    let providers = MediaProviders {
        realtime: vec![provider.clone()],
        speech: None,
        speaker: None,
    };
    let runtime = MediaRuntime::new(ingress, Arc::clone(&store), providers);
    let stream = stream();
    runtime
        .open_session(MediaSessionRequest {
            stream: stream.clone(),
            descriptor: serde_json::Map::new(),
            recording: recording_at_open,
        })
        .await
        .unwrap();
    (store, runtime, provider, stream, agent_id, receiver)
}

async fn emit_final(
    provider: &FakeProvider,
    stream: &MediaStreamDescriptor,
    text: &str,
) {
    let session = provider.sessions.lock().await[0].clone();
    session
        .sink
        .on_transcript(
            stream,
            MediaTranscriptEvent {
                text: text.to_owned(),
                partial: false,
                speaker: None,
                captured_at_ms: None,
                provider_facts: serde_json::Map::new(),
            },
        )
        .await;
}

/// Bytes the admitted final revision sealed, read back from this ledger's
/// CAS. The 44-byte RIFF/WAVE header precedes the captured PCM payload.
fn sealed_wav(store: &Store, stream: &MediaStreamDescriptor) -> Vec<u8> {
    let transcript = store.media_transcript(stream).unwrap().unwrap();
    let audio = transcript
        .segments
        .last()
        .and_then(|segment| segment.effective.audio.clone());
    match audio {
        Some(MediaAudioRef::Sealed { object }) => {
            store.cas.read_object(&object.digest).unwrap()
        }
        other => panic!("final revision must carry sealed audio, got {other:?}"),
    }
}

/// An admitted `stop` on a session opened with recording stops the live
/// capture: audio pushed after the stop is absent from the sealed segment,
/// while bytes captured before the stop still close with the segment-final
/// revision. The durable facts stay honest — the stop receipt says `stopped`
/// and a replayed control returns the ORIGINAL receipt without re-arming
/// anything.
#[tokio::test]
async fn admitted_stop_stops_live_capture_and_seals_only_pre_stop_audio() {
    let (store, runtime, provider, stream, agent_id, _receiver) =
        fixture(Some(scope())).await;
    let recording_id = format!("rec-{}", stream.stream_id);

    runtime.push_audio(&stream, &[1, 2, 3, 4]).await.unwrap();

    let stop = MediaRecordingControl {
        control_id: "ctl-stop-1".into(),
        recording_id: recording_id.clone(),
        stream: stream.clone(),
        action: MediaRecordingAction::Stop,
        scope: None,
        reason: Some("operator stopped".into()),
    };
    let outcome = runtime.control_recording(stop.clone()).await.unwrap();
    assert_eq!(outcome.admission.receipt.state, MediaRecordingState::Stopped);
    assert!(!outcome.admission.receipt.replayed);
    // A live queue exists (the receiver is held like the real Inbox), so the
    // admitted control is entitled to `Queued`: the durable original's wake
    // decoded and reached the subject inbox with this same admission.
    assert_eq!(outcome.delivery, SubjectDelivery::Queued);
    let status = runtime.session_status(&stream).await.unwrap();
    assert!(!status.recording, "live session must report recording stopped");

    // Audio received after the admitted stop must not be captured.
    runtime.push_audio(&stream, &[9, 9, 9, 9, 9, 9]).await.unwrap();

    emit_final(&provider, &stream, "final words").await;

    let wav = sealed_wav(&store, &stream);
    assert_eq!(wav.len(), 44 + 4, "only the pre-stop chunk may seal");
    assert_eq!(wav[44..], [1u8, 2, 3, 4]);

    // The ledger still records exactly what was admitted.
    let recording = store
        .media_recording(&stream, &recording_id)
        .unwrap()
        .expect("recording row must exist");
    assert_eq!(recording.state, MediaRecordingState::Stopped);
    assert_eq!(recording.stop_control_id.as_deref(), Some("ctl-stop-1"));

    // A start under a NEW recording identity re-arms capture; replaying the
    // old stop afterwards must return its ORIGINAL receipt and must not
    // reach the live recorder again — otherwise a stale replay would kill a
    // different recording's capture.
    runtime
        .control_recording(MediaRecordingControl {
            control_id: "ctl-start-2".into(),
            recording_id: "rec-again".into(),
            stream: stream.clone(),
            action: MediaRecordingAction::Start,
            scope: Some(scope()),
            reason: None,
        })
        .await
        .unwrap();
    let replayed = runtime.control_recording(stop).await.unwrap();
    assert!(replayed.admission.receipt.replayed);
    assert_eq!(
        replayed.admission.receipt.event_id,
        outcome.admission.receipt.event_id
    );
    let status = runtime.session_status(&stream).await.unwrap();
    assert!(
        status.recording,
        "a replayed control must not disarm the new recording's capture"
    );
    assert_eq!(replayed.delivery, SubjectDelivery::Replay);

    // The durable-original contract the wake decodes must also hold where it
    // is actually consumed: every pending `media/*` inbox row — including
    // this stop's `media_recording` original — deserializes as InboxEvent
    // exactly the way the subject's pending drain decodes it, never breaking
    // the loop on an unknown kind.
    let originals = store
        .pending_inbox_events(agent_id, 64)
        .unwrap();
    assert!(!originals.is_empty());
    let decoded: Vec<InboxEvent> = originals
        .iter()
        .map(|(event_id, data)| {
            let event: InboxEvent = serde_json::from_str(data).unwrap();
            assert_eq!(event.event_id, *event_id, "row identity must match its original");
            event
        })
        .collect();
    let stop_event = decoded
        .iter()
        .find(|event| event.event_id == outcome.admission.receipt.event_id)
        .expect("the admitted stop's durable original is pending");
    assert_eq!(stop_event.kind, InboxEventKind::MediaRecording);
    let payload: MediaIngressEvent =
        serde_json::from_value(stop_event.payload.clone()).unwrap();
    match payload {
        MediaIngressEvent::Recording { recording, action, .. } => {
            assert_eq!(action, MediaRecordingAction::Stop);
            assert_eq!(recording.state, MediaRecordingState::Stopped);
        }
        other => panic!("stop original must carry a recording event, got {other:?}"),
    }
}

/// An admitted `start` on a session opened WITHOUT recording creates the
/// recorder, so audio pushed after the start is captured and sealed — and
/// audio pushed before the start is not. The durable receipt records the
/// `recording` state it admitted.
#[tokio::test]
async fn admitted_start_arms_recorder_on_unrecorded_session() {
    let (store, runtime, provider, stream, _agent_id, _receiver) = fixture(None).await;
    let status = runtime.session_status(&stream).await.unwrap();
    assert!(!status.recording, "session opened without recording");

    // Not captured: no recorder exists yet.
    runtime.push_audio(&stream, &[7, 7]).await.unwrap();

    let start = MediaRecordingControl {
        control_id: "ctl-start-1".into(),
        recording_id: "rec-late".into(),
        stream: stream.clone(),
        action: MediaRecordingAction::Start,
        scope: Some(scope()),
        reason: Some("operator armed".into()),
    };
    let outcome = runtime.control_recording(start).await.unwrap();
    assert_eq!(outcome.admission.receipt.state, MediaRecordingState::Recording);
    assert!(!outcome.admission.receipt.replayed);
    assert_eq!(outcome.delivery, SubjectDelivery::Queued);
    let status = runtime.session_status(&stream).await.unwrap();
    assert!(status.recording, "admitted start must arm the recorder");

    runtime.push_audio(&stream, &[5, 6, 7, 8]).await.unwrap();
    emit_final(&provider, &stream, "after start").await;

    let wav = sealed_wav(&store, &stream);
    assert_eq!(wav.len(), 44 + 4, "only post-start audio may seal");
    assert_eq!(wav[44..], [5u8, 6, 7, 8]);

    let recording = store
        .media_recording(&stream, "rec-late")
        .unwrap()
        .expect("recording row must exist");
    assert_eq!(recording.state, MediaRecordingState::Recording);

    // An admitted stop then un-arms the same live recorder.
    runtime
        .control_recording(MediaRecordingControl {
            control_id: "ctl-stop-2".into(),
            recording_id: "rec-late".into(),
            stream: stream.clone(),
            action: MediaRecordingAction::Stop,
            scope: None,
            reason: None,
        })
        .await
        .unwrap();
    let status = runtime.session_status(&stream).await.unwrap();
    assert!(!status.recording, "admitted stop must un-arm the recorder");
    runtime.push_audio(&stream, &[3, 3]).await.unwrap();
    emit_final(&provider, &stream, "post stop").await;
    // The second final seals nothing new: the buffer closed with the first
    // final and the stopped recorder kept no post-stop bytes.
    let transcript = store.media_transcript(&stream).unwrap().unwrap();
    let audio = transcript
        .segments
        .last()
        .and_then(|segment| segment.effective.audio.clone());
    assert!(audio.is_none(), "no audio may seal after the stop");
}
