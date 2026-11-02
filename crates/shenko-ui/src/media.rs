//! HTTP ingress for transcript providers — SPEC §15.2, §20.
//!
//! These routes expose the SAME durable admission the native `MediaIngress`
//! service uses: raw revision fact + effective observation committed in one
//! ledger transaction with the complete durable inbox original. Live sends
//! are only wakes; the shared pending drain recovers the same event identity.
//! Replays return the stored original receipt. The responses are
//! honest about delivery: a fact can be durable while the subject queue is
//! full or closed, and that is reported as `pending`/`rejected`, never as a
//! fabricated `queued`.
//!
//! The routes never recognize audio: providers POST real transcript text;
//! `sealed` audio refs must point at objects already sealed in this ledger's
//! CAS.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, put},
};
use serde::Deserialize;

use shenko_media::MediaIngress;
use shenko_media::runtime::MediaRuntime;
use shenko_store::store::Store;
use shenko_types::error::ShenkoError;
use shenko_types::media::*;

/// Route state. `ingress` is required for admissions; `store` alone already
/// serves the read-only effective transcript.
#[derive(Clone, Default)]
pub struct MediaRouteState {
    pub ingress: Option<Arc<MediaIngress>>,
    pub runtime: Option<Arc<MediaRuntime>>,
    pub store: Option<Arc<Store>>,
}

pub fn router(state: MediaRouteState) -> Router {
    Router::new()
        .route("/api/media/ingress/revision", put(admit_revision))
        .route("/api/media/ingress/stream", put(admit_stream))
        .route("/api/media/ingress/stream/end", put(end_stream))
        .route("/api/media/stream", get(stream_view))
        .route("/api/media/session", put(open_session).post(close_session))
        .route("/api/media/session/audio", put(push_audio))
        .route("/api/media/session/status", get(session_status))
        .route("/api/media/recording", put(control_recording).get(recording_view))
        .route("/api/media/priority", put(admit_priority).get(priority_view))
        .route("/api/media/speech", put(synthesize_speech))
        .route("/api/media/playback/stop", put(stop_playback))
        .with_state(state)
}

fn unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "available": false,
            "detail": "media ingress is not wired to a live subject on this node",
        })),
    )
}

fn error_response(error: ShenkoError) -> (StatusCode, Json<serde_json::Value>) {
    match &error {
        ShenkoError::Protocol(protocol) => {
            let status = match protocol.code.as_str() {
                "media_invalid_input" | "media_audio_object_missing" => StatusCode::BAD_REQUEST,
                "media_revision_conflict"
                | "media_stream_conflict"
                | "media_stream_closed"
                | "media_stream_agent_mismatch" => StatusCode::CONFLICT,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            (
                status,
                Json(serde_json::json!({
                    "code": protocol.code,
                    "retry": protocol.retry,
                    "detail": protocol.details.get("detail").cloned().unwrap_or_default(),
                })),
            )
        }
        ShenkoError::Storage(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "code": "storage", "detail": message })),
        ),
        other => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "code": "invalid", "detail": other.to_string() })),
        ),
    }
}

async fn admit_revision(
    State(state): State<MediaRouteState>,
    Json(input): Json<MediaRevisionInput>,
) -> impl IntoResponse {
    let Some(ingress) = state.ingress else {
        return unavailable();
    };
    match ingress.admit_revision(input).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "receipt": outcome.admission.receipt,
                "delivery": outcome.delivery,
            })),
        ),
        Err(error) => error_response(error),
    }
}

/// `PUT /api/media/session` with a `MediaSessionRequest` body opens a realtime
/// session against a configured engine binding and returns the durable stream
/// descriptor. No binding for the provider is an explicit 404, never a fake
/// session.
async fn open_session(
    State(state): State<MediaRouteState>,
    Json(request): Json<MediaSessionRequest>,
) -> impl IntoResponse {
    let Some(runtime) = state.runtime else {
        return unavailable();
    };
    match runtime.open_session(request).await {
        Ok(descriptor) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "stream": descriptor,
            })),
        ),
        Err(error) => match &error {
            ShenkoError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "code": "media_binding_missing", "detail": error.to_string() })),
            ),
            _ => error_response(error),
        },
    }
}

