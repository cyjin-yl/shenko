//! Continuous-media / transcript revision ingress types — SPEC §15.2, §20.
//!
//! A transcript provider (call audio, LAN realtime speech, classroom
//! listening, recording review, speaker-identity pipeline) reports one typed
//! revision per observation attempt. The store keeps every raw revision plus
//! the latest effective observation atomically; a superseded or contradictory
//! revision is an explicit durable fact, never a silently dropped row and
//! never a second contradictory utterance.
//!
//! Nothing in these types asserts that ASR happened: `text`, `speaker` and
//! `audio` are the *provider's* claims and provenance. Shenko never
//! synthesizes transcript text and never presents a synthetic transcript as
//! real recognition output. Speaker claims carry confidence, unknown and
//! correction semantics and never grant chat-user or admin authority.

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::EntityId;
use crate::object::ObjectRef;

/// Join key inside opaque store keys. Providers must not embed this byte in
/// identity fields; admission rejects it so keys stay unambiguous.
pub const MEDIA_KEY_SEPARATOR: char = '\u{1f}';

/// Identity of one continuous-media provider stream (§15.1 provider/plugin,
/// account/tenant, conversation scope). `(provider, account, stream_id)` is
/// the durable stream identity; a terminal stream's identity is never reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaStreamDescriptor {
    /// Provider/plugin identity, e.g. "phone-sip", "lan-voice", "classroom-asr".
    pub provider: String,
    /// Provider account/tenant the stream was captured under (§15.1).
    pub account: String,
    /// Provider-scoped stream/session identity; unique per provider stream.
    pub stream_id: String,
}

/// Opaque store key for one stream identity.
pub fn media_stream_key(stream: &MediaStreamDescriptor) -> String {
    format!(
        "{}{}{}{}{}",
        stream.provider, MEDIA_KEY_SEPARATOR, stream.account, MEDIA_KEY_SEPARATOR, stream.stream_id
    )
}

/// Opaque store key for one segment inside a stream.
pub fn media_segment_key(stream: &MediaStreamDescriptor, segment: u64) -> String {
    format!(
        "{}{}{}",
        media_stream_key(stream),
        MEDIA_KEY_SEPARATOR,
        segment
    )
}

/// A provider-local object handle for the audio behind a revision. It is
/// provenance, not archive: the provider must keep the locator resolvable, or
/// first seal the bytes into this ledger's CAS and send
/// [`MediaAudioRef::Sealed`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderObjectRef {
    pub locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Audio provenance behind one transcript revision (§20 转写/原音频引用).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaAudioRef {
    /// Bytes already sealed in this ledger's CAS. Admission verifies the
    /// object exists with the declared digest and size BEFORE the metadata
    /// commit (§41.2.2: object first, reference second).
    Sealed { object: ObjectRef },
    /// Provider-local handle; recorded verbatim and never treated as an
    /// archived object of this ledger.
    Provider(ProviderObjectRef),
}

/// Optional speaker claim (§15.2 声纹/说话人识别).
///
/// A claim is provider/model output kept with confidence, correction and
/// unknown semantics. It is recorded for context suggestion only: it never
/// elevates to user identity, admin authority or SSH permission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSpeakerClaim {
    /// Provider-side speaker tag (e.g. "speaker-2", diarization cluster id).
    /// `None` with the claim present = the provider explicitly reports the
    /// speaker as unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// Optional claim that links this segment to an internal person id.
    /// This is an unverified association claim, not an identity binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<EntityId>,
    /// Model confidence in `[0.0, 1.0]`; `None` = the provider reported no
    /// score. A score is a model signal, not a real identity probability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// True when the provider corrected an earlier claim on this segment.
    #[serde(default)]
    pub corrected: bool,
    /// The recognizer/speaker-model identity and revision that produced the
    /// claim, so later model swaps stay auditable (§15.2 model_revision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One typed transcript revision (partial or final) as provided by a real
