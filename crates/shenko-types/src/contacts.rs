//! Contact / person directory protocol types — SPEC §15.2, §15.2.1, §16, §20.
//!
//! Requirements carried here: R059 (stable person ids, aliases, cross-platform
//! mapping), R060 (voiceprint is an optional identity *clue* that keeps
//! confidence, unknown and mis-identification semantics), R102 (reuse Life's
//! existing identity and voiceprint material — recording scope, candidates,
//! revisions — while keeping the human-set admin boundary), R104 (the
//! subject's personal knowledge is not audience-filtered; relationships are
//! the subject's own judgement, never a score).
//!
//! Four separations are load-bearing and are expressed by the types
//! themselves, not by convention:
//!
//! 1. **A claim is not a fact.** Every statement about a person is a
//!    [`PersonClaim`] carrying provenance, a [`Confidence`] that may be
//!    *unknown*, and an explicit [`PersonClaimState`]. Nothing here promotes
//!    a model inference, a nickname or a voiceprint score to a fact.
//! 2. **A person is not an account.** A [`PlatformAccount`] is a claim that
//!    points at a person. The person id is the stable internal identity; it is
//!    never a nickname, a provider id or a display name.
//! 3. **Recording-local ids are not person ids.** Life's `rank_id` and
//!    `cluster_id` are scoped to one recording (§15.2.1) and never identify a
//!    person on their own; only an explicit confirmation links them.
//! 4. **Scope is provenance, not an audience filter.** [`ClaimScope`] records
//!    *where* a fact was learned. It never hides the fact from the subject
//!    (§6 D1); what the subject says to whom stays the subject's judgement.
//!
//! The admin fact is deliberately *not* a claim: it is a separate, explicitly
//! human-set record ([`AdminFact`]) so that no familiarity score, group
//! status, platform role or voiceprint match can produce one.

use serde::{Deserialize, Serialize};

use crate::commit::CommitRef;
use crate::ids::EntityId;

/// Join key separator inside opaque store keys. Identity fields must not
/// contain it, so a key is always unambiguous.
pub const CONTACTS_KEY_SEPARATOR: char = '\u{1f}';

/// Stable internal person id. Renaming a person, adding an account or
/// learning a new fact never changes it; a merge/split event changes which
/// person id is *current* without rewriting the original attribution.
pub type PersonId = EntityId;

/// A platform account coordinate (§15.1: provider account/tenant plus the
/// platform's own stable user id). Accounts are opaque strings; a QQ number,
/// an OpenID or a Feishu chat id are never forced into an integer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformAccount {
    /// Provider/plugin id, e.g. `napcat`, `qq-official`, `feishu`.
    pub provider: String,
    /// Provider account namespace the session is bound to.
    pub account: String,
    /// Tenant/app namespace; empty when the provider has none.
    #[serde(default)]
    pub tenant: String,
    /// The provider's own stable id for the human.
    pub platform_user_id: String,
}

impl PlatformAccount {
    /// Opaque, unambiguous store key for one account coordinate.
    pub fn key(&self) -> String {
        format!(
            "{}{}{}{}{}{}{}",
            self.provider,
            CONTACTS_KEY_SEPARATOR,
            self.account,
            CONTACTS_KEY_SEPARATOR,
            self.tenant,
            CONTACTS_KEY_SEPARATOR,
            self.platform_user_id
        )
    }
}

/// Who or what asserted a claim. The variants keep the difference between
/// "the platform said so", "a human said so", "the subject guessed" and "a
/// provider produced a signal" visible in the durable record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClaimSource {
    /// The platform's own account fact.
    PlatformFact { provider: String, account: String },
    /// A human stated it (the owner, or an identified chat user).
    HumanStatement {
        actor: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    /// The subject's own inference. It stays a candidate until a human or an
    /// authoritative source confirms it.
    ModelInference {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<String>,
    },
    /// The Life adapter (`cyjin-yl/life` people catalog / voiceprint
    /// registry), §15.2.1.
    LifeAdapter {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        life_person_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_revision: Option<String>,
    },
    /// A media provider's speaker claim (§15.2, §20).
    MediaProvider {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_revision: Option<String>,
    },
    /// The local operator acting explicitly in the owner's own session.
    Operator,
}

