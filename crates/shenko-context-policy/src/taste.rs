//! Installable revision-disclosure projection — docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-9.
//!
//! Revisions are retained, never rewritten. What a surface shows *by default*
//! about a revised unit or attitude is a decision of the installed `taste`
//! macro package, not of the consumer: the confirmed default is that the
//! selected revision is the material, older revisions collapse to their count
//! and the latest modification time, and full text is shown only when a
//! revision is named explicitly.
//!
//! This module is the mechanism plus the distribution's preinstalled default,
//! not a second registry and not a second content store:
//!
//! * [`RevisionChain`] is mechanism metadata. When the caller read the
//!   canonical store's disclosure counters
//!   (`shenko_store::atoms::revision_disclosure` → `ContentRevisionDisclosure`)
//!   they are carried verbatim in [`CanonicalDisclosure`] and are the
//!   authority: a taste never recomputes them. A journal family that keeps its
//!   own retained ordering log (the working notebook) supplies the same
//!   fields from that log, which is its mechanism authority.
//! * [`RevisionTaste`] is the replaceable decision. One implementation is
//!   published as one Cordis service ([`TasteHost`] / [`InstalledTaste`]) and
//!   withdrawn with its Fiber, so installing another taste replaces the
//!   projection rather than adding a parallel one.
//! * [`disclose_revisions`] is the one call consumers make. With no taste
//!   installed it reports `None`: the caller records the material as
//!   omitted/unresolved (docs/specs/cognition-units.md#spec-45-10) instead of inventing a count or falling back
//!   to a locally hardcoded policy.
//!
//! [`DefaultRevisionTaste`] is the distribution's preinstalled implementation
//! of that confirmed default and the mechanism-level fallback when no taste
//! macro member is selected. It names itself honestly: it is a reference
//! implementation shipped with the distribution, not an installed content
//! unit, so it carries no content digest. A selected macro member becomes an
//! installed taste by evaluating its `cap.taste.revision_projection/1` rule
//! body; until that evaluator exists the runtime reports the member as
//! unresolved rather than attributing the mechanism default's decision to it.
use std::borrow::Cow;
use std::sync::Arc;

use cordis_core::{Context, Plugin, Service};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::ids::EntityId;

/// Contract version of the disclosure projection. A consumer that does not
/// understand a newer version reports the projection as unresolved instead of
/// guessing at the fields.
pub const TASTE_CONTRACT_VERSION: u32 = 1;

/// Capability contract a `taste` macro member provides when it carries this
/// revision-projection role.
pub const REVISION_PROJECTION_CONTRACT: &str = "cap.taste.revision_projection/1";

/// One retained revision, as mechanism identity. The bytes are addressed by
/// digest in the content store; nothing here copies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionMeta {
    /// Monotonic journal revision. Retained originals are never renumbered.
    pub revision: u64,
    /// Content digest of this revision's bytes, once they are registered in
    /// the content store. A retained row whose bytes were never registered is
    /// still a revision; it simply has no digest to read by yet.
    pub content_digest: Option<String>,
    /// When this revision was written.
    pub written_at_ms: i64,
}

/// The canonical content store's disclosure counters, verbatim.
///
/// Field-for-field the store's `ContentRevisionDisclosure`
/// (`latest_digest`, `old_version_count`, `latest_modified_at_ms`). Carrying it
/// as an input is what keeps a taste from re-deriving counts: the store owns
/// the revision chain, the taste only decides what to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalDisclosure {
    pub latest_digest: String,
    pub old_version_count: u64,
    pub latest_modified_at_ms: i64,
}

impl CanonicalDisclosure {
    pub fn new(
        latest_digest: impl Into<String>,
        old_version_count: u64,
        latest_modified_at_ms: i64,
    ) -> Self {
        Self {
            latest_digest: latest_digest.into(),
            old_version_count,
            latest_modified_at_ms,
        }
    }
}

/// The retained revisions of one content family under one scope.
///
/// Families are the unit/attitude labels a surface already knows (for example
/// `context.notebook`); the chain carries only what the projection may look
/// at, so a renderer cannot recompute a policy from other state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionChain {
    pub family: String,
    pub scope_id: EntityId,
    /// Canonical store counters, when the caller read them.
    pub canonical: Option<CanonicalDisclosure>,
    /// Mechanism revision numbers of a journal family. Sorted ascending; a
    /// duplicate revision is refused.
    pub revisions: Vec<RevisionMeta>,
}

