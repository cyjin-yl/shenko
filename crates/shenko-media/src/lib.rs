//! Continuous-media transcript ingress service — SPEC §15.2, §20, §41.3.
//!
//! `MediaIngress` is the typed admission surface a transcript provider (call
//! audio, LAN realtime speech, classroom listening, speaker pipeline) calls,
//! in-process or through the UI route. Each admission commits the raw
//! revision fact, the segment's effective observation and the bound durable
//! `inbox_events` original in ONE ledger transaction, then hands a volatile
//! wake to the SAME subject inbox; there is no second main loop for media.
//! A lost wake costs nothing: the subject's pending drain recovers the
//! durable original under the same event id. Replays return the stored
//! receipt with the SAME event id and deliver nothing, so a late `send`
//! after a crash reconciles through `event_disposition_commit` instead of
//! duplicating an utterance.
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
use shenko_subject::inbox::InboxEvent;
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::media::*;

/// How the bound durable event reached the subject after THIS admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectDelivery {
    /// No new wake on this replay. The original event id stays bound to the
    /// already committed row, which may still await the subject's drain.
    Replay,
    /// The bound event entered the live subject inbox queue this call (the
    /// volatile wake succeeded; the durable row is already committed).
    Queued,
    /// No live queue took the wake (loop stopped or not wired). The durable
    /// inbox row was committed with the fact, so the subject's normal pending
    /// drain delivers the same event identity on a later step; the sender may
    /// also reconcile via `event_disposition_commit`, never by fabricating a
    /// new event id.
    Pending,
    /// The queue rejected the wake as full. Same durable row and same
    /// recovery rule as `Pending`.
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
    /// rejected queue send; the durable `inbox_events` row the admission
    /// committed already delivers the same identity through the normal
    /// pending drain, so no caller ever fabricates a second event.
    pub fn event_id_of(event: &MediaIngressEvent) -> EntityId {
        event.event_id()
    }

    fn deliver(&self, event_id: EntityId) -> Result<SubjectDelivery, ShenkoError> {
        let original = self.store.media_inbox_event(self.agent_id, event_id)?
            .ok_or_else(|| ShenkoError::not_found("admitted media inbox original"))?;
        let event: InboxEvent = serde_json::from_str(&original)?;
        Ok(match self.inbox.try_send(event) {
            Ok(()) => SubjectDelivery::Queued,
            Err(mpsc::error::TrySendError::Closed(_)) => SubjectDelivery::Pending,
            // This is only a wake. The normal pending drain recovers the
            // already committed original without blocking this producer.
            Err(mpsc::error::TrySendError::Full(_)) => SubjectDelivery::Rejected,
        })
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
            Some(event) => self.deliver(event.event_id())?,
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
            Some(event) => self.deliver(event.event_id())?,
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
            Some(event) => self.deliver(event.event_id())?,
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
