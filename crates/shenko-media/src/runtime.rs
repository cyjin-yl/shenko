//! Media runtime — live sessions, recording, barge-in, playback (SPEC §20).
//!
//! [`MediaRuntime`] is the typed service behind the reserved media slots: it
//! opens realtime sessions against configured provider bindings
//! ([`crate::provider::RealtimeSessionProvider`]), feeds their PCM16 capture
//! into a [`crate::recording::MediaAudioRecorder`] while a recording is
//! live, admits every engine transcript event through the SAME
//! [`MediaIngress`] the external providers use (one admission path, one
//! durable original per fact, same subject inbox), stops the agent's own
//! playback when the far side speaks (barge-in), and applies admitted
//! recording controls to the live capture — an admitted `start` arms the
//! recorder, an admitted `stop` stops the capture so post-stop audio is
//! never sealed into the final transcript.
//!
//! Nothing here synthesizes transcript text: the engine owns every word,
//! every speaker claim stays a provider claim (unknown stays unknown), and
//! the sealed original audio is what the provider heard — CAS bytes, never a
//! provider-local handle.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use cordis_core::{Context, Plugin, Service};
use shenko_store::store::Store;
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::media::*;

use crate::provider::{
    AudioOutput, MediaEventSink, MediaPlaybackReceipt, RealtimeSession, RealtimeSessionProvider,
    SpeakerIdentity,
};
use crate::recording::MediaAudioRecorder;
use crate::{MediaDelivery, MediaIngress};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Configured engine bindings the runtime can open sessions against.
pub struct MediaProviders {
    pub realtime: Vec<Arc<dyn RealtimeSessionProvider>>,
    pub speech: Option<Arc<dyn AudioOutput>>,
    pub speaker: Option<Arc<dyn SpeakerIdentity>>,
}

impl MediaProviders {
    /// Load engine bindings from `<data_dir>/media/providers.json`, an array
    /// of [`MediaProviderBinding`] values. Missing file or malformed entry is
    /// a typed error, not a silent no-engine install: the runtime refuses to
    /// pretend a binding exists. An absent config file means no engines are
    /// configured — the ingress path still works for external providers.
    pub fn load(data_dir: &std::path::Path, cas: Arc<shenko_store::cas::ContentStore>) -> std::result::Result<Self, ShenkoError> {
        use crate::provider::{FunAsrSpeakerIdentityInCas, FunAsrWebsocketProvider, OpenAiSpeechOutput};

        let config = data_dir.join("media").join("providers.json");
        if !config.exists() {
            return Ok(Self { realtime: vec![], speech: None, speaker: None });
        }
        let bytes = std::fs::read(&config).map_err(ShenkoError::Io)?;
        let bindings: Vec<MediaProviderBinding> = serde_json::from_slice(&bytes)
            .map_err(ShenkoError::Serde)?;
        let mut providers = Self { realtime: vec![], speech: None, speaker: None };
        for binding in bindings {
            match binding.capability {
                MediaProviderCapability::RealtimeSession => providers
                    .realtime
                    .push(Arc::new(FunAsrWebsocketProvider::new(binding))),
                MediaProviderCapability::SpeechOutput => providers.speech = Some(Arc::new(
                    OpenAiSpeechOutput::new(binding, Arc::clone(&cas)),
                )),
                MediaProviderCapability::SpeakerIdentity => providers.speaker = Some(Arc::new(
                    FunAsrSpeakerIdentityInCas::new(binding, Arc::clone(&cas)),
                )),
            }
        }
        Ok(providers)
    }
}

impl std::fmt::Debug for MediaProviders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MediaProviders")
            .field("realtime", &self.realtime.len())
            .field("speech", &self.speech.is_some())
            .field("speaker", &self.speaker.is_some())
            .finish()
    }
}

/// Live state of one open session. The provider session itself is immutable;
/// only the bookkeeping the runtime owns changes.
pub struct ActiveMediaSession {
    pub descriptor: MediaStreamDescriptor,
    session: Arc<dyn RealtimeSession>,
    recorder: Option<MediaAudioRecorder>,
    /// Sample rate of the binding this session opened against; needed to arm
    /// a recorder when an admitted `start` reaches a session opened without
    /// recording.
    sample_rate: u32,
    /// Recording identities whose admitted controls arm this session's
    /// capture. Kept on the session itself — never inside the recorder,
    /// which `push_audio` takes out of the map while a provider push
    /// awaits — so an admitted `stop` lands even mid-push. Capture runs
    /// while any admitted recording is live, so a `stop` of one recording
    /// never silently ends the capture another live recording still owns:
    /// the live state and the ledger's per-recording state agree.
    recording_ids: HashSet<String>,
    segment: u64,
    revision_no: u64,
    last_speaker: Option<MediaSpeakerClaim>,
    priority_counter: u64,
}

