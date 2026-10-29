//! Continuous-media transcript ingress service — SPEC §15.2, §20, §41.3.
//!
//! `MediaIngress` is the typed admission surface a transcript provider (call
//! audio, LAN realtime speech, classroom listening, speaker pipeline) calls,
//! in-process or through the UI route. Each admission commits the raw
//! revision fact and the segment's effective observation in ONE ledger
//! transaction, then hands the bound durable event to the SAME subject inbox;
//! there is no second main loop for media. Replays return the stored receipt
//! with the SAME event id and deliver nothing, so a late `send` after a crash
//! reconciles through `event_disposition_commit` instead of duplicating an
//! utterance.
//!
//! This service recognizes no audio itself: it never synthesizes transcript
//! text, never fakes ASR, and speaker claims stay provider claims with
//! confidence/correction/unknown — never chat-user or admin authority.

use std::convert::Infallible;
use std::sync::Arc;

use anyhow::Result;
use cordis_core::{Context, Plugin, Service};
use tokio::sync::mpsc;

use shenko_store::store::Store;
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::media::*;

/// How the bound durable event reached the subject after THIS admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectDelivery {
    /// Nothing to deliver: this call replayed an already committed fact and
    /// the receipt already carries the original bound event id.
    Replay,
    /// The bound event entered the live subject inbox queue this call.
    Queued,
    /// No live queue took the event (loop stopped or not wired). The fact is
    /// durable; the sender replays the same identity after reopen and
    /// reconciles via `event_disposition_commit`, never by fabricating a new
    /// event id.
    Pending,
    /// The queue rejected the event as full. Same durable fact, same retry
    /// rule as `Pending`.
    Rejected,
}

/// Typed outcome of one admission call. `admission` carries the durable
/// receipt; `delivery` reports what happened with the bound inbox event.
#[derive(Debug)]
pub struct MediaDelivery<A> {
    pub admission: A,
    pub delivery: SubjectDelivery,
}

/// The one media ingress service. Published on the shared Cordis root by
/// [`MediaIngressPlugin`]; not a second service locator or a model-facing
/// tool schema.
pub struct MediaIngress {
    store: Arc<Store>,
    agent_id: EntityId,
    inbox: mpsc::Sender<InboxEvent>,
}

impl Service for MediaIngress {
    const NAME: &'static str = "shenko.media.ingress/v1";
}

impl MediaIngress {
    pub fn new(store: Arc<Store>, agent_id: EntityId, inbox: mpsc::Sender<InboxEvent>) -> Self {
        Self {
            store,
            agent_id,
            inbox,
        }
    }

    /// Stable subject inbox identity of a durable media fact. Callers use it
    /// with `Store::event_disposition_commit` to reconcile after a crash or a
    /// rejected queue send, instead of redelivering a phantom event.
    pub fn event_id_of(event: &MediaIngressEvent) -> EntityId {
        event.event_id()
    }

    fn inbox_event(&self, event: MediaIngressEvent) -> InboxEvent {
        let (kind, source) = match &event {
            MediaIngressEvent::Revision { stream, .. } => (
                InboxEventKind::MediaRevision,
                format!("media/{}", stream.provider),
            ),
            MediaIngressEvent::StreamOpened { stream, .. }
            | MediaIngressEvent::StreamState { stream, .. } => (
                InboxEventKind::MediaStream,
                format!("media/{}", stream.provider),
            ),
        };
        InboxEvent {
            event_id: event.event_id(),
            kind,
            payload: serde_json::to_value(&event).expect("media ingress event serialization"),
            created_at: chrono::Utc::now(),
            source,
        }
    }

    async fn deliver(&self, event: MediaIngressEvent) -> SubjectDelivery {
        let event = self.inbox_event(event);
        match self.inbox.try_send(event) {
            Ok(()) => SubjectDelivery::Queued,
            Err(mpsc::error::TrySendError::Closed(_)) => SubjectDelivery::Pending,
            Err(mpsc::error::TrySendError::Full(event)) => {
                // A full queue must not drop the durable fact or block the
                // provider's own connection loop; a bounded wait decides.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    self.inbox.send(event),
                )
                .await
                {
                    Ok(Ok(())) => SubjectDelivery::Queued,
                    Ok(Err(_)) => SubjectDelivery::Pending,
                    Err(_) => SubjectDelivery::Rejected,
                }
            }
        }
    }

    /// Admit one transcript revision and deliver its bound durable event to
    /// the subject. Replays return the stored receipt and `Pending` is never
    /// synthesized for a replay — the caller already owns the event id.
    pub async fn admit_revision(
        &self,
        input: MediaRevisionInput,
    ) -> Result<MediaDelivery<MediaAdmission>, ShenkoError> {
        let admission = self.store.admit_media_revision(self.agent_id, input)?;
        let delivery = match &admission.notify {
            Some(event) => self.deliver(event.clone()).await,
            None => SubjectDelivery::Replay,
        };
        Ok(MediaDelivery {
            admission,
            delivery,
        })
    }

    /// Admit an explicit stream open.
    pub async fn admit_stream(
        &self,
        open: MediaStreamOpen,
    ) -> Result<MediaDelivery<MediaStreamAdmission>, ShenkoError> {
        let admission = self.store.admit_media_stream(self.agent_id, open)?;
        let delivery = match &admission.notify {
            Some(event) => self.deliver(event.clone()).await,
            None => SubjectDelivery::Replay,
        };
        Ok(MediaDelivery {
            admission,
            delivery,
        })
    }

    /// Admit a terminal stream state (cancel/disconnect/end). Committed
    /// segments keep their last effective text; gaps stay gaps.
    pub async fn end_stream(
        &self,
        end: MediaStreamEnd,
    ) -> Result<MediaDelivery<MediaStreamAdmission>, ShenkoError> {
        let admission = self.store.end_media_stream(self.agent_id, end)?;
        let delivery = match &admission.notify {
            Some(event) => self.deliver(event.clone()).await,
            None => SubjectDelivery::Replay,
        };
        Ok(MediaDelivery {
            admission,
            delivery,
        })
    }

    /// Effective transcript of one stream in segment order with the honest
    /// gap list — the reconciliation read a provider or the UI uses.
    pub fn transcript(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Option<MediaTranscript>, ShenkoError> {
        self.store.media_transcript(stream)
    }

    /// The committed stream row, or `None` when this ledger has no fact.
    pub fn stream(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Option<MediaStream>, ShenkoError> {
        self.store.media_stream(stream)
    }
}

/// Publishes the typed ingress service on the shared Cordis root so real
/// provider plugins inject it instead of bypassing admission.
pub struct MediaIngressPlugin;

impl Plugin for MediaIngressPlugin {
    type Config = (Arc<Store>, EntityId, mpsc::Sender<InboxEvent>);
    type Input = Arc<MediaIngress>;
    type PrepareError = Infallible;
    type ApplyError = cordis_core::service::ServicePublishError;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-media-ingress")
    }

    fn prepare(
        &self,
        (store, agent_id, inbox): Self::Config,
    ) -> Result<Self::Input, Self::PrepareError> {
        Ok(Arc::new(MediaIngress::new(store, agent_id, inbox)))
    }

    async fn apply(&self, ctx: Context, service: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(service.clone())?;
        Ok(())
    }
}
