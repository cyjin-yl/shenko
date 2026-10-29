//! HTTP ingress for transcript providers — SPEC §15.2, §20.
//!
//! These routes expose the SAME durable admission the native `MediaIngress`
//! service uses: raw revision fact + effective observation committed in one
//! ledger transaction, bound subject event delivered at most once, replays
//! returning the stored receipt with the same event id. The responses are
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
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, put},
};
use serde::Deserialize;

use shenko_media::MediaIngress;
use shenko_store::store::Store;
use shenko_types::error::ShenkoError;
use shenko_types::media::*;

/// Route state. `ingress` is required for admissions; `store` alone already
/// serves the read-only effective transcript.
#[derive(Clone, Default)]
pub struct MediaRouteState {
    pub ingress: Option<Arc<MediaIngress>>,
    pub store: Option<Arc<Store>>,
}

pub fn router(state: MediaRouteState) -> Router {
    Router::new()
        .route("/api/media/ingress/revision", put(admit_revision))
        .route("/api/media/ingress/stream", put(admit_stream))
        .route("/api/media/ingress/stream/end", put(end_stream))
        .route("/api/media/stream", get(stream_view))
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