/// Provenance of one claim: what asserted it and where the evidence can be
/// read back. Evidence is a reference (object ref, message id, catalog
/// path), never a copy of somebody's private material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub source: ClaimSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    /// Provider/observation time, epoch milliseconds. `None` = not reported;
    /// never fabricated locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<i64>,
}

/// How a claim's confidence is grounded. `Unknown` is a first-class state:
/// "the provider produced no score" is not the same as "low confidence".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBasis {
    /// Authoritative for that kind of fact (a platform account id, an
    /// owner-set fact).
    Verified,
    /// Stated by a human.
    Reported,
    /// Inferred by a model or a heuristic; never silently promoted.
    Inferred,
    /// Explicitly unknown.
    Unknown,
}

/// Confidence of one claim. `value` is a model/provider signal in `[0, 1]`,
/// not a real-world probability of identity (§20).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    pub basis: ConfidenceBasis,
}

impl Confidence {
    pub fn unknown() -> Self {
        Self { value: None, basis: ConfidenceBasis::Unknown }
    }

    pub fn verified() -> Self {
        Self { value: None, basis: ConfidenceBasis::Verified }
    }

    pub fn reported() -> Self {
        Self { value: None, basis: ConfidenceBasis::Reported }
    }

    pub fn inferred(value: Option<f64>) -> Self {
        Self { value, basis: ConfidenceBasis::Inferred }
    }

    /// True when the claim carries no usable confidence at all.
    pub fn is_unknown(&self) -> bool {
        self.basis == ConfidenceBasis::Unknown && self.value.is_none()
    }
}

/// What kind of statement a claim makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonClaimKind {
    /// The person's own record: display name and other profile facts.
    Profile,
    /// A name or nickname used in some scope.
    Alias,
    /// A platform account bound to this person.
    PlatformAccount,
    /// A relationship to the owner (role, kind, note).
    Relationship,
    /// A speaker/voiceprint observation. A clue only (§15.2, R060).
    SpeakerObservation,
    /// A Life voiceprint rank record linked to this person (§15.2.1).
    LifeIdentity,
    /// A stable id owned by another system (for example a Life person slug).
    ExternalIdentity,
    /// A link to a memory entry that holds the narrative about this person.
    MemoryAnchor,
    /// A link to a task that tracks something owed to this person.
    TaskAnchor,
}

impl PersonClaimKind {
    /// Stable machine name of the kind. It is part of the slot identity, so
    /// it must not change once claims exist.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Alias => "alias",
            Self::PlatformAccount => "platform_account",
            Self::Relationship => "relationship",
            Self::SpeakerObservation => "speaker_observation",
            Self::LifeIdentity => "life_identity",
            Self::ExternalIdentity => "external_identity",
            Self::MemoryAnchor => "memory_anchor",
            Self::TaskAnchor => "task_anchor",
        }
    }
}

/// The state of one claim revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonClaimState {
    /// Current and accepted.
    Active,
    /// A candidate association that is not confirmed yet.
    Candidate,
    /// Explicitly confirmed by a human or an authoritative source.
    Confirmed,
    /// Explicitly rejected by a human.
    Rejected,
}

impl PersonClaimState {
    /// Stable machine name of the state, as stored in the ledger.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Candidate => "candidate",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
        }
    }
}

/// Where a fact was learned. This is provenance metadata, never an access
/// control: the subject's personal knowledge is not audience-filtered (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimScopeKind {
    Private,
    Conversation,
    Recording,
    Platform,
    Catalog,
    Operator,
}

/// The scope a claim was learned in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimScope {
    pub kind: ClaimScopeKind,
    /// Scope-specific reference (conversation id, recording scope, provider…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

impl ClaimScope {
    pub fn private() -> Self {
        Self { kind: ClaimScopeKind::Private, reference: None }
    }
}