impl RevisionChain {
    /// A journal-backed chain: the retained ordering log is the mechanism
    /// authority for which revisions exist.
    ///
    /// Revision numbers order themselves, so input order is not a fact. A
    /// repeated revision is refused: two different contents cannot both be
    /// revision *n* of one family, and silently picking one would hide a
    /// corrupt journal.
    pub fn new(
        family: impl Into<String>,
        scope_id: EntityId,
        mut revisions: Vec<RevisionMeta>,
    ) -> Result<Self, TasteError> {
        revisions.sort_by_key(|meta| meta.revision);
        for pair in revisions.windows(2) {
            if pair[0].revision == pair[1].revision {
                return Err(TasteError::DuplicateRevision { revision: pair[0].revision });
            }
        }
        Ok(Self { family: family.into(), scope_id, canonical: None, revisions })
    }

    /// Attach the canonical store's counters. They are then the authority for
    /// the default view; no count is recomputed from `revisions`.
    pub fn with_canonical(mut self, canonical: CanonicalDisclosure) -> Self {
        self.canonical = Some(canonical);
        self
    }

    /// The newest retained revision: the one a default projection selects.
    pub fn selected(&self) -> Option<&RevisionMeta> {
        self.revisions.last()
    }

    pub fn find(&self, revision: u64) -> Option<&RevisionMeta> {
        self.revisions.iter().find(|meta| meta.revision == revision)
    }

    pub fn retained(&self) -> Vec<u64> {
        self.revisions.iter().map(|meta| meta.revision).collect()
    }
}

/// What the caller wants to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureRequest {
    /// The default view of this family.
    Selected,
    /// Full text of one named revision (the explicit-version read).
    Revision(u64),
}

/// The projection decision. It names the revision and its digest; resolving
/// bytes is mechanism, so a `Selected` decision never carries text and a
/// `Version` decision carries text only once the caller resolved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disclosure", rename_all = "snake_case")]
pub enum Disclosure {
    /// Default view: the selected revision plus the collapsed older revisions.
    Selected {
        family: String,
        /// Mechanism revision number, when this family keeps one.
        revision: Option<u64>,
        /// Content digest of the selected revision, when registered.
        content_digest: Option<String>,
        /// Mechanism write time of the selected revision, when known.
        written_at_ms: Option<i64>,
        /// Retained revisions strictly older than the selected one.
        older_versions: u64,
        /// Newest retained modification time.
        latest_modified_at_ms: i64,
    },
    /// Explicit-version view: this revision's full text, when resolved.
    Version {
        family: String,
        revision: u64,
        content_digest: String,
        written_at_ms: i64,
        text: Option<String>,
    },
}

impl Disclosure {
    /// The revision this decision discloses, when the family keeps numbers.
    pub fn revision(&self) -> Option<u64> {
        match self {
            Self::Selected { revision, .. } => *revision,
            Self::Version { revision, .. } => Some(*revision),
        }
    }

    pub fn content_digest(&self) -> Option<&str> {
        match self {
            Self::Selected { content_digest, .. } => content_digest.as_deref(),
            Self::Version { content_digest, .. } => Some(content_digest.as_str()),
        }
    }

    /// Attach the resolved bytes of an explicit-version read.
    ///
    /// A `Selected` decision is unchanged: the default view never shows full
    /// text, whatever bytes the caller happens to hold.
    pub fn attach_text(self, text: String) -> Self {
        match self {
            Self::Version { family, revision, content_digest, written_at_ms, .. } => Self::Version {
                family,
                revision,
                content_digest,
                written_at_ms,
                text: Some(text),
            },
            other => other,
        }
    }