impl ActiveMediaSession {
    fn next_revision(&mut self) -> (u64, u64) {
        self.revision_no += 1;
        (self.segment, self.revision_no)
    }

    /// True while at least one admitted recording arms this session's
    /// capture.
    fn is_recording(&self) -> bool {
        !self.recording_ids.is_empty()
    }
}

/// Read-side snapshot of one open session (UI/reconciliation).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaSessionStatus {
    pub stream: MediaStreamDescriptor,
    pub segment: u64,
    pub recording: bool,
    pub revisions: u64,
    pub last_speaker: Option<MediaSpeakerClaim>,
}

/// The media runtime service — published on the shared Cordis root.
pub struct MediaRuntime {
    ingress: Arc<MediaIngress>,
    store: Arc<Store>,
    providers: MediaProviders,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, ActiveMediaSession>>>,
}

impl std::fmt::Debug for MediaRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MediaRuntime").finish_non_exhaustive()
    }
}

impl Service for MediaRuntime {
    const NAME: &'static str = "shenko.media.runtime/v1";
}

struct RuntimeSink(Arc<MediaRuntime>);

#[async_trait]
impl MediaEventSink for RuntimeSink {
    async fn on_transcript(&self, stream: &MediaStreamDescriptor, event: MediaTranscriptEvent) {
        if let Err(error) = self.0.admit_transcript(stream, event).await {
            tracing::warn!(stream = %stream.stream_id, "media transcript admission failed: {error}");
        }
    }

    async fn on_session_finished(&self, stream: &MediaStreamDescriptor, reason: Option<String>) {
        self.0.on_session_finished(stream, reason).await;
    }
}

