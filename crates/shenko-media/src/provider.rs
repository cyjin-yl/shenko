//! Real media engine adapters — SPEC §20.
//!
//! [`FunAsrWebsocketProvider`] speaks the documented FunASR websocket
//! runtime protocol: after connecting to the server's root websocket path the
//! client sends one JSON control message
//! `{mode, chunk_size, chunk_interval, audio_fs, wav_name, wav_format,
//!   is_speaking, hotwords, itn}`
//! and then streams binary mono PCM16 little-endian frames at the declared
//! `audio_fs`. Server replies are JSON objects
//! `{text, mode, is_final, is_end, wav_name, timestamp, spk_name, spk_score}`
//! where `mode ∈ {online, 2pass-online, 2pass-offline, offline}`; `online`
//! results are partial, `2pass-offline`/`offline` results are final. Ending
//! the stream is `{"is_speaking": false, "is_end": true}` and the server
//! acknowledges with `{"is_end": true, "is_final": true}`.
//!
//! [`OpenAiSpeechOutput`] synthesizes through an OpenAI-compatible
//! `POST /v1/audio/speech` (`{model, input, voice, response_format, speed}`
//! with `Authorization: Bearer <token>`), seals the returned audio bytes in
//! this ledger's CAS, and plays the verified CAS path through a local
//! command player (`ffplay`/`aplay`/`paplay`) whose process is a first-class
//! stop-able child — barge-in kills it, the receipt says so.
//!
//! Speaker identity claims (`spk_name`/`spk_score`) are mapped to
//! [`MediaSpeakerClaim`] verbatim: an empty or absent `spk_name` stays
//! unknown (no claim), a non-empty one carries the engine score as
//! confidence and never becomes identity authority (§35.3).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use shenko_store::cas::ContentStore;
use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError};
use shenko_types::media::*;
use shenko_types::object::ObjectRef;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

/// One session command routed to the engine task.
enum SessionCommand {
    Audio(Vec<u8>),
    Finish(oneshot::Sender<Result<bool, ShenkoError>>),
    Close,
}

fn media_wire_error(code: &str, retry: RetryStrategy, message: impl Into<String>) -> ShenkoError {
    ShenkoError::Protocol(ProtocolError::new(code, ErrorScope::Inbox, retry)
        .with_details(serde_json::json!({ "detail": message.into() })))
}

fn credential_from(binding: &MediaProviderBinding) -> Option<String> {
    binding.credential_env.as_ref().and_then(|name| std::env::var(name).ok())
}

fn model_tag(binding: &MediaProviderBinding) -> String {
    format!("funasr:{}", binding.model.as_deref().unwrap_or("unknown"))
}

/// Map one FunASR result to a speaker claim. The absence of a speaker tag is
/// an explicit unknown, not a default identity.
fn funasr_speaker_claim(value: &serde_json::Value, tag: &str) -> Option<MediaSpeakerClaim> {
    let name = value.get("spk_name").and_then(serde_json::Value::as_str);
    match name {
        Some(name) if !name.is_empty() => Some(MediaSpeakerClaim {
            speaker: Some(name.to_owned()),
            person_id: None,
            confidence: value.get("spk_score").and_then(serde_json::Value::as_f64),
            corrected: false,
            source: Some(tag.to_owned()),
        }),
        _ => None,
    }
}

/// A live realtime session: PCM16 audio in, transcript events out.
#[async_trait]
pub trait RealtimeSession: Send + Sync {
    fn descriptor(&self) -> MediaStreamDescriptor;
    /// Push one chunk of mono PCM16 little-endian audio at the binding's
    /// sample rate.
    async fn push_audio(&self, chunk: &[u8]) -> Result<(), ShenkoError>;
    /// Tell the engine no more audio is coming and wait for its final ack.
    async fn finish(&self) -> Result<(), ShenkoError>;
    /// Close the transport now.
    async fn close(&self) -> Result<(), ShenkoError>;
}