    /// The one server shape every surface renders.
    ///
    /// Field names are stable; a renderer reports these fields instead of
    /// deriving its own counts. `full_text` is `null` unless an explicit
    /// revision was requested and its bytes were resolved.
    pub fn project_json(&self) -> Value {
        match self {
            Self::Selected {
                family,
                revision,
                content_digest,
                written_at_ms,
                older_versions,
                latest_modified_at_ms,
            } => json!({
                "family": family,
                "selected": revision,
                "content_digest": content_digest,
                "written_at_ms": written_at_ms,
                "older_versions": older_versions,
                "latest_modified_at_ms": latest_modified_at_ms,
                "full_text": Value::Null,
            }),
            Self::Version {
                family,
                revision,
                content_digest,
                written_at_ms,
                text,
            } => json!({
                "family": family,
                "selected": revision,
                "content_digest": content_digest,
                "written_at_ms": written_at_ms,
                "older_versions": Value::Null,
                "latest_modified_at_ms": Value::Null,
                "full_text": text.as_ref().map(|text| json!({
                    "revision": revision,
                    "written_at_ms": written_at_ms,
                    "text": text,
                })),
            }),
        }
    }
}

/// Why a projection was refused. Every variant names the real fact that made
/// the request impossible; none of them fabricates a revision.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum TasteError {
    #[error("revision {revision} appears twice in one retained chain")]
    DuplicateRevision { revision: u64 },
    #[error("revision {requested} is not retained; retained revisions: {retained:?}")]
    UnknownRevision { requested: u64, retained: Vec<u64> },
    #[error("{family} has no retained revision to disclose")]
    EmptyChain { family: String },
    #[error("revision {revision} is retained but its bytes were never registered as content")]
    UnregisteredRevision { revision: u64 },
}

/// The identity of the installed taste that made a decision. Evidence names
/// it, so a projection can be attributed to the exact rule that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasteUnitRef {
    /// Unit identity of the taste member, or the distribution reference name.
    pub id: String,
    /// Revision of that unit.
    pub revision: u64,
    /// Content digest of the rule, when the rule came from installed content.
    pub content_digest: Option<String>,
}

/// How revisions are disclosed. One implementation is installed per node; the
/// trait carries no storage, so replacing the taste cannot fork history.
pub trait RevisionTaste: Send + Sync + 'static {
    /// The installed taste this decision belongs to.
    fn unit(&self) -> TasteUnitRef;

    /// Contract version this implementation speaks.
    fn contract(&self) -> u32 {
        TASTE_CONTRACT_VERSION
    }

    fn disclose(
        &self,
        chain: &RevisionChain,
        request: DisclosureRequest,
    ) -> Result<Disclosure, TasteError>;
}

/// The distribution's preinstalled projection: docs/specs/cognition-units.md#spec-51-1's confirmed default
/// — retained originals, the selected revision as material, older revisions
/// collapsed to their count and the latest modification time, full text only
/// by explicit revision.
///
/// It uses the canonical store counters when the caller supplied them and
/// otherwise reports the journal's own mechanism facts; it never invents a
/// revision.
pub struct DefaultRevisionTaste;

impl DefaultRevisionTaste {
    /// Reference identity of this preinstalled rule. It is not an installed
    /// content unit, so it carries no content digest.
    pub const UNIT: &'static str = "distribution/default-revision-taste";
}

impl RevisionTaste for DefaultRevisionTaste {
    fn unit(&self) -> TasteUnitRef {
        TasteUnitRef {
            id: Self::UNIT.into(),
            revision: u64::from(TASTE_CONTRACT_VERSION),
            content_digest: None,
        }
    }