impl MediaRuntime {
    pub fn new(
        ingress: Arc<MediaIngress>,
        store: Arc<Store>,
        providers: MediaProviders,
    ) -> Arc<Self> {
        Arc::new(Self {
            ingress,
            store,
            providers,
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Engine bindings currently configured.
    pub fn providers(&self) -> &MediaProviders {
        &self.providers
    }

    /// Open one realtime session against a configured realtime binding. The
    /// stream open is admitted (durable) before the transport connects; a
    /// connect failure leaves an honestly-open stream row the caller ends
    /// explicitly.
    pub async fn open_session(
        self: &Arc<Self>,
        mut request: MediaSessionRequest,
    ) -> Result<MediaStreamDescriptor, ShenkoError> {
        let provider = self
            .providers
            .realtime
            .iter()
            .find(|provider| {
                provider.binding().provider == request.stream.provider
                    && provider.binding().capability == MediaProviderCapability::RealtimeSession
            })
            .cloned()
            .ok_or_else(|| ShenkoError::not_found(format!(
                "no realtime media binding for provider {}", request.stream.provider)))?;
        let recording = request.recording.take();
        let descriptor = request.stream.clone();
        self.ingress
            .admit_stream(MediaStreamOpen {
                open_id: format!("open-{}", descriptor.stream_id),
                stream: descriptor.clone(),
                descriptor: request.descriptor.clone(),
            })
            .await?;
        if let Some(scope) = recording {
            let recording_id = format!("rec-{}", descriptor.stream_id);
            self.ingress
                .admit_recording(MediaRecordingControl {
                    control_id: format!("rec-{}-start", descriptor.stream_id),
                    recording_id: recording_id.clone(),
                    stream: descriptor.clone(),
                    action: MediaRecordingAction::Start,
                    scope: Some(scope.clone()),
                    reason: Some("recording armed at session open".to_owned()),
                })
                .await?;
            let sample_rate = provider.binding().sample_rate;
            let recorder = MediaAudioRecorder::new(descriptor.clone(), sample_rate);
            let session = provider
                .open(request, Arc::new(RuntimeSink(self.clone())))
                .await?;
            self.sessions.lock().await.insert(
                descriptor.stream_id.clone(),
                ActiveMediaSession {
                    descriptor: descriptor.clone(),
                    session,
                    recorder: Some(recorder),
                    sample_rate,
                    recording_ids: HashSet::from([recording_id]),
                    segment: 1,
                    revision_no: 0,
                    last_speaker: None,
                    priority_counter: 0,
                },
            );
            return Ok(descriptor);
        }
        let sink: Arc<dyn MediaEventSink> = Arc::new(RuntimeSink(self.clone()));
        let sample_rate = provider.binding().sample_rate;
        let session = provider.open(request, sink).await?;
        self.sessions.lock().await.insert(
            descriptor.stream_id.clone(),
            ActiveMediaSession {
                descriptor: descriptor.clone(),
                session,
                recorder: None,
                sample_rate,
                recording_ids: HashSet::new(),
                segment: 1,
                revision_no: 0,
                last_speaker: None,
                priority_counter: 0,
            },
        );
        Ok(descriptor)
    }

    /// End one session's durable stream row and close its transport. An
    /// unknown stream is a plain not-found, never a synthesized end.
    pub async fn close_session(
        self: &Arc<Self>,
        stream: &MediaStreamDescriptor,
        reason: Option<String>,
    ) -> Result<(), ShenkoError> {
        let session = self.sessions.lock().await.remove(&stream.stream_id);
        if let Some(active) = session {
            let _ = active.session.close().await;
        }
        self.ingress
            .end_stream(MediaStreamEnd {
                end_id: format!("end-{}-{}", stream.stream_id, EntityId::new()),
                stream: stream.clone(),
                state: MediaStreamState::Ended,
                ended_at_ms: Some(now_ms()),
                reason,
            })
            .await?;
        Ok(())
    }

    /// Push one chunk of mono PCM16 audio into the session; while a
    /// recording is live the same bytes are captured for the segment seal.
    /// The capture decision consults the admitted recording state BOTH when
    /// the push begins and when the provider acknowledges it, so a chunk in
    /// flight while a `stop` was admitted is never captured after the stop.
    pub async fn push_audio(
        self: &Arc<Self>,
        stream: &MediaStreamDescriptor,
        chunk: &[u8],
    ) -> Result<(), ShenkoError> {
        let (session, recorder) = {
            let mut sessions = self.sessions.lock().await;
            let Some(active) = sessions.get_mut(&stream.stream_id) else {
                return Err(ShenkoError::not_found(format!(
                    "no live media session for stream {}", stream.stream_id)));
            };
            let recorder = if active.is_recording() {
                active.recorder.take()
            } else {
                None
            };
            (active.session.clone(), recorder)
        };
        let result = session.push_audio(chunk).await;
        let mut sessions = self.sessions.lock().await;
        if let Some(active) = sessions.get_mut(&stream.stream_id) {
            if let Some(mut recorder) = recorder {
                if result.is_ok() && active.is_recording() {
                    recorder.push(chunk);
                }
                active.recorder = Some(recorder);
            }
        }
        result
    }

    /// Tell the engine the input ended and wait for its final ack; the
    /// session stays open for a possible renegotiation until
    /// [`MediaRuntime::close_session`].
    pub async fn finish_input(
        self: &Arc<Self>,
        stream: &MediaStreamDescriptor,
    ) -> Result<(), ShenkoError> {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(&stream.stream_id)
                .map(|active| active.session.clone())
                .ok_or_else(|| ShenkoError::not_found(format!(
                    "no live media session for stream {}", stream.stream_id)))?
        };
        session.finish().await
    }

    /// Admit one recording control and apply the admitted decision to the
    /// live session. `Start` arms capture — creating the recorder when the
    /// session opened without one — so the next audio push is captured;
    /// `Stop` releases that recording's arm so audio received after the
    /// admitted stop is not sealed as if the recording had continued, while
    /// bytes already held still close with the next segment-final revision.
    /// The durable notice commits first: only a newly admitted
    /// (non-replayed) control changes live capture, so the recorder and the
    /// ledger never disagree, and applying the same control twice stays
    /// idempotent.
    pub async fn control_recording(
        self: &Arc<Self>,
        control: MediaRecordingControl,
    ) -> Result<MediaDelivery<MediaRecordingAdmission>, ShenkoError> {
        let stream_id = control.stream.stream_id.clone();
        let recording_id = control.recording_id.clone();
        let action = control.action;
        let outcome = self.ingress.admit_recording(control).await?;
        if !outcome.admission.receipt.replayed {
            let mut sessions = self.sessions.lock().await;
            if let Some(active) = sessions.get_mut(&stream_id) {
                match action {
                    MediaRecordingAction::Stop => {
                        active.recording_ids.remove(&recording_id);
                    }
                    MediaRecordingAction::Start => {
                        active.recording_ids.insert(recording_id);
                        if active.recorder.is_none() {
                            active.recorder = Some(MediaAudioRecorder::new(
                                active.descriptor.clone(),
                                active.sample_rate,
                            ));
                        }
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// Admit one priority input from a UI or channel surface.
    pub async fn admit_priority_input(
        self: &Arc<Self>,
        input: MediaPriorityInput,
    ) -> Result<(), ShenkoError> {
        if input.kind == MediaPriorityKind::StopPlayback {
            if let Some(speech) = &self.providers.speech {
                let _ = speech.stop_playback().await;
            }
        }
        self.ingress.admit_priority(input).await?;
        Ok(())
    }

    /// Synthesize one utterance through the configured speech binding and
    /// optionally play it. Playback is interruptible: a far-side transcript
    /// arriving while it plays is admitted as barge-in and stops the voice.
    pub async fn speak(
        self: &Arc<Self>,
        request: MediaSpeechRequest,
        play: bool,
    ) -> Result<MediaSpeechAudio, ShenkoError> {
        let speech = self
            .providers
            .speech
            .as_ref()
            .ok_or_else(|| ShenkoError::not_found("no speech output binding configured"))?;
        let audio = speech.synthesize(&request).await?;
        if play {
            let speech = Arc::clone(speech);
            let object = audio.object.clone();
            let stream = request.stream.clone();
            let runtime = self.clone();
            tokio::spawn(async move {
                match speech.play(&object).await {
                    Ok(MediaPlaybackReceipt::Stopped { .. }) => {
                        if let Some(stream) = stream {
                            runtime.emit_playback_stopped(&stream).await;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(request = %request.request_id, "media playback failed: {error}");
                    }
                }
            });
        }
        Ok(audio)
    }

    /// Stop the current playback now (UI stop button, channel hangup).
    pub async fn stop_playback(self: &Arc<Self>) -> Result<bool, ShenkoError> {
        match &self.providers.speech {
            Some(speech) => speech.stop_playback().await,
            None => Ok(false),
        }
    }

    /// Live status of one open session.
    pub async fn session_status(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Option<MediaSessionStatus> {
        let sessions = self.sessions.lock().await;
        sessions.get(&stream.stream_id).map(|active| MediaSessionStatus {
            stream: active.descriptor.clone(),
            segment: active.segment,
            recording: active.is_recording(),
            revisions: active.revision_no,
            last_speaker: active.last_speaker.clone(),
        })
    }

    /// Read-side status of one recording from the durable rows.
    pub fn recording_status(
        &self,
        stream: &MediaStreamDescriptor,
        recording_id: &str,
    ) -> Result<Option<StoredMediaRecording>, ShenkoError> {
        self.store.media_recording(stream, recording_id)
    }

    /// Admitted priority inputs of one stream, oldest first.
    pub fn priority_inputs(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Vec<StoredMediaPriorityInput>, ShenkoError> {
        self.store.media_priority_inputs(stream)
    }

    async fn admit_transcript(
        self: &Arc<Self>,
        stream: &MediaStreamDescriptor,
        event: MediaTranscriptEvent,
    ) -> Result<(), ShenkoError> {
        // Barge-in lands BEFORE the interrupting transcript so the subject
        // reads the stop before the words that caused it.
        self.maybe_barge_in(stream).await;
        let (revision_id, segment, revision, audio, speaker, truncated) = {
            let mut sessions = self.sessions.lock().await;
            let Some(active) = sessions.get_mut(&stream.stream_id) else {
                // The session vanished (closed concurrently); the durable
                // stream row already records the end, so drop the late event
                // rather than inventing a segment.
                tracing::debug!(stream = %stream.stream_id, "transcript event for closed session dropped");
                return Ok(());
            };
            let (segment, revision) = active.next_revision();
            let revision_id = format!("{}:s{segment}:r{revision}", stream.stream_id);
            let (audio, truncated) = if event.partial {
                (None, false)
            } else {
                match active.recorder.as_mut() {
                    Some(recorder) => match recorder.seal_segment(&self.store.cas)? {
                        Some(sealed) => (Some(MediaAudioRef::Sealed { object: sealed.object }), sealed.truncated),
                        None => (None, false),
                    },
                    None => (None, false),
                }
            };
            let speaker = event.speaker.map(|claim| {
                let corrected = active
                    .last_speaker
                    .as_ref()
                    .is_some_and(|previous| previous != &claim);
                active.last_speaker = Some(claim.clone());
                MediaSpeakerClaim { corrected, ..claim }
            });
            (revision_id, segment, revision, audio, speaker, truncated)
        };
        let mut provider_facts = event.provider_facts;
        if truncated {
            provider_facts.insert("recording_truncated".to_owned(), serde_json::Value::Bool(true));
        }
        self.ingress
            .admit_revision(MediaRevisionInput {
                revision_id,
                stream: stream.clone(),
                segment,
                revision,
                text: event.text,
                partial: event.partial,
                captured_at_ms: event.captured_at_ms,
                speaker,
                audio,
                provider_facts,
            })
            .await?;
        if !event.partial {
            let mut sessions = self.sessions.lock().await;
            if let Some(active) = sessions.get_mut(&stream.stream_id) {
                active.segment += 1;
                active.last_speaker = None;
            }
        }
        Ok(())
    }

    async fn maybe_barge_in(self: &Arc<Self>, stream: &MediaStreamDescriptor) {
        let Some(speech) = self.providers.speech.as_ref() else { return };
        if !speech.is_playing() {
            return;
        }
        if !speech.stop_playback().await.unwrap_or(false) {
            return;
        }
        let priority_id = {
            let mut sessions = self.sessions.lock().await;
            let Some(active) = sessions.get_mut(&stream.stream_id) else { return };
            active.priority_counter += 1;
            format!("barge-in-{}-{}", stream.stream_id, active.priority_counter)
        };
        let input = MediaPriorityInput {
            priority_id,
            stream: stream.clone(),
            kind: MediaPriorityKind::BargeIn,
            text: None,
            occurred_at_ms: Some(now_ms()),
            audio: None,
            reason: Some("far side transcript arrived while the agent was speaking".to_owned()),
        };
        if let Err(error) = self.ingress.admit_priority(input).await {
            tracing::warn!(stream = %stream.stream_id, "barge-in priority admission failed: {error}");
        }
    }

    async fn emit_playback_stopped(self: &Arc<Self>, stream: &MediaStreamDescriptor) {
        let priority_id = {
            let mut sessions = self.sessions.lock().await;
            let Some(active) = sessions.get_mut(&stream.stream_id) else { return };
            active.priority_counter += 1;
            format!("stop-playback-{}-{}", stream.stream_id, active.priority_counter)
        };
        let input = MediaPriorityInput {
            priority_id,
            stream: stream.clone(),
            kind: MediaPriorityKind::StopPlayback,
            text: None,
            occurred_at_ms: Some(now_ms()),
            audio: None,
            reason: Some("playback stopped by operator or channel".to_owned()),
        };
        if let Err(error) = self.ingress.admit_priority(input).await {
            tracing::warn!(stream = %stream.stream_id, "stop-playback admission failed: {error}");
        }
    }

    async fn on_session_finished(
        self: &Arc<Self>,
        stream: &MediaStreamDescriptor,
        reason: Option<String>,
    ) {
        self.sessions.lock().await.remove(&stream.stream_id);
        if let Some(reason) = reason {
            tracing::info!(stream = %stream.stream_id, "media session finished: {reason}");
        }
    }
}

/// Publishes the media runtime on the shared Cordis root.
pub struct MediaRuntimePlugin;

impl Plugin for MediaRuntimePlugin {
    type Config = (Arc<MediaIngress>, Arc<Store>, MediaProviders);
    type Input = Arc<MediaRuntime>;
    type PrepareError = Infallible;
    type ApplyError = cordis_core::service::ServicePublishError;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-media-runtime")
    }

    fn prepare(
        &self,
        (ingress, store, providers): Self::Config,
    ) -> Result<Self::Input, Self::PrepareError> {
        Ok(MediaRuntime::new(ingress, store, providers))
    }

    async fn apply(&self, ctx: Context, service: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(service.clone())?;
        Ok(())
    }
}