/// The event sink a session reports provider output through. Implemented by
/// the media runtime: each event becomes an admitted, durable ingress fact.
#[async_trait]
pub trait MediaEventSink: Send + Sync {
    async fn on_transcript(&self, stream: &MediaStreamDescriptor, event: MediaTranscriptEvent);
    async fn on_session_finished(&self, stream: &MediaStreamDescriptor, reason: Option<String>);
}

/// A provider that can open realtime media sessions (§20).
#[async_trait]
pub trait RealtimeSessionProvider: Send + Sync {
    fn binding(&self) -> &MediaProviderBinding;
    async fn open(
        &self,
        request: MediaSessionRequest,
        sink: Arc<dyn MediaEventSink>,
    ) -> Result<Arc<dyn RealtimeSession>, ShenkoError>;
}

/// Engine task for one FunASR websocket session.
async fn run_funasr_engine(
    mut ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    mut commands: tokio::sync::mpsc::Receiver<SessionCommand>,
    sink: Arc<dyn MediaEventSink>,
    descriptor: MediaStreamDescriptor,
    control: serde_json::Value,
    tag: String,
) {
    if let Err(error) = ws.send(Message::text(control.to_string())).await {
        tracing::warn!(stream = %descriptor.stream_id, "funasr control send failed: {error}");
        sink.on_session_finished(&descriptor, Some(format!("control send failed: {error}"))).await;
        return;
    }
    let mut finish_waiter: Option<oneshot::Sender<Result<bool, ShenkoError>>> = None;
    loop {
        tokio::select! {
            message = ws.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                    let final_ack = value.get("is_end").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    if final_ack {
                        if let Some(waiter) = finish_waiter.take() {
                            let ok = value.get("is_final").and_then(serde_json::Value::as_bool).unwrap_or(false);
                            let _ = waiter.send(Ok(ok));
                        }
                    }
                    let Some(text_out) = value.get("text").and_then(serde_json::Value::as_str) else { continue };
                    if text_out.is_empty() {
                        continue;
                    }
                    let mode = value.get("mode").and_then(serde_json::Value::as_str).unwrap_or("");
                    let speaker = funasr_speaker_claim(&value, &tag);
                    let provider_facts = serde_json::Map::from_iter([
                        ("mode".into(), mode.into()),
                        ("is_final".into(), value.get("is_final").cloned().unwrap_or(serde_json::Value::Null)),
                        ("wav_name".into(), value.get("wav_name").cloned().unwrap_or(serde_json::Value::Null)),
                    ]);
                    sink.on_transcript(&descriptor, MediaTranscriptEvent {
                        text: text_out.to_owned(),
                        partial: !matches!(mode, "offline" | "2pass-offline"),
                        speaker,
                        captured_at_ms: None,
                        provider_facts,
                    }).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    sink.on_session_finished(&descriptor, None).await;
                    break;
                }
                Some(Err(error)) => {
                    sink.on_session_finished(&descriptor, Some(format!("websocket error: {error}"))).await;
                    break;
                }
                _ => {}
            },
            command = commands.recv() => match command {
                Some(SessionCommand::Audio(bytes)) => {
                    if let Err(error) = ws.send(Message::binary(bytes)).await {
                        tracing::warn!(stream = %descriptor.stream_id, "funasr audio send failed: {error}");
                    }
                }
                Some(SessionCommand::Finish(waiter)) => {
                    let end = serde_json::json!({ "is_speaking": false, "is_end": true }).to_string();
                    if let Err(error) = ws.send(Message::text(end)).await {
                        let _ = waiter.send(Err(media_wire_error(
                            "media_engine_closed", RetryStrategy::AfterReconcile,
                            format!("funasr end-of-input send failed: {error}"))));
                    } else {
                        finish_waiter = Some(waiter);
                    }
                }
                Some(SessionCommand::Close) | None => {
                    let _ = ws.close(None).await;
                    sink.on_session_finished(&descriptor, None).await;
                    break;
                }
            },
        }
    }
    if let Some(waiter) = finish_waiter.take() {
        let _ = waiter.send(Err(media_wire_error(
            "media_engine_closed", RetryStrategy::AfterReconcile,
            "funasr session closed before final ack")));
    }
}