    fn disclose(
        &self,
        chain: &RevisionChain,
        request: DisclosureRequest,
    ) -> Result<Disclosure, TasteError> {
        match request {
            DisclosureRequest::Selected => {
                if let Some(canonical) = &chain.canonical {
                    // The store owns the chain: carry its counters verbatim and
                    // name a mechanism revision only when this chain already
                    // holds that exact digest.
                    let selected = chain.selected().filter(|meta| {
                        meta.content_digest.as_deref() == Some(canonical.latest_digest.as_str())
                    });
                    return Ok(Disclosure::Selected {
                        family: chain.family.clone(),
                        revision: selected.map(|meta| meta.revision),
                        content_digest: Some(canonical.latest_digest.clone()),
                        written_at_ms: selected.map(|meta| meta.written_at_ms),
                        older_versions: canonical.old_version_count,
                        latest_modified_at_ms: canonical.latest_modified_at_ms,
                    });
                }
                let Some(selected) = chain.selected() else {
                    return Err(TasteError::EmptyChain { family: chain.family.clone() });
                };
                // A journal family: its retained log is the mechanism truth
                // for ordering, so the count and the newest write time are
                // read from that log, never from a renderer's guess.
                let latest_modified_at_ms = chain
                    .revisions
                    .iter()
                    .map(|meta| meta.written_at_ms)
                    .max()
                    .unwrap_or(selected.written_at_ms);
                Ok(Disclosure::Selected {
                    family: chain.family.clone(),
                    revision: Some(selected.revision),
                    content_digest: selected.content_digest.clone(),
                    written_at_ms: Some(selected.written_at_ms),
                    older_versions: chain.revisions.len().saturating_sub(1) as u64,
                    latest_modified_at_ms,
                })
            }
            DisclosureRequest::Revision(revision) => {
                let Some(meta) = chain.find(revision) else {
                    return Err(TasteError::UnknownRevision {
                        requested: revision,
                        retained: chain.retained(),
                    });
                };
                let Some(content_digest) = meta.content_digest.clone() else {
                    return Err(TasteError::UnregisteredRevision { revision: meta.revision });
                };
                Ok(Disclosure::Version {
                    family: chain.family.clone(),
                    revision: meta.revision,
                    content_digest,
                    written_at_ms: meta.written_at_ms,
                    text: None,
                })
            }
        }
    }
}

/// The one call a consumer makes.
///
/// `taste` is the installed projection service. `None` means no taste is
/// installed on this node: the caller reports the revision material as
/// omitted/unresolved (docs/specs/cognition-units.md#spec-45-10) rather than applying a default of its own.
pub fn disclose_revisions(
    taste: Option<&dyn RevisionTaste>,
    chain: &RevisionChain,
    request: DisclosureRequest,
) -> Result<Option<Disclosure>, TasteError> {
    match taste {
        Some(taste) => taste.disclose(chain, request).map(Some),
        None => Ok(None),
    }
}

/// The published taste service. Publication is generation-owned: disposing the
/// host plugin withdraws it, so replacing a taste is an install, not a
/// mutation of shared state.
pub struct InstalledTaste(Arc<dyn RevisionTaste>);

impl InstalledTaste {
    pub fn new(taste: Arc<dyn RevisionTaste>) -> Self {
        Self(taste)
    }

    pub fn taste(&self) -> &Arc<dyn RevisionTaste> {
        &self.0
    }
}

impl Service for InstalledTaste {
    const NAME: &'static str = "shenko.context.taste";
}

/// Installs one taste for the lifetime of its Fiber.
pub struct TasteHost;