#[derive(Debug, Deserialize)]
struct CloseSessionQuery {
    provider: String,
    account: String,
    stream_id: String,
    reason: Option<String>,
}

/// `POST /api/media/session?provider=&account=&stream_id=` ends the durable
/// stream row and closes the transport.
async fn close_session(
    State(state): State<MediaRouteState>,
    Query(query): Query<CloseSessionQuery>,
) -> impl IntoResponse {
    let Some(runtime) = state.runtime else {
        return unavailable();
    };
    let descriptor = MediaStreamDescriptor {
        provider: query.provider,
        account: query.account,
        stream_id: query.stream_id,
    };
    match runtime.close_session(&descriptor, query.reason).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "closed": true }))),
        Err(error) => error_response(error),
    }
}

#[derive(Debug, Deserialize)]
struct AudioQuery {
    provider: String,
    account: String,
    stream_id: String,
}

/// `PUT /api/media/session/audio?...` with a raw PCM16 little-endian body
/// feeds one chunk into the live session (and its recorder, when armed).
async fn push_audio(
    State(state): State<MediaRouteState>,
    Query(query): Query<AudioQuery>,
    body: Body,
) -> impl IntoResponse {
    let Some(runtime) = state.runtime else {
        return unavailable();
    };
    let descriptor = MediaStreamDescriptor {
        provider: query.provider,
        account: query.account,
        stream_id: query.stream_id,
    };
    let Ok(chunk) = axum::body::to_bytes(body, 1024 * 1024).await else {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({ "code": "media_audio_chunk_too_large", "detail": "chunks are bounded to 1 MiB" })),
        );
    };
    match runtime.push_audio(&descriptor, &chunk).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "pushed": chunk.len() }))),
        Err(error) => error_response(error),
    }
}

/// `GET /api/media/session/status?...` → live session bookkeeping (segment,
/// revision count, armed recorder). `null` when no live session.
async fn session_status(
    State(state): State<MediaRouteState>,
    Query(query): Query<StreamQuery>,
) -> impl IntoResponse {
    let Some(runtime) = state.runtime else {
        return unavailable();
    };
    let descriptor = MediaStreamDescriptor {
        provider: query.provider,
        account: query.account,
        stream_id: query.stream_id,
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "schema_version": 1,
            "session": runtime.session_status(&descriptor).await,
        })),
    )
}

/// `PUT /api/media/recording` with a `MediaRecordingControl` body. The
/// control is a durable admitted fact immediately; capture arms/stops with
/// the session's audio flow.
async fn control_recording(
    State(state): State<MediaRouteState>,
    Json(control): Json<MediaRecordingControl>,
) -> impl IntoResponse {
    let Some(ingress) = state.ingress else {
        return unavailable();
    };
    match ingress.admit_recording(control).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "receipt": outcome.admission.receipt,
                "delivery": outcome.delivery,
            })),
        ),
        Err(error) => error_response(error),
    }
}

#[derive(Debug, Deserialize)]
struct RecordingQuery {
    provider: String,
    account: String,
    stream_id: String,
    recording_id: String,
}

/// `GET /api/media/recording?...&recording_id=` → durable recording row and
/// its sealed-audio usage.
async fn recording_view(
    State(state): State<MediaRouteState>,
    Query(query): Query<RecordingQuery>,
) -> impl IntoResponse {
    let Some(store) = state.store else {
        return unavailable();
    };
    let descriptor = MediaStreamDescriptor {
        provider: query.provider,
        account: query.account,
        stream_id: query.stream_id,
    };
    match store.media_recording(&descriptor, &query.recording_id) {
        Ok(recording) => {
            let usage = store
                .media_recording_usage(&descriptor, &query.recording_id)
                .unwrap_or(MediaRecordingUsage {
                    audio_bytes: 0,
                    audio_segments: 0,
                    revisions: 0,
                });
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "schema_version": 1,
                    "recording": recording,
                    "usage": usage,
                })),
            )
        }
        Err(error) => error_response(error),
    }
}

