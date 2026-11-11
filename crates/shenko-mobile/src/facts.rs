//! Capability/authority facts and the docs/specs/mobile-runtime.md#spec-24-5-1 database snapshot-dimension
//! model.
//!
//! These types are the durable vocabulary every mobile observation uses.
//! They deliberately keep dimensions apart that a careless adapter would
//! merge: OS facts vs app facts vs database facts vs process facts vs key
//! facts vs format facts; bridge reachability vs actual OS grant; a raw
//! recovered copy vs a point-in-time consistent snapshot.
//!
//! docs/specs/cognition-units.md#spec-51-9 reading: [`SnapshotDimension`] and [`DbSnapshot`] record whether the
//! *dimensions of one snapshot* were reached — they are not cognitive facts.
//! They carry no content revision, no attitude, no reason pointer and no
//! confidence, and nothing here may be presented as understanding the
//! database. Only a later subject that asserts or learns something through the
//! canonical content/attitude API turns an observation into a reason root.
//!
//! `CapabilityState`/`KeyMaterialState` are mechanism facts too: what an
//! adapter or a piece of key material actually is right now. `Unknown` stays
//! `Unknown` — no default value is filled in for it.

use serde::{Deserialize, Serialize};

/// The real mechanism an observation or action went through.
///
/// The variants are parallel and combinable, never a privilege ladder:
/// `NormalPerm` does not "upgrade into" `Accessibility`, `ShizukuBinder`,
/// `DhizukuOwner` or `RootSu`, and gaining root does not make every encrypted
/// library, hardware-bound key or app format parseable (docs/specs/mobile-runtime.md#spec-24-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileAuthorityKind {
    /// On-device `su` argv execution — user-preferred optional enhancement.
    RootSu,
    /// Shizuku binder proxy. The service may have been started by ADB or by
    /// root; those identities have different uids and different real
    /// capability sets, and the probe records which one this is.
    ShizukuBinder,
    /// Dhizuku shared DeviceOwner API. Device owner is neither root nor ADB
    /// and grants no arbitrary app-private-data plaintext read.
    DhizukuOwner,
    /// Android AccessibilityService: node-tree observation and gestures.
    Accessibility,
    /// This app's own uid with normally granted runtime permissions.
    NormalPerm,
}

impl MobileAuthorityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RootSu => "root_su",
            Self::ShizukuBinder => "shizuku_binder",
            Self::DhizukuOwner => "dhizuku_owner",
            Self::Accessibility => "accessibility",
            Self::NormalPerm => "normal_perm",
        }
    }

    /// Elevated mechanisms whose presence must still be re-verified per use.
    pub fn is_privileged(self) -> bool {
        matches!(
            self,
            Self::RootSu | Self::ShizukuBinder | Self::DhizukuOwner
        )
    }
}

/// Live state of one authority adapter, probed on-device and cached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapabilityState {
    /// The mechanism answered and the specific capability is granted now.
    Available {
        detail: String,
        verified_at_ms: i64,
    },
    /// The mechanism is present but refuses or is not authorized right now.
    Unavailable { reason: String, revalidate: String },
    /// Presence undetermined (bridge down, not yet probed, grant never asked).
    Unknown { reason: String },
    /// Probe failed with an error; `retryable` says whether revalidation
    /// could change the answer.
    Failed { error: String, retryable: bool },
}

impl CapabilityState {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }
}

/// Whether decryption key material exists for an app database.
///
/// Key availability is a fact about *material*, separate from whether the
/// ciphertext was read or the format understood. This plugin never extracts
/// keys; app adapters that legitimately hold keys report them separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KeyMaterialState {
    /// No key material is available to this plugin.
    NoneAvailable { note: String },
    /// Key material was supplied by an app-specific adapter; the digest and
    /// provenance of that adapter's revision are recorded by the caller.
    SuppliedByAdapter { adapter: String, revision: String },
    /// Whether keys exist is itself unknown (e.g. bridge unreachable).
    Unknown,
}

/// Whether one dimension of a database snapshot was reached, and what proved
/// it (docs/specs/cognition-units.md#spec-51-9: `SnapshotDimension`, *not* a cognitive fact).
///
/// A reached dimension is evidence about our own copy, never about the whole
/// database: it says nothing about plaintext, schema or meaning on its own.
/// Nothing here is content, an attitude or a reason pointer, and no confidence
/// is attached — a dimension either was established with evidence or it was
/// not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDimension {
    pub reached: bool,
    /// What proved the dimension (e.g. "sqlite_header",
    /// "readonly_open_sqlite_master").
    pub evidence: Option<String>,
    /// Honest qualification (e.g. "concurrent writes may be missing").
    pub note: Option<String>,
}

impl SnapshotDimension {
    pub fn reached(evidence: impl Into<String>) -> Self {
        Self {
            reached: true,
            evidence: Some(evidence.into()),
            note: None,
        }
    }

    pub fn missed(note: impl Into<String>) -> Self {
        Self {
            reached: false,
            evidence: None,
            note: Some(note.into()),
        }
    }
}

/// docs/specs/mobile-runtime.md#spec-24-5-1 dimension separation for one app-database snapshot (docs/specs/cognition-units.md#spec-51-9:
/// `DbSnapshot`, a snapshot result — not a cognitive fact).
///
/// No total order is implied: `plaintext_verified` and
/// `structure_interpretable` are independent dimensions, `snapshot_*` speaks
/// only about copy consistency, and `semantic_complete` is never set by this
/// plugin — understanding content is a subject judgment over actually
/// recovered plaintext, not a property of a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbSnapshot {
    /// The database path for the package was located.
    pub file_located: SnapshotDimension,
    /// The bytes were readable through some granted authority.
    pub file_readable: SnapshotDimension,
    /// The complete byte set (main file plus WAL/SHM/journal companions) was
    /// copied and the copy recovered when opened. This is NOT a point-in-time
    /// consistent snapshot; concurrent writes may be missing or torn.
    pub snapshot_recovered_copy: SnapshotDimension,
    /// A point-in-time consistent snapshot: SQLite Online Backup API over a
    /// directly readable source, or a verified quiesced/filesystem snapshot.
    pub snapshot_consistent: SnapshotDimension,
    /// Content verified plaintext for the claimed format/version (e.g. the
    /// `SQLite format 3\0` header). Absent means encrypted or a foreign
    /// format — which one stays unknown without key material.
    pub plaintext_verified: SnapshotDimension,
    /// The schema was parsed on the snapshot (e.g. `sqlite_master` readable).
    pub structure_interpretable: SnapshotDimension,
    /// Never reached by this plugin; kept explicit so no caller can read a
    /// default `true`.
    pub semantic_complete: SnapshotDimension,
    /// Key material state, an independent dimension.
    pub key_material: KeyMaterialState,
}

impl DbSnapshot {
    /// Initial state for a snapshot attempt that located nothing yet.
    pub fn none() -> Self {
        Self {
            file_located: SnapshotDimension::missed("database path not located"),
            file_readable: SnapshotDimension::missed("not attempted"),
            snapshot_recovered_copy: SnapshotDimension::missed("not attempted"),
            snapshot_consistent: SnapshotDimension::missed("not attempted"),
            plaintext_verified: SnapshotDimension::missed("not attempted"),
            structure_interpretable: SnapshotDimension::missed("not attempted"),
            semantic_complete: SnapshotDimension::missed(
                "semantic completeness is never claimed by the snapshot plugin",
            ),
            key_material: KeyMaterialState::NoneAvailable {
                note: "this plugin does not extract or hold decryption keys"
                    .to_string(),
            },
        }
    }
}