/// recognizer or session provider. Durable admission is idempotent on
/// `(stream, revision_id)`: a replay resends the same identity and receives
/// the original receipt without creating a second fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaRevisionInput {
    /// Provider-scoped unique revision identity. Replays resend it verbatim;
    /// a different fact under the same identity is a typed conflict.
    pub revision_id: String,
    pub stream: MediaStreamDescriptor,
    /// Provider segment identity: the unit one utterance line revises.
    pub segment: u64,
    /// Provider monotone revision order inside the segment.
    pub revision: u64,
    /// The segment text the provider currently claims. Only real provider
    /// output is admitted; the kernel never generates this text.
    pub text: String,
    /// True = this revision is a partial/interim claim; false = the provider
    /// claims the segment finalized with this revision.
    pub partial: bool,
    /// Provider capture time, epoch milliseconds. `None` = the provider did
    /// not report a capture clock; never fabricated locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<MediaSpeakerClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<MediaAudioRef>,
    /// Structured provider facts (VAD marks, language, model id…). Stored
    /// verbatim for the subject; never interpreted by the kernel.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub provider_facts: serde_json::Map<String, serde_json::Value>,
}

/// How one admitted revision relates to the segment's effective observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaRevisionDisposition {
    /// Applied; the first admitted revision of its segment.
    Applied,
    /// Applied; supersedes a non-final effective revision (normal
    /// partial→partial or partial→final progression).
    AppliedSupersedes,
    /// Applied over an already-final effective revision — an explicit provider
    /// contradiction/correction, retained and rendered as such, never merged
    /// silently.
    FinalCorrection,
    /// Retained raw but NOT effective: arrived after a newer revision was
    /// already applied for this segment. Out-of-order evidence stays
    /// readable; it never retroactively rewrites the effective observation.
    Superseded,
}

/// One retained raw revision row, exactly as committed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMediaRevision {
    pub revision_id: String,
    pub segment: u64,
    pub revision: u64,
    pub text: String,
    pub partial: bool,
    pub captured_at_ms: Option<i64>,
    pub speaker: Option<MediaSpeakerClaim>,
    pub audio: Option<MediaAudioRef>,
    pub provider_facts: serde_json::Map<String, serde_json::Value>,
    pub disposition: MediaRevisionDisposition,
    /// Revision number this one replaced, when it was applied over an
    /// existing effective revision.
    pub supersedes_revision: Option<u64>,
    /// Revision number that later replaced this one, when it is not the
    /// current effective observation.
    pub superseded_by_revision: Option<u64>,
    /// The durable subject-inbox event identity bound to this fact. Stable
    /// across replays so senders can reconcile consumption.
    pub event_id: EntityId,
    /// Wall-clock admission time of THIS ledger, epoch milliseconds.
    pub admitted_at_ms: i64,
    pub admitted_commit: CommitRef,
}

/// Durable stream state (§20 session scope and media cancellation/disconnect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaStreamState {
    Open,
    /// Provider-side cancellation: the stream stopped on purpose.
    Cancelled,
    /// The provider connection was lost; the provider may later report
    /// `Ended`, or open a NEW stream identity to continue.
    Disconnected,
    /// The stream completed normally.
    Ended,
}

impl MediaStreamState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Open)
    }
}

/// Explicit stream-open admission. Opening declares the session scope and its
/// provider descriptor before the first revision; providers that only send
/// revisions create the stream implicitly (recorded as `implicit`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaStreamOpen {
    /// Provider-scoped idempotency identity for the open notice.
    pub open_id: String,
    pub stream: MediaStreamDescriptor,
    /// Provider facts bound at open (call id, class label, direction,
    /// remote party claims…). Retained verbatim; never interpreted.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub descriptor: serde_json::Map<String, serde_json::Value>,
}

/// Terminal stream state as reported by the provider. Admitting the end of a
/// stream the store has not seen yet creates the durable stream row in the
/// reported terminal state, so a lost open delivery stays an honest gap
/// instead of fabricating an open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaStreamEnd {
    /// Provider-scoped idempotency identity for this terminal notice.
    pub end_id: String,
    pub stream: MediaStreamDescriptor,
    /// Must be terminal (`cancelled`, `disconnected`, `ended`); `open` is
    /// rejected as invalid input.
    pub state: MediaStreamState,
    /// Provider end time, epoch milliseconds; `None` = unreported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The committed stream row, exactly as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaStream {
    pub stream: MediaStreamDescriptor,
    pub agent_id: EntityId,
    pub state: MediaStreamState,
    /// Ledger wall clock of the first admission that created this row.
    pub opened_at_ms: i64,
    /// Terminal admission details; `None` while the stream is open.
    pub end: Option<MediaStreamEnd>,
    /// True when the row was created by the first revision/end delivery
    /// rather than an explicit `MediaStreamOpen`.
    pub implicit: bool,
    /// Verbatim provider descriptor from the explicit open, if any.
    pub descriptor: serde_json::Map<String, serde_json::Value>,
    pub commit_seq: u64,
}