/// One durable claim about a person.
///
/// Claims are append-only revisions: a slot (`slot_id`) holds the revision
/// history of one fact, and the highest revision is the current one. Nothing
/// rewrites an earlier revision, so "who said this, when" stays readable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonClaim {
    /// Identity of this revision.
    pub claim_id: EntityId,
    /// Identity of the fact slot the revision belongs to.
    pub slot_id: EntityId,
    pub agent_id: EntityId,
    /// Attribution at the time the claim was made. A merge/split event
    /// changes the *current* owner without rewriting this field.
    pub person_id: PersonId,
    pub kind: PersonClaimKind,
    /// Stable key inside the kind, e.g. an account key or `alias|<scope>|<text>`.
    pub claim_key: String,
    /// Revision number inside the slot, starting at 1.
    pub revision: u64,
    pub payload: serde_json::Value,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub state: PersonClaimState,
    pub scope: ClaimScope,
    pub created_at_ms: i64,
    pub commit_seq: u64,
}

/// What a caller submits to record one claim revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonClaimInput {
    pub person_id: PersonId,
    pub kind: PersonClaimKind,
    pub claim_key: String,
    pub payload: serde_json::Value,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub state: PersonClaimState,
    pub scope: ClaimScope,
    /// Explicit slot identity. `None` derives a globally unique slot from
    /// `(kind, claim_key)`; pass one when the key may legitimately exist for
    /// more than one person (for example a shared nickname).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<EntityId>,
}

/// Durable receipt of one claim admission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonClaimReceipt {
    pub claim_id: EntityId,
    pub slot_id: EntityId,
    pub person_id: PersonId,
    pub revision: u64,
    /// True when the same fact was already admitted: the original revision is
    /// returned and nothing new is written.
    pub replayed: bool,
    pub commit_ref: CommitRef,
}

/// What happened to a person's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonIdentityEventKind {
    /// The person record was created.
    Created,
    /// Another person's claims now belong to the target person.
    Merged,
    /// Selected claims were returned to their original owner.
    Split,
}

/// An append-only identity event. Merge and split never rewrite the claims
/// they touch: the current owner of a fact is the replay of these events over
/// the original attribution (§15.2 "错误合并可拆分，历史来源不被改写").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonIdentityEvent {
    pub event_id: EntityId,
    pub agent_id: EntityId,
    pub kind: PersonIdentityEventKind,
    /// The person that lost or regained the claims.
    pub source_person_id: PersonId,
    /// The person that holds the claims now.
    pub target_person_id: PersonId,
    /// Slots moved by a merge.
    #[serde(default)]
    pub moved_slot_ids: Vec<EntityId>,
    /// Slots returned by a split.
    #[serde(default)]
    pub returned_slot_ids: Vec<EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub provenance: Provenance,
    pub occurred_at_ms: i64,
    pub commit_seq: u64,
}

/// What the admin fact is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminSubjectKind {
    Person,
    PlatformAccount,
}

/// An explicitly human-set admin fact.
///
/// It is never derived from familiarity, group status, a platform role, a
/// voiceprint match or an inference, and it is never a chat admission
/// condition: ordinary users can talk (§16). Only the owner sets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminFact {
    pub agent_id: EntityId,
    pub subject_kind: AdminSubjectKind,
    /// Person id or platform account key.
    pub subject_key: String,
    pub granted: bool,
    /// Who set it. Only an explicit human decision may appear here.
    pub granted_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub updated_at_ms: i64,
    pub commit_seq: u64,
}

/// What the owner submits to set an admin fact. Only a human decision may
/// appear in `granted_by`; the store rejects an empty actor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminFactInput {
    pub subject_kind: AdminSubjectKind,
    /// Person id or platform account key.
    pub subject_key: String,
    pub granted: bool,
    pub granted_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The resolved view of one person: the current canonical id, the persons
