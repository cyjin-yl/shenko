//! Unit uninstall through the real node JSON wire — docs/specs/cognition-units.md#spec-45-10 lifecycle.
//!
//! The caller is a JSON producer, not this crate's own API: the request is
//! deserialized from `Request { command: "units", request: { command:
//! "uninstall" } }`, executed by `Node::handle`, and the removal must actually
//! retire the materialized package while leaving the artifact bytes and the
//! removal fact readable.
use serde_json::{Value, json};
use shenko_node::{Node, Request, UnitRequest, b64_encode};
use shenko_types::object::Digest;

fn package(unit_id: &str, revision: &str, bytes: &[u8]) -> Value {
    let digest = Digest::blake3(bytes);
    json!({
        "schema": shenko_node::UNIT_SCHEMA,
        "root_unit_id": unit_id,
        "root_revision": revision,
        "root_artifact_digest": digest,
        "origin_node": null,
        "enabled": false,
        "packages": [{
            "unit": {
                "unit_id": unit_id,
                "kind": "skill",
                "revision": revision,
                "artifact_digest": digest,
                "provides": [],
                "dependencies": [],
                "config": {"mode": "wire"},
                "applicability": null,
            },
            "artifact_base64": b64_encode(bytes),
            "evidence": [],
            "resolved_dependencies": null,
        }],
    })
}

#[test]
fn unit_uninstall_crosses_the_node_json_wire() {
    let temporary = tempfile::tempdir().unwrap();
    let node_dir = temporary.path().join("node");
    let package_root = temporary.path().join("packages");
    std::fs::create_dir(&package_root).unwrap();
    let mut node = Node::open(&node_dir).unwrap();

    let bytes = b"wire-artifact";
    let digest = Digest::blake3(bytes);
    let import: Request = serde_json::from_value(json!({
        "command": "units",
        "request": {
            "command": "import",
            "export": package("wire.skill", "1.0.0", bytes),
            "package_root": package_root.to_string_lossy(),
            "enable": true,
        },
    }))
    .expect("the import request is a valid node wire request");
    let imported = node.handle(import).unwrap();
    assert_eq!(imported["schema"], json!(shenko_node::UNIT_SCHEMA));

    // The wire shape this slice adds, including the omitted optional fields.
    let request = UnitRequest::Uninstall {
        unit_id: "wire.skill".into(),
        revision: "1.0.0".into(),
        artifact_digest: None,
        reason: "wire removal".into(),
        activate: None,
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["command"], json!("uninstall"));
    let decoded: UnitRequest = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);

    let removed = node
        .handle(serde_json::from_value(json!({
            "command": "units",
            "request": encoded,
        }))
        .unwrap())
        .unwrap();
    assert_eq!(removed["removed"], json!(true));
    assert_eq!(removed["was_current"], json!(true));
    let retired = removed["files_retired"].as_array().unwrap();
    assert_eq!(retired.len(), 1);
    assert!(
        !std::path::Path::new(retired[0].as_str().unwrap()).exists(),
        "the materialized package directory is retired"
    );

    // Facts a receipt can still name: the bytes, and the removal record.
    assert_eq!(node.unit_object_bytes(&digest).unwrap(), bytes.to_vec());
    let lineage = node
        .handle(
            serde_json::from_value(json!({
                "command": "units",
                "request": {"command": "lineage", "unit_id": "wire.skill"},
            }))
            .unwrap(),
        )
        .unwrap();
    assert_eq!(lineage["versions"], json!([]));
    assert_eq!(lineage["removed"][0]["reason"], json!("wire removal"));

    // And the withdrawn version is no longer offered through the same wire.
    let status = node.handle(
        serde_json::from_value(json!({
            "command": "units",
            "request": {"command": "status", "unit_id": "wire.skill"},
        }))
        .unwrap(),
    );
    assert!(status.is_err());
}