/// FunASR websocket realtime session handle.
pub struct FunAsrSession {
    descriptor: MediaStreamDescriptor,
    commands: tokio::sync::mpsc::Sender<SessionCommand>,
}

#[async_trait]
impl RealtimeSession for FunAsrSession {
    fn descriptor(&self) -> MediaStreamDescriptor {
        self.descriptor.clone()
    }

    async fn push_audio(&self, chunk: &[u8]) -> Result<(), ShenkoError> {
        self.commands
            .send(SessionCommand::Audio(chunk.to_vec()))
            .await
            .map_err(|_| media_wire_error("media_engine_closed", RetryStrategy::AfterReconcile,
                "funasr session engine stopped"))
    }

    async fn finish(&self) -> Result<(), ShenkoError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(SessionCommand::Finish(tx))
            .await
            .map_err(|_| media_wire_error("media_engine_closed", RetryStrategy::AfterReconcile,
                "funasr session engine stopped"))?;
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(true))) => Ok(()),
            Ok(Ok(Ok(false))) => Err(media_wire_error("media_engine_final_failed",
                RetryStrategy::NeverAutomatically, "funasr engine failed to finalize input")),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err(media_wire_error("media_engine_closed", RetryStrategy::AfterReconcile,
                "funasr session engine stopped")),
            Err(_) => Err(media_wire_error("media_engine_final_timeout", RetryStrategy::AfterReconcile,
                "funasr final ack timed out after 30s")),
        }
    }

    async fn close(&self) -> Result<(), ShenkoError> {
        let _ = self.commands.send(SessionCommand::Close).await;
        Ok(())
    }
}

/// FunASR websocket session provider. The binding endpoint is the server
/// root (e.g. `ws://127.0.0.1:10095`); protocol sub-paths stay an engine
/// deployment detail, so the runtime connects to the root exactly as the
/// documented server serves it.
pub struct FunAsrWebsocketProvider {
    binding: MediaProviderBinding,
}

impl FunAsrWebsocketProvider {
    pub fn new(binding: MediaProviderBinding) -> Self {
        Self { binding }
    }
}

#[async_trait]
impl RealtimeSessionProvider for FunAsrWebsocketProvider {
    fn binding(&self) -> &MediaProviderBinding {
        &self.binding
    }

    async fn open(
        &self,
        request: MediaSessionRequest,
        sink: Arc<dyn MediaEventSink>,
    ) -> Result<Arc<dyn RealtimeSession>, ShenkoError> {
        let url = format!("{}/", self.binding.endpoint.trim_end_matches('/'));
        let (ws, _) = connect_async(url.as_str()).await.map_err(|error| media_wire_error(
            "media_engine_unreachable", RetryStrategy::AfterReconcile,
            format!("funasr websocket connect to {url} failed: {error}")))?;
        let option = |name: &str| self.binding.options.get(name).cloned();
        let control = serde_json::json!({
            "mode": option("mode").and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "2pass".into()),
            "chunk_size": option("chunk_size").unwrap_or(serde_json::json!([5, 10, 5])),
            "chunk_interval": option("chunk_interval").and_then(|v| v.as_u64()).unwrap_or(10),
            "encoder_chunk_look_back": option("encoder_chunk_look_back")
                .and_then(|v| v.as_u64()).unwrap_or(4),
            "decoder_chunk_look_back": option("decoder_chunk_look_back")
                .and_then(|v| v.as_u64()).unwrap_or(0),
            "audio_fs": self.binding.sample_rate,
            "wav_name": request.stream.stream_id.clone(),
            "wav_format": "pcm",
            "is_speaking": true,
            "hotwords": option("hotwords").and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(),
            "itn": option("itn").and_then(|v| v.as_bool()).unwrap_or(true),
        });
        let descriptor = request.stream.clone();
        let tag = model_tag(&self.binding);
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(run_funasr_engine(ws, command_rx, sink, descriptor.clone(), control, tag));
        Ok(Arc::new(FunAsrSession { descriptor, commands: command_tx }))
    }
}