/// merged into it and the current revision of every claim that belongs to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonView {
    /// Canonical (post merge/split) person id.
    pub person_id: PersonId,
    /// Person ids whose claims were merged into this one.
    pub merged_from: Vec<PersonId>,
    /// Current revision of every claim owned by this person.
    pub claims: Vec<PersonClaim>,
    /// The owner-set admin fact, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminFact>,
}

/// Candidate status of one Life voiceprint candidate (§15.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifeCandidateStatus {
    /// Explicitly confirmed by the user or by a consenting reference sample.
    Confirmed,
    Suspected,
    Unresolved,
}

/// One candidate inside a Life voiceprint rank record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifeRankCandidate {
    /// The stable Life person id this candidate points at, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub life_person_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub status: LifeCandidateStatus,
    /// Where the evidence for this candidate lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    /// Free-text confidence note from the source document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_note: Option<String>,
    /// Similarity signal, when the source recorded one. It is a model
    /// output, not an identity probability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// One Life voiceprint rank record (§15.2.1).
///
/// `rank_id` tracks a candidate speaker across recordings; `cluster_id` is
/// the recording-local automatic clustering number and may change on a
/// re-run. Both are scoped by `recording_scope` and neither is a person id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifeRankRecord {
    pub rank_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<String>,
    /// Recording the rank was observed in. Rank and cluster numbers are
    /// recording-local and are meaningless outside it.
    pub recording_scope: String,
    /// The registry revision that produced this record. It increases with
    /// every data addition and is never overwritten.
    pub model_revision: String,
    #[serde(default)]
    pub candidates: Vec<LifeRankCandidate>,
    /// Only an explicit user confirmation may fill this in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_identity: Option<String>,
}

/// Durable receipt of one Life voiceprint rank admission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifeRankReceipt {
    pub recording_scope: String,
    pub rank_id: String,
    pub model_revision: String,
    /// Number of person link claims admitted for this rank.
    pub links: usize,
    /// True when this exact record was already admitted.
    pub replayed: bool,
    pub commit_ref: CommitRef,
}

/// One entry of the Life people catalog (§15.2.1). The catalog keeps the
/// stable id in front matter and never uses a real name as the key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifePersonEntry {
    /// The catalog's stable id, e.g. `person-advisor-wangxu`.
    pub life_person_id: String,
    /// Explicit name field, when the catalog provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The catalog's heading, recorded verbatim as evidence text. The adapter
    /// does not guess that a heading is a legal name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_heading: Option<String>,
    /// Relation to the user, as the catalog states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_to_user: Option<String>,
    /// The catalog's own privacy marker, kept as provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// Where the entry can be read back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
}

/// Deterministic id derived from a domain and its parts.
///
/// It gives imported material a stable internal id that does not depend on
/// import order: the same Life person always maps to the same Shenko person.
pub fn deterministic_entity_id(domain: &str, parts: &[&str]) -> EntityId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(&[0u8]);
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    EntityId(uuid::Uuid::from_bytes(bytes))
}

/// Slot identity for a fact that belongs to exactly one person.
pub fn person_slot_id(
    agent_id: EntityId,
    kind: PersonClaimKind,
    person_id: PersonId,
    claim_key: &str,
) -> EntityId {
    deterministic_entity_id(
        "shenko.contacts.slot.v1",
        &[&agent_id.to_string(), kind.as_str(), &person_id.to_string(), claim_key],
    )
}

/// Slot identity for a fact that is globally unique inside its kind (a
/// platform account, a voiceprint rank inside its recording).
pub fn global_slot_id(agent_id: EntityId, kind: PersonClaimKind, claim_key: &str) -> EntityId {
    deterministic_entity_id(
        "shenko.contacts.slot.global.v1",
        &[&agent_id.to_string(), kind.as_str(), claim_key],
    )
}

/// The stable Shenko person id of a Life catalog person.
///
/// Reusing the catalog's own id keeps one person one id across imports; it
/// is derived, never randomly assigned, so a second import does not create a
/// second unrelated number for somebody already known.
pub fn life_person_id(life_person_id: &str) -> EntityId {
    deterministic_entity_id("shenko.contacts.life.person.v1", &[life_person_id])
}
