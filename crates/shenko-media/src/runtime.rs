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
//!
//! Two properties the session keeps in its own lineage:
//!
//! - the provider owns the segment boundary whenever it reports one
//!   (`MediaTranscriptEvent::segment`). A revision that re-reports a segment
//!   already finalized is a CORRECTION of that utterance — same segment,
//!   same original audio reference, never a fabricated second utterance. Only
//!   an index-less provider gets the runtime's derived boundary.
//! - the recording SCOPE is a permission, not a suggestion: bytes are
//!   captured and sealed only while an admitted recording declares
//!   `store_audio: true`. Capture that was already armed still seals what it
//!   heard, and a `stop` never discards it.

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
use crate::{MediaDelivery, MediaIngress, SubjectDelivery};

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
    ///
    /// Each entry names the protocol it speaks (`options.protocol`, defaulting
    /// to the capability's documented protocol); a protocol this build does
    /// not speak, a zero sample rate on a capture binding, a duplicate realtime
    /// provider or a second speech/speaker binding is refused instead of being
    /// silently driven as FunASR or silently overriding the earlier entry.
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
        let mut occupied: Vec<(String, MediaProviderCapability)> = Vec::new();
        for binding in bindings {
            validate_media_binding(&binding)?;
            // Speech output and speaker identity are single slots on this
            // runtime, so a second binding of either kind would silently
            // override the first: refuse instead.
            let single_slot = !matches!(
                binding.capability,
                MediaProviderCapability::RealtimeSession
            );
            let clashes = occupied.iter().any(|(provider, capability)| {
                (provider == &binding.provider && *capability == binding.capability)
                    || (single_slot && *capability == binding.capability)
            });
            if clashes {
                return Err(binding_error(
                    "media_binding_duplicate",
                    format!(
                        "provider {} cannot add another {:?} binding; that slot is already taken",
                        binding.provider, binding.capability
                    ),
                ));
            }
            occupied.push((binding.provider.clone(), binding.capability));
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

fn binding_error(code: &str, message: impl Into<String>) -> ShenkoError {
    ShenkoError::Protocol(shenko_types::error::ProtocolError {
        code: code.to_owned(),
        scope: shenko_types::error::ErrorScope::Inbox,
        retry: shenko_types::error::RetryStrategy::NeverAutomatically,
        operation_id: None,
        commit_ref: None,
        details: serde_json::json!({ "detail": message.into() }),
    })
}

/// One binding must describe a protocol this build actually speaks, at a
/// usable sample rate. A recording at `sample_rate: 0` cannot produce a WAVE
/// duration and a binding under an unknown protocol would be driven with the
/// wrong wire format: both are refused before any session opens.
fn validate_media_binding(binding: &MediaProviderBinding) -> Result<(), ShenkoError> {
    let protocol = binding
        .options
        .get("protocol")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(match binding.capability {
            MediaProviderCapability::RealtimeSession | MediaProviderCapability::SpeakerIdentity => {
                "funasr"
            }
            MediaProviderCapability::SpeechOutput => "openai_audio_speech",
        });
    let supported: &[&str] = match binding.capability {
        MediaProviderCapability::RealtimeSession | MediaProviderCapability::SpeakerIdentity => {
            &["funasr"]
        }
        MediaProviderCapability::SpeechOutput => &["openai_audio_speech"],
    };
    if !supported.contains(&protocol) {
        return Err(binding_error(
            "media_binding_protocol_unsupported",
            format!(
                "provider {} declares protocol {protocol}; this build speaks {}",
                binding.provider,
                supported.join(", ")
            ),
        ));
    }
    // Capture and voiceprint bindings declare the engine's PCM rate; it is
    // what the recorder writes into the WAVE header, so zero is unusable.
    // A speech-output binding may leave the rate to the engine.
    if binding.sample_rate == 0
        && matches!(
            binding.capability,
            MediaProviderCapability::RealtimeSession | MediaProviderCapability::SpeakerIdentity
        )
    {
        return Err(binding_error(
            "media_binding_invalid",
            format!("provider {} declares sample_rate 0", binding.provider),
        ));
    }
    Ok(())
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
    /// The subset of live recordings whose admitted scope declares that the
    /// ORIGINAL audio bytes are sealed into this ledger's CAS. A recording
    /// whose scope says `store_audio: false` keeps its durable state visible
    /// while no byte is stored: the kernel never widens a declared scope.
    audio_ids: HashSet<String>,
    /// Next segment index the runtime derives when the provider reports none.
    /// A provider that reports its own segment owns the boundary; this value
    /// is never used for it.
    derived_segment: u64,
    /// Highest segment index whose captured audio interval is still open.
    /// A final that closes an interval owned by a LATER segment must not
    /// attribute the bytes being held right now to that late segment.
    audio_cursor: u64,
    /// Per-segment lineage: revisions, finality, original audio and the
    /// engine's own last speaker claim. This is what makes a later provider
    /// revision a correction of the SAME utterance instead of a fabricated
    /// second one.
    segments: HashMap<u64, SegmentLineage>,
    revision_no: u64,
    /// Most recent engine speaker claim of any segment (status read only).
    last_speaker: Option<MediaSpeakerClaim>,
    priority_counter: u64,
}

/// What the runtime knows about one transcript segment of a live stream.
///
/// The provider owns the segment identity whenever it reports one
/// (`MediaTranscriptEvent::segment`); the runtime only derives an index from
/// final boundaries when it does not. Everything here is derived from facts
/// the provider or this ledger already stated: no text, no speaker claim and
/// no audio is invented.
#[derive(Debug, Default)]
struct SegmentLineage {
    /// A final revision is this segment's current observation.
    finalized: bool,
    /// The segment's original audio, once sealed in this ledger's CAS.
    audio: Option<MediaAudioRef>,
    /// The last seal of this segment was cut at the recorder's PCM bound, so
    /// the same segment's audio continues with its next final.
    truncated: bool,
    /// The engine's last speaker claim for THIS segment. `corrected` is a
    /// per-segment property: a different speaker in the next utterance is a
    /// different speaker, not a correction of this one.
    last_speaker: Option<MediaSpeakerClaim>,
}

impl ActiveMediaSession {
    /// True while at least one admitted recording arms this session's
    /// capture.
    fn is_recording(&self) -> bool {
        !self.recording_ids.is_empty()
    }

    /// True while at least one live recording declared that original audio
    /// bytes are stored. Capture that stores nothing must not run at all.
    fn stores_audio(&self) -> bool {
        !self.audio_ids.is_empty()
    }

    fn next_revision_no(&mut self) -> u64 {
        self.revision_no += 1;
        self.revision_no
    }

    /// Decide what original audio a final revision of `segment` carries and
    /// seal the interval it closes.
    ///
    /// - a provider re-report of an already closed segment CORRECTS the same
    ///   utterance: it keeps that segment's original bytes and never seals
    ///   the audio held now (that audio belongs to a later interval);
    /// - a final whose segment is older than the interval being held is
    ///   equally late: it keeps whatever that segment already had instead of
    ///   mislabelling audio the provider did not attest to;
    /// - otherwise the final closes the interval captured since the previous
    ///   seal, and the sealed bytes are the segment's original audio.
    fn close_segment_audio(
        &mut self,
        segment: u64,
        cas: &shenko_store::cas::ContentStore,
    ) -> Result<(Option<MediaAudioRef>, bool), ShenkoError> {
        let lineage = self.segments.entry(segment).or_default();
        if lineage.finalized && !lineage.truncated {
            return Ok((lineage.audio.clone(), false));
        }
        if segment < self.audio_cursor {
            return Ok((lineage.audio.clone(), false));
        }
        // Bytes captured while capture was armed are sealed even when the
        // recording stopped in the meantime: an admitted stop ends future
        // capture, it never discards audio already heard.
        let Some(recorder) = self.recorder.as_mut() else {
            self.audio_cursor = segment.saturating_add(1);
            return Ok((self.segments.entry(segment).or_default().audio.clone(), false));
        };
        match recorder.seal_segment(cas)? {
            Some(sealed) => {
                let object = MediaAudioRef::Sealed {
                    object: sealed.object.clone(),
                };
                let truncated = sealed.truncated;
                self.audio_cursor = if truncated {
                    segment
                } else {
                    segment.saturating_add(1)
                };
                let lineage = self.segments.entry(segment).or_default();
                lineage.audio = Some(object.clone());
                lineage.truncated = truncated;
                Ok((Some(object), truncated))
            }
            None => {
                self.audio_cursor = segment.saturating_add(1);
                Ok((self.segments.entry(segment).or_default().audio.clone(), false))
            }
        }
    }

    /// The engine's claim for this revision, with `corrected` derived from
    /// THIS segment's previous claim. The engine's own `corrected` flag is
    /// preserved; the model score stays a claim, never an identity.
    fn claim_speaker(&mut self, segment: u64, claim: MediaSpeakerClaim) -> MediaSpeakerClaim {
        let lineage = self.segments.entry(segment).or_default();
        let corrected = claim.corrected
            || lineage
                .last_speaker
                .as_ref()
                .is_some_and(|previous| previous != &claim);
        let claim = MediaSpeakerClaim { corrected, ..claim };
        lineage.last_speaker = Some(claim.clone());
        self.last_speaker = Some(claim.clone());
        claim
    }

    /// Record one admitted revision in the segment's lineage. Only called
    /// after the ledger committed the fact.
    fn record_admitted(&mut self, segment: u64, partial: bool) {
        let lineage = self.segments.entry(segment).or_default();
        if !partial {
            lineage.finalized = true;
            self.derived_segment = self.derived_segment.max(segment.saturating_add(1));
        }
    }
}

/// Read-side snapshot of one open session (UI/reconciliation).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaSessionStatus {
    pub stream: MediaStreamDescriptor,
    /// Next segment the runtime would derive for a provider that reports no
    /// index of its own.
    pub segment: u64,
    /// A recording is live (its durable state says `recording`).
    pub recording: bool,
    /// The live scope actually stores original audio bytes in this CAS.
    pub storing_audio: bool,
    /// Distinct segments with at least one admitted revision.
    pub segments: usize,
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
            // The declared scope decides whether bytes are stored: a
            // recording that declares `store_audio: false` is still a real,
            // visible recording whose transcript is admitted, but no audio
            // byte is sealed and no recorder runs.
            let stores_audio = scope.store_audio;
            let recorder = stores_audio
                .then(|| MediaAudioRecorder::new(descriptor.clone(), sample_rate));
            let session = provider
                .open(request, Arc::new(RuntimeSink(self.clone())))
                .await?;
            self.sessions.lock().await.insert(
                descriptor.stream_id.clone(),
                ActiveMediaSession {
                    descriptor: descriptor.clone(),
                    session,
                    recorder,
                    sample_rate,
                    recording_ids: HashSet::from([recording_id.clone()]),
                    audio_ids: if stores_audio {
                        HashSet::from([recording_id])
                    } else {
                        HashSet::new()
                    },
                    derived_segment: 1,
                    audio_cursor: 1,
                    segments: HashMap::new(),
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
                audio_ids: HashSet::new(),
                derived_segment: 1,
                audio_cursor: 1,
                segments: HashMap::new(),
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
    /// recording whose declared scope stores original audio is live, the
    /// same bytes are captured for the segment seal. A recording whose scope
    /// says `store_audio: false` is live but stores nothing.
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
            let recorder = if active.stores_audio() {
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
                if result.is_ok() && active.stores_audio() {
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
    /// live session. `Start` arms the recording — and arms capture only when
    /// the ADMITTED scope declares that original audio is stored, creating
    /// the recorder when the session opened without one; `Stop` releases
    /// that recording's arm so audio received after the admitted stop is not
    /// sealed as if the recording had continued, while bytes already held
    /// still close with the next segment-final revision. The durable notice
    /// commits first: only a newly admitted (non-replayed) control changes
    /// live capture, so the recorder and the ledger never disagree, and
    /// applying the same control twice stays idempotent. A `Start` captures
    /// only when the scope IT declared stores audio — that is the scope the
    /// ledger committed for this recording.
    pub async fn control_recording(
        self: &Arc<Self>,
        control: MediaRecordingControl,
    ) -> Result<MediaDelivery<MediaRecordingAdmission>, ShenkoError> {
        let stream_id = control.stream.stream_id.clone();
        let recording_id = control.recording_id.clone();
        let action = control.action;
        let stores_audio = control
            .scope
            .as_ref()
            .is_some_and(|scope| scope.store_audio);
        let outcome = self.ingress.admit_recording(control).await?;
        if !outcome.admission.receipt.replayed {
            let mut sessions = self.sessions.lock().await;
            if let Some(active) = sessions.get_mut(&stream_id) {
                match action {
                    MediaRecordingAction::Stop => {
                        active.recording_ids.remove(&recording_id);
                        // The arm only stops FUTURE capture; the recorder keeps
                        // the bytes already heard so the next segment-final
                        // revision still seals them.
                        active.audio_ids.remove(&recording_id);
                    }
                    MediaRecordingAction::Start => {
                        active.recording_ids.insert(recording_id.clone());
                        if stores_audio {
                            active.audio_ids.insert(recording_id);
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
            segment: active.derived_segment,
            recording: active.is_recording(),
            storing_audio: active.stores_audio(),
            segments: active.segments.len(),
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

    /// Ask the configured speaker-identity engine about one segment's
    /// ORIGINAL sealed audio and admit the engine's claim as a revision of
    /// that SAME segment (§15.2, §20, §35.3, §51.1, §51.2).
    ///
    /// The engine reads the bytes this ledger already sealed: the recording
    /// is never re-captured, re-downloaded or re-transcribed, and no audio
    /// byte leaves the ledger. The admitted revision keeps the retained
    /// provider TEXT verbatim (the model never rewrites what was said) and
    /// the original audio reference, so the segment's text, its audio and
    /// the claim stay one traceable observation. The claim is a model signal
    /// with its confidence; it never becomes identity authority.
    ///
    /// The observation result is content: [`MediaIdentificationContent`]
    /// names the sealed audio, the revision that sealed it, the perception
    /// that read it and the claim, and is sealed in this ledger's CAS. Its
    /// digest derives the revision identity, so the SAME bytes are ONE
    /// observation: a reconcile or a repeated engine answer returns the
    /// stored receipt instead of minting another revision (§51.1「字节没变就
    /// 不另开一版」). The revision's own disposition (applied / correcting /
    /// superseded) is the attitude; it never changes these bytes.
    pub async fn identify_speaker(
        self: &Arc<Self>,
        stream: &MediaStreamDescriptor,
        segment: u64,
    ) -> Result<MediaDelivery<MediaAdmission>, ShenkoError> {
        let speaker = self
            .providers
            .speaker
            .as_ref()
            .ok_or_else(|| ShenkoError::not_found("no speaker identity binding configured"))?;
        let effective = self
            .store
            .media_effective_segment(stream, segment)?
            .ok_or_else(|| {
                ShenkoError::not_found(format!(
                    "no admitted revision for segment {segment}"
                ))
            })?;
        let audio = effective.audio.clone().ok_or_else(|| {
            ShenkoError::not_found(
                "this segment carries no original audio reference to identify",
            )
        })?;
        let object = match &audio {
            MediaAudioRef::Sealed { object } => object.clone(),
            MediaAudioRef::Provider(provider) => {
                return Err(ShenkoError::not_found(format!(
                    "provider-local audio handle {} is not archived; seal the bytes before identifying",
                    provider.locator
                )));
            }
        };
        // The engine's claim on the sealed bytes. Nothing is re-created.
        let claim = speaker.identify(&object).await?;
        let corrected = claim.corrected
            || effective
                .speaker
                .as_ref()
                .is_some_and(|previous| previous != &claim);
        let claim = MediaSpeakerClaim { corrected, ..claim };
        // Read the segment again AFTER the engine answered: a provider may
        // have corrected the utterance while the engine was reading it, and
        // the revision number this admission really takes must be the next
        // free one.
        let revisions = self.store.media_revisions(stream, segment)?;
        // §51.2: the result's reason stops at the observation that produced
        // this audio — the revision that sealed these exact bytes. Later
        // interpretations of the same utterance stay later, and a re-run of
        // the identification does not move the reason onto itself.
        let observed_revision_id = revisions
            .iter()
            .find(|revision| match &revision.audio {
                Some(MediaAudioRef::Sealed { object: sealed }) => {
                    sealed.digest == object.digest && sealed.bytes == object.bytes
                }
                _ => false,
            })
            .map(|revision| revision.revision_id.clone())
            .unwrap_or_else(|| effective.revision_id.clone());
        // §51.2「记录谁的 perception」: the configured binding and the engine
        // identity it reported, not a claim that this is Shenko's own unaided
        // reading.
        let binding = speaker.binding();
        let perception = MediaPerceptionRef {
            provider: binding.provider.clone(),
            account: binding.account.clone(),
            model: binding.model.clone(),
            reported_source: claim.source.clone(),
        };
        // §51.1: one immutable content identity per observation result.
        let content = self
            .store
            .cas
            .seal_object(
                &serde_json::to_vec(&MediaIdentificationContent {
                    schema_version: 1,
                    stream: stream.clone(),
                    segment,
                    observed_audio: object.clone(),
                    observed_revision_id: observed_revision_id.clone(),
                    perception: perception.clone(),
                    claim: claim.clone(),
                })
                .map_err(ShenkoError::Serde)?,
                "application/json",
                "media-identification",
                "media-identification-content-v1",
                1,
            )?
            .object;
        let revision_id = format!(
            "{}:s{segment}:identify:{}",
            stream.stream_id,
            content.digest.hex()
        );
        if let Some(recorded) = revisions
            .iter()
            .find(|revision| revision.revision_id == revision_id)
        {
            // This exact observation already holds this identity: the stored
            // receipt is the answer. No second revision and no second wake.
            return Ok(MediaDelivery {
                admission: MediaAdmission {
                    receipt: MediaRevisionReceipt {
                        revision_id: recorded.revision_id.clone(),
                        disposition: recorded.disposition,
                        replayed: true,
                        event_id: recorded.event_id,
                        commit_ref: recorded.admitted_commit.clone(),
                    },
                    notify: None,
                },
                delivery: SubjectDelivery::Replay,
            });
        }
        let next_revision = revisions
            .iter()
            .map(|revision| revision.revision)
            .max()
            .unwrap_or(effective.revision)
            .saturating_add(1);
        let mut provider_facts = serde_json::Map::new();
        provider_facts.insert(
            "identification".to_owned(),
            serde_json::to_value(MediaIdentification {
                schema_version: 1,
                perception,
                observed_audio: object,
                observed_revision_id,
                content,
            })
            .map_err(ShenkoError::Serde)?,
        );
        let outcome = self
            .ingress
            .admit_revision(MediaRevisionInput {
                revision_id,
                stream: stream.clone(),
                segment,
                revision: next_revision,
                text: effective.text.clone(),
                // Identification states WHO spoke, never whether the
                // utterance finished: a segment still awaiting its final
                // keeps the interim status it already had.
                partial: effective.partial,
                captured_at_ms: effective.captured_at_ms,
                speaker: Some(claim.clone()),
                audio: Some(audio),
                provider_facts,
            })
            .await?;
        if !outcome.admission.receipt.replayed {
            let mut sessions = self.sessions.lock().await;
            if let Some(active) = sessions.get_mut(&stream.stream_id) {
                let lineage = active.segments.entry(segment).or_default();
                lineage.last_speaker = Some(claim.clone());
                if !effective.partial {
                    lineage.finalized = true;
                }
                active.last_speaker = Some(claim);
            }
        }
        Ok(outcome)
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
            // The provider owns the segment boundary when it reports one —
            // a classroom or call engine that decodes its own utterances
            // keeps them, and a re-report of a closed segment stays a
            // correction of THAT utterance. Only an index-less provider gets
            // the runtime's derived boundary (a final opens the next one).
            let segment = event.segment.unwrap_or(active.derived_segment);
            let revision = active.next_revision_no();
            let revision_id = format!("{}:s{segment}:r{revision}", stream.stream_id);
            let (audio, truncated) = if event.partial {
                (None, false)
            } else {
                active.close_segment_audio(segment, &self.store.cas)?
            };
            let speaker = event
                .speaker
                .map(|claim| active.claim_speaker(segment, claim));
            (revision_id, segment, revision, audio, speaker, truncated)
        };
        let mut provider_facts = event.provider_facts;
        if truncated {
            provider_facts.insert("recording_truncated".to_owned(), serde_json::Value::Bool(true));
        }
        // A final that repeats an already-finalized segment is the provider
        // correcting the SAME utterance; the store records it as
        // `final_correction` and retains both raw facts.
        let partial = event.partial;
        self.ingress
            .admit_revision(MediaRevisionInput {
                revision_id,
                stream: stream.clone(),
                segment,
                revision,
                text: event.text,
                partial,
                captured_at_ms: event.captured_at_ms,
                speaker,
                audio,
                provider_facts,
            })
            .await?;
        // Only a committed fact enters the live lineage: a rejected
        // admission never advances segment finality.
        let mut sessions = self.sessions.lock().await;
        if let Some(active) = sessions.get_mut(&stream.stream_id) {
            active.record_admitted(segment, partial);
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
        let still_live = self
            .sessions
            .lock()
            .await
            .remove(&stream.stream_id)
            .is_some();
        if let Some(reason) = &reason {
            tracing::info!(stream = %stream.stream_id, "media session finished: {reason}");
        }
        if !still_live {
            // `close_session` already owns this stream's terminal notice.
            return;
        }
        // The transport ended without the provider stating a terminal cause
        // (engine crash, socket loss, control send failure). Leaving the
        // durable stream `open` would hide the disconnect from the subject,
        // so the honest terminal fact is recorded — a later explicit end may
        // still settle it under a NEW end identity (§20 取消/断线).
        let end = MediaStreamEnd {
            end_id: format!("disconnect-{}-{}", stream.stream_id, EntityId::new()),
            stream: stream.clone(),
            state: MediaStreamState::Disconnected,
            ended_at_ms: Some(now_ms()),
            reason,
        };
        if let Err(error) = self.ingress.end_stream(end).await {
            tracing::warn!(
                stream = %stream.stream_id,
                "media disconnect notice not admitted: {error}"
            );
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