/// Speaker identity from a real voiceprint model: engine claim only.
#[async_trait]
pub trait SpeakerIdentity: Send + Sync {
    fn binding(&self) -> &MediaProviderBinding;
    /// Compute the engine's claim for one sealed audio object: speaker tag
    /// and confidence in `[0,1]`, or explicit unknown.
    async fn identify(&self, audio: &ObjectRef) -> Result<MediaSpeakerClaim, ShenkoError>;
}

/// Extract mono PCM16 data and its sample rate from a RIFF/WAVE file
/// produced by [`crate::recording::MediaAudioRecorder`]. Only PCM (format
/// tag 1) is supported; anything else is an honest capability error.
fn pcm16_from_wav(bytes: &[u8]) -> Result<(std::borrow::Cow<'_, [u8]>, u32), ShenkoError> {
    use std::borrow::Cow;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(media_wire_error("media_audio_unsupported", RetryStrategy::NeverAutomatically,
            "sealed audio is not a RIFF/WAVE recording"));
    }
    let mut offset = 12usize;
    let mut pcm: Option<Cow<'_, [u8]>> = None;
    let mut sample_rate = 16000u32;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body_start = offset + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " => {
                if size < 16 || body_end > bytes.len() {
                    return Err(media_wire_error("media_audio_unsupported", RetryStrategy::NeverAutomatically,
                        "malformed WAVE fmt chunk"));
                }
                let format = u16::from_le_bytes(bytes[body_start..body_start + 2].try_into().unwrap());
                if format != 1 {
                    return Err(media_wire_error("media_audio_unsupported", RetryStrategy::NeverAutomatically,
                        format!("unsupported WAVE format tag {format}; only PCM16 is supported")));
                }
                sample_rate = u32::from_le_bytes(bytes[body_start + 4..body_start + 8].try_into().unwrap());
            }
            b"data" => {
                pcm = Some(Cow::Borrowed(&bytes[body_start..body_end]));
            }
            _ => {}
        }
        offset = body_start + size + (size & 1);
    }
    let pcm = pcm.ok_or_else(|| media_wire_error("media_audio_unsupported", RetryStrategy::NeverAutomatically,
        "WAVE file has no data chunk"))?;
    Ok((pcm, sample_rate))
}

/// Speaker identity backed by this ledger's CAS.
pub struct FunAsrSpeakerIdentityInCas {
    binding: MediaProviderBinding,
    cas: Arc<ContentStore>,
}

impl FunAsrSpeakerIdentityInCas {
    pub fn new(binding: MediaProviderBinding, cas: Arc<ContentStore>) -> Self {
        Self { binding, cas }
    }
}

#[async_trait]
impl SpeakerIdentity for FunAsrSpeakerIdentityInCas {
    fn binding(&self) -> &MediaProviderBinding {
        &self.binding
    }

    async fn identify(&self, audio: &ObjectRef) -> Result<MediaSpeakerClaim, ShenkoError> {
        let path = self.cas.verified_object_path(audio)?;
        identify_funasr_pcm(&self.binding, &path).await
    }
}

