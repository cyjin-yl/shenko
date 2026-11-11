//! Android perception/execution plugins — docs/specs/mobile-runtime.md#spec-24-5, docs/specs/mobile-runtime.md#spec-24-5-1, C15 (mobile half).
//!
//! Mobile OS perception and execution are plugins, not core business: this
//! crate implements the on-device bridge client and the native task handlers
//! a Shenko node running inside the Android APK uses to observe and act
//! through *actually granted* capabilities. The APK is only a distribution
//! bundle: **bundled ≠ available ≠ OS-authorized**. Every authority is a
//! replaceable adapter; root is an optional user-preferred enhancement, never
//! a prerequisite for joining or being intelligent.
//!
//! Real mechanisms only (verified against upstream sources pinned in
//! `docs/specs/mobile-runtime.md`):
//! - root: direct argv execution through `su` (no shell string interpolation);
//! - Shizuku: binder proxy started by ADB or root — ADB and root identities
//!   differ and are reported separately, never assumed equivalent;
//! - Dhizuku: shared DeviceOwner API — device owner is not root and not ADB,
//!   and grants no arbitrary app-private-data read;
//! - Accessibility: node tree observation + gesture dispatch;
//! - normal permissions: this app's own uid surface.
//! These authorities are parallel and combinable — there is deliberately no
//! "normal → Shizuku → Dhizuku → root" escalation ladder in this crate.
//!
//! Honest limits, by design:
//! - The bridge handshake authenticates the node process to the app-owned
//!   bridge service (same uid, per-incarnation token). It grants nothing and
//!   never elevates authority; each operation still goes through the real
//!   OS mechanism with its own grant state.
//! - Off-device (any non-Android host) every execution returns a typed
//!   unavailable fact naming the exact missing mechanism. A desktop node
//!   cannot pretend to hold this phone's OS grants (docs/specs/nodes.md#spec-5-4-4: a copied
//!   conclusion is not the receiver's capability).
//! - App-database observation keeps `file located / readable / recovered
//!   copy / point-in-time consistent / plaintext / structure / semantics` as
//!   separate facts (docs/specs/mobile-runtime.md#spec-24-5-1). A raw db+wal copy is never reported as a
//!   consistent snapshot, and decrypted or semantically-complete content is
//!   never claimed without the actual plaintext evidence.

mod backend;
mod bridge;
mod facts;
mod fingerprint;
mod handlers;

pub use backend::AndroidBackend;
pub use bridge::{
    BridgeAuthority, BridgeError, BridgeErrorKind, BridgeOp, DeviceBridge, SocketBridge,
    UnavailableBridge, BRIDGE_PROTOCOL_VERSION,
};
pub use facts::{
    CapabilityState, DbSnapshot, KeyMaterialState, MobileAuthorityKind, SnapshotDimension,
};
pub use fingerprint::{
    FingerprintAbsence, FingerprintEvidence, FingerprintRequest, FingerprintScope,
    SemanticFingerprint, FINGERPRINT_ALGORITHM, KIND_NODE_TREE, KIND_NODE_TREE_PREFIX,
    KIND_PIXELS_ONLY, MAX_FINGERPRINT_DEPTH, MAX_FINGERPRINT_NODES, MAX_FINGERPRINT_WINDOWS,
};
pub use handlers::{
    AndroidAppDbSnapshotHandler, AndroidControlInputHandler,
    AndroidControlLeaseHandler, AndroidDeviceProbeHandler, AndroidScreenObserveHandler,
    AndroidShellExecHandler,
    MOBILE_NATIVE_KINDS,
};