impl Plugin for TasteHost {
    type Config = Arc<dyn RevisionTaste>;
    type Input = Arc<dyn RevisionTaste>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("context-taste")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        // Generation-owned: Cordis withdraws the publication on dispose.
        let _publication = ctx
            .provide(Arc::new(InstalledTaste::new(Arc::clone(input))))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// The installed taste of a context, when one is published.
pub fn installed_taste(ctx: &Context) -> Option<Arc<dyn RevisionTaste>> {
    ctx.try_service::<InstalledTaste>()
        .ok()
        .map(|installed| Arc::clone(installed.taste()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(revisions: &[(u64, &str, i64)]) -> RevisionChain {
        RevisionChain::new(
            "context.notebook",
            EntityId::new(),
            revisions
                .iter()
                .map(|(revision, digest, written_at_ms)| RevisionMeta {
                    revision: *revision,
                    content_digest: Some((*digest).into()),
                    written_at_ms: *written_at_ms,
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn the_default_collapses_older_revisions_and_keeps_the_selected_one() {
        let chain = chain(&[(1, "aa", 10), (2, "bb", 20), (3, "cc", 30)]);
        let projected = DefaultRevisionTaste
            .disclose(&chain, DisclosureRequest::Selected)
            .unwrap();
        assert_eq!(
            projected,
            Disclosure::Selected {
                family: "context.notebook".into(),
                revision: Some(3),
                content_digest: Some("cc".into()),
                written_at_ms: Some(30),
                older_versions: 2,
                latest_modified_at_ms: 30,
            }
        );
        let json = projected.project_json();
        assert_eq!(json["older_versions"], 2);
        assert_eq!(json["latest_modified_at_ms"], 30);
        assert_eq!(json["full_text"], Value::Null, "the default view is not a full-text read");
        // Bytes the caller happens to hold do not turn a default view into a
        // full-text disclosure.
        assert_eq!(
            projected.attach_text("held bytes".into()).project_json()["full_text"],
            Value::Null
        );
    }

    #[test]
    fn the_canonical_counters_are_carried_and_never_recomputed() {
        // The store says two older versions exist although this caller only
        // read one revision number: the store's disclosure is the authority.
        let known = chain(&[(7, "gg", 70)])
            .with_canonical(CanonicalDisclosure::new("gg", 5, 90));
        let projected = DefaultRevisionTaste
            .disclose(&known, DisclosureRequest::Selected)
            .unwrap();
        assert_eq!(projected.revision(), Some(7), "the digest names its mechanism revision");
        assert_eq!(
            projected.project_json()["older_versions"],
            5,
            "a taste must not count locally"
        );
        assert_eq!(projected.project_json()["latest_modified_at_ms"], 90);
        // A canonical digest this chain does not know is still reported
        // verbatim and names no mechanism revision.
        let foreign = chain(&[(7, "gg", 70)]).with_canonical(CanonicalDisclosure::new("zz", 1, 5));
        let projected = DefaultRevisionTaste
            .disclose(&foreign, DisclosureRequest::Selected)
            .unwrap();
        assert_eq!(projected.content_digest(), Some("zz"));
        assert_eq!(projected.revision(), None);
    }

    #[test]
    fn an_explicit_revision_shows_its_full_text_and_nothing_else() {
        let chain = chain(&[(1, "aa", 10), (2, "bb", 25)]);
        let projected = DefaultRevisionTaste
            .disclose(&chain, DisclosureRequest::Revision(1))
            .unwrap();
        assert_eq!(projected.content_digest(), Some("aa"));
        let json = projected.clone().attach_text("older state".into()).project_json();
        assert_eq!(json["selected"], 1);
        assert_eq!(json["full_text"]["revision"], 1);
        assert_eq!(json["full_text"]["text"], "older state");
        assert_eq!(json["older_versions"], Value::Null);
        assert_eq!(json["latest_modified_at_ms"], Value::Null);
    }

    #[test]
    fn a_revision_that_is_not_retained_is_refused_with_the_real_retained_set() {
        let chain = chain(&[(4, "dd", 40), (7, "gg", 70)]);
        assert_eq!(
            DefaultRevisionTaste.disclose(&chain, DisclosureRequest::Revision(5)),
            Err(TasteError::UnknownRevision {
                requested: 5,
                retained: vec![4, 7],
            })
        );
        // A recorded clear is a revision too; it is retained, not deleted.
        assert_eq!(
            DefaultRevisionTaste
                .disclose(&chain, DisclosureRequest::Revision(4))
                .unwrap()
                .revision(),
            Some(4)
        );
    }

    #[test]
    fn a_chain_orders_itself_and_refuses_a_double_revision() {
        let ordered = RevisionChain::new(
            "f",
            EntityId::new(),
            vec![
                RevisionMeta { revision: 2, content_digest: Some("b".into()), written_at_ms: 2 },
                RevisionMeta { revision: 1, content_digest: Some("a".into()), written_at_ms: 1 },
            ],
        )
        .unwrap();
        assert_eq!(ordered.retained(), vec![1, 2]);
        assert_eq!(
            RevisionChain::new(
                "f",
                EntityId::new(),
                vec![
                    RevisionMeta { revision: 2, content_digest: Some("b".into()), written_at_ms: 2 },
                    RevisionMeta { revision: 2, content_digest: Some("c".into()), written_at_ms: 3 },
                ],
            ),
            Err(TasteError::DuplicateRevision { revision: 2 })
        );
    }

    #[test]
    fn a_node_without_an_installed_taste_reports_unresolved_not_a_default() {
        let existing = chain(&[(1, "aa", 10)]);
        assert_eq!(
            disclose_revisions(None, &existing, DisclosureRequest::Selected).unwrap(),
            None,
            "no installed taste is not the distribution default"
        );
        assert_eq!(
            DefaultRevisionTaste.disclose(&chain(&[]), DisclosureRequest::Selected),
            Err(TasteError::EmptyChain { family: "context.notebook".into() })
        );
        assert_eq!(DefaultRevisionTaste.unit().content_digest, None);
        assert!(installed_taste(&Context::new()).is_none());
    }
}