async fn identify_funasr_pcm(
    binding: &MediaProviderBinding,
    wav_path: &Path,
) -> Result<MediaSpeakerClaim, ShenkoError> {
    let bytes = std::fs::read(wav_path).map_err(ShenkoError::Io)?;
    let (pcm, sample_rate) = pcm16_from_wav(&bytes)?;
    let url = format!("{}/", binding.endpoint.trim_end_matches('/'));
    let (ws, _) = connect_async(url.as_str()).await.map_err(|error| media_wire_error(
        "media_engine_unreachable", RetryStrategy::AfterReconcile,
        format!("funasr websocket connect to {url} failed: {error}")))?;
    let control = serde_json::json!({
        "mode": "offline",
        "chunk_size": serde_json::json!([5, 10, 5]),
        "chunk_interval": 10,
        "audio_fs": sample_rate,
        "wav_name": format!("speaker-{}", wav_path.file_name().and_then(|n| n.to_str()).unwrap_or("audio")),
        "wav_format": "pcm",
        "is_speaking": true,
        "hotwords": "",
        "itn": true,
    });
    let mut ws = ws;
    SinkExt::send(&mut ws, Message::text(control.to_string())).await.map_err(|error| media_wire_error(
        "media_engine_unreachable", RetryStrategy::AfterReconcile,
        format!("funasr control send failed: {error}")))?;
    for frame in pcm.chunks(16000) {
        SinkExt::send(&mut ws, Message::binary(frame.to_vec())).await.map_err(|error| media_wire_error(
            "media_engine_closed", RetryStrategy::AfterReconcile,
            format!("funasr audio send failed: {error}")))?;
    }
    SinkExt::send(&mut ws, Message::text(serde_json::json!({ "is_speaking": false, "is_end": true }).to_string()))
        .await.map_err(|error| media_wire_error("media_engine_closed", RetryStrategy::AfterReconcile,
            format!("funasr end-of-input send failed: {error}")))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let tag = model_tag(binding);
    let mut fallback: Option<MediaSpeakerClaim> = None;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(remaining, ws.next()).await else {
            break;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        if let Some(claim) = funasr_speaker_claim(&value, &tag) {
            if value.get("mode").and_then(serde_json::Value::as_str)
                .is_some_and(|mode| mode.contains("offline"))
            {
                return Ok(claim);
            }
            fallback.get_or_insert(claim);
        }
        if value.get("is_end").and_then(serde_json::Value::as_bool).unwrap_or(false) {
            break;
        }
    }
    Ok(fallback.unwrap_or(MediaSpeakerClaim {
        speaker: None,
        person_id: None,
        confidence: None,
        corrected: false,
        source: Some(tag),
    }))
}

/// What one playback actually did — the agent's voice is interruptible
/// (§20) so the receipt must distinguish finished from barge-in-stopped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MediaPlaybackReceipt {
    Finished { object: ObjectRef },
    Stopped { object: ObjectRef },
    Failed { object: ObjectRef, detail: String },
}

/// Synthesized audio sealed in CAS, ready to play.
#[derive(Debug, Clone)]
pub struct SynthesizedSpeech {
    pub object: ObjectRef,
    pub media_type: String,
}

/// Speech output: synthesize into CAS, play the verified CAS path, stop the
/// current playback for barge-in.
#[async_trait]
pub trait AudioOutput: Send + Sync {
    fn binding(&self) -> &MediaProviderBinding;
    async fn synthesize(&self, request: &MediaSpeechRequest) -> Result<MediaSpeechAudio, ShenkoError>;
    async fn play(&self, object: &ObjectRef) -> Result<MediaPlaybackReceipt, ShenkoError>;
    /// Barge-in: stop the current playback. Returns true when a playback was
    /// actually stopped.
    async fn stop_playback(&self) -> Result<bool, ShenkoError>;
    fn is_playing(&self) -> bool;
}