/// `PUT /api/media/priority` with a `MediaPriorityInput` body. A
/// `stop_playback` input also stops the live playback immediately.
async fn admit_priority(
    State(state): State<MediaRouteState>,
    Json(input): Json<MediaPriorityInput>,
) -> impl IntoResponse {
    let Some(ingress) = state.ingress else {
        return unavailable();
    };
    let stop_playback = input.kind == MediaPriorityKind::StopPlayback;
    if stop_playback {
        if let Some(runtime) = &state.runtime {
            let _ = runtime.stop_playback().await;
        }
    }
    match ingress.admit_priority(input).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "receipt": outcome.admission.receipt,
                "delivery": outcome.delivery,
            })),
        ),
        Err(error) => error_response(error),
    }
}

/// `GET /api/media/priority?...` → admitted priority inputs of the stream.
async fn priority_view(
    State(state): State<MediaRouteState>,
    Query(query): Query<StreamQuery>,
) -> impl IntoResponse {
    let Some(store) = state.store else {
        return unavailable();
    };
    let descriptor = MediaStreamDescriptor {
        provider: query.provider,
        account: query.account,
        stream_id: query.stream_id,
    };
    match store.media_priority_inputs(&descriptor) {
        Ok(inputs) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "inputs": inputs,
            })),
        ),
        Err(error) => error_response(error),
    }
}

#[derive(Debug, Deserialize)]
struct SpeechQuery {
    play: Option<bool>,
}

/// `PUT /api/media/speech?play=true` with a `MediaSpeechRequest` body
/// synthesizes through the configured speech binding; the sealed audio is
/// returned by digest and played when requested. Playback is interruptible
/// by far-side barge-in.
async fn synthesize_speech(
    State(state): State<MediaRouteState>,
    Query(query): Query<SpeechQuery>,
    Json(request): Json<MediaSpeechRequest>,
) -> impl IntoResponse {
    let Some(runtime) = state.runtime else {
        return unavailable();
    };
    let play = query.play.unwrap_or(false);
    match runtime.speak(request, play).await {
        Ok(audio) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "audio": audio,
                "playing": play,
            })),
        ),
        Err(error) => match &error {
            ShenkoError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "code": "media_speech_unconfigured", "detail": error.to_string() })),
            ),
            _ => error_response(error),
        },
    }
}

/// `PUT /api/media/playback/stop` stops the current playback now.
async fn stop_playback(
    State(state): State<MediaRouteState>,
) -> impl IntoResponse {
    let Some(runtime) = state.runtime else {
        return unavailable();
    };
    match runtime.stop_playback().await {
        Ok(stopped) => (StatusCode::OK, Json(serde_json::json!({ "stopped": stopped }))),
        Err(error) => error_response(error),
    }
}

async fn admit_stream(
    State(state): State<MediaRouteState>,
    Json(open): Json<MediaStreamOpen>,
) -> impl IntoResponse {
    let Some(ingress) = state.ingress else {
        return unavailable();
    };
    match ingress.admit_stream(open).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "receipt": outcome.admission.receipt,
                "delivery": outcome.delivery,
            })),
        ),
        Err(error) => error_response(error),
    }
}

async fn end_stream(
    State(state): State<MediaRouteState>,
    Json(end): Json<MediaStreamEnd>,
) -> impl IntoResponse {
    let Some(ingress) = state.ingress else {
        return unavailable();
    };
    match ingress.end_stream(end).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "receipt": outcome.admission.receipt,
                "delivery": outcome.delivery,
            })),
        ),
        Err(error) => error_response(error),
    }
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    provider: String,
    account: String,
    stream_id: String,
}

/// `GET /api/media/stream?provider=&account=&stream_id=` → the durable stream
/// row and effective transcript with the honest gap list. `null` when this
/// ledger holds no fact; `available:false` without a store handle.
async fn stream_view(
    State(state): State<MediaRouteState>,
    Query(query): Query<StreamQuery>,
) -> impl IntoResponse {
    let Some(store) = state.store else {
        return unavailable();
    };
    let descriptor = MediaStreamDescriptor {
        provider: query.provider,
        account: query.account,
        stream_id: query.stream_id,
    };
    match store.media_transcript(&descriptor) {
        Ok(transcript) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "schema_version": 1,
                "available": true,
                "transcript": transcript,
            })),
        ),
        Err(error) => error_response(error),
    }
}