/// The one durable subject event bound to a newly committed media fact.
/// `notify` is `None` on replays so replays never duplicate utterances; the
/// returned `event_id` stays stable for reconciliation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaIngressEvent {
    StreamOpened {
        event_id: EntityId,
        stream: MediaStreamDescriptor,
        descriptor: serde_json::Map<String, serde_json::Value>,
        opened_at_ms: i64,
    },
    Revision {
        event_id: EntityId,
        stream: MediaStreamDescriptor,
        record: StoredMediaRevision,
        /// The segment's effective revision BEFORE this admission: the row
        /// this revision superseded, or — for a `superseded` late arrival —
        /// the still-current effective row. `None` for a segment's first
        /// revision. Embedding it keeps the event self-describing so the
        /// subject projection never guesses what this revision replaced.
        prior_effective: Option<Box<StoredMediaRevision>>,
    },
    StreamState {
        event_id: EntityId,
        stream: MediaStreamDescriptor,
        state: MediaStreamState,
        reason: Option<String>,
        ended_at_ms: Option<i64>,
        /// Number of segments holding at least one admitted revision.
        segments: u64,
    },
}

impl MediaIngressEvent {
    pub fn event_id(&self) -> EntityId {
        match self {
            Self::StreamOpened { event_id, .. }
            | Self::Revision { event_id, .. }
            | Self::StreamState { event_id, .. } => *event_id,
        }
    }

    pub fn stream(&self) -> &MediaStreamDescriptor {
        match self {
            Self::StreamOpened { stream, .. }
            | Self::Revision { stream, .. }
            | Self::StreamState { stream, .. } => stream,
        }
    }
}

/// Durable receipt of one revision admission (§41.3 CommitReceipt shape:
/// commit ref plus the bound event identity).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRevisionReceipt {
    pub revision_id: String,
    pub disposition: MediaRevisionDisposition,
    /// True when the same durable identity was already admitted: the store
    /// returned the original receipt and created no new fact.
    pub replayed: bool,
    /// The subject inbox event bound to this fact. Replays return the SAME
    /// event id so senders reconcile consumption instead of redelivering a
    /// phantom.
    pub event_id: EntityId,
    pub commit_ref: CommitRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaAdmission {
    pub receipt: MediaRevisionReceipt,
    /// Present iff this commit admitted a NEW fact; the caller delivers it to
    /// the subject inbox exactly once.
    pub notify: Option<MediaIngressEvent>,
}

/// Durable receipt of a stream open/terminal admission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaStreamReceipt {
    pub stream: MediaStreamDescriptor,
    pub state: MediaStreamState,
    pub replayed: bool,
    /// Bound subject event id; stable across replays. `None` only when the
    /// stream row existed already (e.g. created implicitly by a revision) and
    /// this admission generated no new fact.
    pub event_id: Option<EntityId>,
    pub commit_ref: CommitRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaStreamAdmission {
    pub receipt: MediaStreamReceipt,
    pub notify: Option<MediaIngressEvent>,
}

/// One segment's effective observation plus its retained revision count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveSegment {
    pub segment: u64,
    pub effective: StoredMediaRevision,
    /// Count of retained raw revisions for this segment, effective included.
    pub revisions: u64,
    /// True when any admitted revision superseded an already-final revision.
    pub had_final_correction: bool,
}

/// Effective transcript of one stream in segment order, plus the honest gap
/// list: segment numbers between the smallest and largest admitted segment
/// that hold no revision at all. A cancelled or disconnected stream keeps
/// whatever final text was committed; gaps are reported, never filled by
/// fabrication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaTranscript {
    pub stream: MediaStream,
    /// Inclusive `[first_missing, last_missing]` ranges between admitted
    /// segments.
    pub gaps: Vec<(u64, u64)>,
    pub segments: Vec<EffectiveSegment>,
}