/// OpenAI-compatible `/v1/audio/speech` output with local command playback.
///
/// The player command must accept the audio file path as its final argument
/// (`ffplay -nodisp -autoexit <path>`, `aplay <path>`, `paplay <path>`).
pub struct OpenAiSpeechOutput {
    binding: MediaProviderBinding,
    client: reqwest::Client,
    cas: Arc<ContentStore>,
    /// Player command argv; the verified CAS path is appended as the final
    /// argument.
    player: Vec<String>,
    active: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

impl OpenAiSpeechOutput {
    pub fn new(binding: MediaProviderBinding, cas: Arc<ContentStore>) -> Self {
        let player = binding.options.get("player_command")
            .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
            .unwrap_or_else(|| vec![
                "ffplay".to_owned(), "-nodisp".to_owned(), "-autoexit".to_owned(),
                "-loglevel".to_owned(), "error".to_owned(),
            ]);
        Self {
            binding,
            client: reqwest::Client::new(),
            cas,
            player,
            active: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl AudioOutput for OpenAiSpeechOutput {
    fn binding(&self) -> &MediaProviderBinding {
        &self.binding
    }

    async fn synthesize(&self, request: &MediaSpeechRequest) -> Result<MediaSpeechAudio, ShenkoError> {
        let model = request.model.clone()
            .or_else(|| self.binding.model.clone())
            .unwrap_or_else(|| "tts-1".to_owned());
        let response_format = request.response_format.clone()
            .unwrap_or_else(|| "wav".to_owned());
        let mut body = serde_json::json!({
            "model": model,
            "input": request.text,
            "response_format": response_format,
        });
        if let Some(voice) = &request.voice {
            body["voice"] = serde_json::Value::String(voice.clone());
        }
        if let Some(speed) = request.speed {
            body["speed"] = serde_json::Value::from(speed);
        }
        let url = format!("{}/v1/audio/speech", self.binding.endpoint.trim_end_matches('/'));
        let mut http = self.client.post(&url).json(&body);
        if let Some(token) = credential_from(&self.binding) {
            http = http.header("Authorization", format!("Bearer {token}"));
        }
        let response = http.send().await.map_err(|error| media_wire_error(
            "media_speech_unreachable", RetryStrategy::AfterReconcile,
            format!("speech request to {url} failed: {error}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(media_wire_error("media_speech_rejected", RetryStrategy::NeverAutomatically,
                format!("speech provider returned {status}: {detail}")));
        }
        let bytes = response.bytes().await.map_err(|error| media_wire_error(
            "media_speech_incomplete", RetryStrategy::AfterReconcile,
            format!("speech response body incomplete: {error}")))?;
        let media_type = match response_format.as_str() {
            "mp3" => "audio/mpeg",
            "opus" => "audio/opus",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            _ => "audio/wav",
        };
        let sealed = self.cas.seal_object(&bytes, media_type, "media-speech", "speech-audio", 1)?;
        Ok(MediaSpeechAudio {
            request_id: request.request_id.clone(),
            object: sealed.object,
            media_type: media_type.to_owned(),
            provider: self.binding.provider.clone(),
            model: Some(model),
            voice: request.voice.clone(),
        })
    }

    async fn play(&self, object: &ObjectRef) -> Result<MediaPlaybackReceipt, ShenkoError> {
        let path = self.cas.verified_object_path(object)?;
        let Some(program) = self.player.first() else {
            return Err(media_wire_error("media_player_unavailable", RetryStrategy::NeverAutomatically,
                "no playback command configured"));
        };
        let mut command = Command::new(program);
        for arg in self.player.iter().skip(1) {
            command.arg(arg);
        }
        let child: std::result::Result<Child, std::io::Error> = command
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = child.map_err(|error| media_wire_error("media_player_unavailable",
            RetryStrategy::NeverAutomatically, format!("failed to spawn {program}: {error}")))?;
        self.active.store(true, Ordering::SeqCst);
        let outcome = tokio::select! {
            status = child.wait() => match status {
                Ok(status) if status.success() => MediaPlaybackReceipt::Finished { object: object.clone() },
                Ok(status) => MediaPlaybackReceipt::Failed {
                    object: object.clone(),
                    detail: format!("player exited with {status}"),
                },
                Err(error) => MediaPlaybackReceipt::Failed {
                    object: object.clone(),
                    detail: format!("player wait failed: {error}"),
                },
            },
            _ = self.stop_notify.notified() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                MediaPlaybackReceipt::Stopped { object: object.clone() }
            }
        };
        self.active.store(false, Ordering::SeqCst);
        Ok(outcome)
    }

    async fn stop_playback(&self) -> Result<bool, ShenkoError> {
        if !self.active.swap(false, Ordering::SeqCst) {
            return Ok(false);
        }
        self.stop_notify.notify_one();
        Ok(true)
    }

    fn is_playing(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}
