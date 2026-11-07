//! Shared fixtures for the `shenko-evidence` consumer tests.
//!
//! One canonical sealed-report shape so the admission tests and the
//! executable's tests cannot drift apart. These are fixture bytes for the
//! admission contract, not a long-run observation.

use serde_json::{Value, json};

/// The candidate every fixture is bound to.
pub const CANDIDATE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Binary digest a sealed run declares and measures.
pub const BINARY: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// A sealed sampler report in the shape `scripts/observe_longrun.py` writes:
/// schema 5, with executed-file SHA-256 binding, integrity, declared windows
/// and the terminal `run_end` lifecycle.
pub fn sealed_run_report() -> Value {
    json!({
        "schema": 5,
        "record_schema": 3,
        "kind": "c19_long_run_observation_report",
        "run_id": "fixture-run",
        "evidence_class": "real_observation",
        // The sampler records how many samples the report folded in; the seal
        // names one of them.
        "samples": 2,
        "subject_alive_samples": 2,
        "candidate": {
            "commit": CANDIDATE,
            "profile": "release-native",
            "binary_sha256": BINARY,
            "binary_measured": {"sha256": BINARY, "bytes": 4096, "mtime": 7},
        },
        "declared_windows": [
            {"window": "7d", "required_seconds": 604800,
             "qualifying_span_seconds": 604800, "status": "observed"},
            {"window": "30d", "required_seconds": 2592000,
             "qualifying_span_seconds": 2592000, "status": "observed"},
        ],
        "integrity": {
            "intact": true,
            "last_sample_epoch": 1060.0,
            "candidate_binary_binding": "proc_exe_sha256",
            "candidate_binary_verified_sample_seqs": [1, 2],
            "candidate_binary_unverified_sample_seqs": [],
            "candidate_binary_drift_sample_seqs": [],
        },
        "lifecycle": {
            "sealed": true,
            "stop_reason": "completed",
            "stop_epoch": 1061.0,
            "stop_sample_seq": 2,
            "checkpoints": [
                {"checkpoint_id": "review-1", "sample_seq": 1, "epoch": 1000.0}
            ],
        },
        "verdict": "all_declared_windows_observed",
        "eligible_as_product_evidence": true,
    })
}
