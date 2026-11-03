use anyhow::Result;
use serde_json::{Value, json};
use shenko_node::{Identity, Node, PeerExperience, Request, ResultReceipt, Ticket, running_manifest};
use shenko_types::{ids::EntityId, object::Digest};
use std::path::Path;

fn initialize(dir: &Path, identity: &Identity) -> Result<(Node, EntityId)> {
    let mut node = Node::open(dir)?;
    let id = EntityId::new();
    node.handle(Request::Initialize { node_id: id, identity: Some(identity.clone()) })?;
    node.handle(Request::Join {
        operation_id: EntityId::new(), expected_node: id,
        identity: identity.clone(), manifest: running_manifest()?,
    })?;
    node.handle(Request::Register {
        node_id: id, target_id: "receipt-fixture".into(), binding_digest: Digest::blake3(b"local fixture"),
    })?;
    Ok((node, id))
}

fn execute(node: &mut Node, node_id: EntityId, task_id: EntityId, input: &[u8]) -> Result<ResultReceipt> {
    let created = node.handle(Request::Create { task_id, node_id, input: input.to_vec() })?;
    let ticket: Ticket = serde_json::from_value(created["ticket"].clone())?;
    let executed = node.handle(Request::Execute { ticket, input: input.to_vec() })?;
    Ok(serde_json::from_value(executed["receipt"].clone())?)
}

fn exported(node: &mut Node) -> Result<Value> {
    node.handle(Request::ExportReceipts { after: "0".into(), limit: 128 })
}

#[test]
fn peer_receipts_survive_reopen_without_settlement_or_duplication() -> Result<()> {
    let root = tempfile::tempdir()?;
    let local_dir = root.path().join("local");
    let identity = Identity {
        agent_id: EntityId::new(), branch_id: EntityId::new(), origin_proof: Digest::blake3(b"fixture origin"),
    };
    let (mut local, local_id) = initialize(&local_dir, &identity)?;
    let (mut peer, peer_id) = initialize(&root.path().join("peer"), &identity)?;
    let task_id = EntityId::new();
    let local_receipt = execute(&mut local, local_id, task_id, b"local outcome")?;
    let accepted = local.handle(Request::Accept { from_node: local_id, receipt: local_receipt.clone() })?;
    assert_eq!(accepted["accepted"], true);
    let conflict = execute(&mut peer, peer_id, task_id, b"parallel peer outcome")?;
    let unassociated = execute(&mut peer, peer_id, EntityId::new(), b"peer-only outcome")?;
    let experience = PeerExperience {
        peer_node_id: peer_id, peer_branch_id: Some(identity.branch_id), events: vec![],
        receipts: vec![conflict.clone(), unassociated.clone()], cognition_fingerprint: None,
        cognition_high_water: None, note: "native fixture".into(),
    };
    drop(peer);
    let record = local.reconcile_peer_experience(&experience)?;
    assert_eq!(record.effects[0].classification, "parallel_terminal_fact");
    assert_eq!(record.effects[1].classification, "evidence_only");
    drop(local);

    let mut local = Node::open(&local_dir)?;
    let first = exported(&mut local)?;
    let receipts = first["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 3, "retain both remote receipts and the original local settlement");
    for receipt in [&conflict, &unassociated] {
        let original = serde_json::to_value(receipt)?;
        let retained = receipts.iter().find(|row| row["receipt"] == original).expect("complete peer receipt survives");
        assert_eq!(retained["status"], "evidence_only");
        assert_eq!(retained["reason"], "peer_reconciliation");
    }
    let own = receipts.iter().find(|row| row["receipt"] == json!(local_receipt)).unwrap();
    assert_eq!(own["status"], "settled");
    // Receiving evidence is not a second execution or a new settlement.
    let connection = rusqlite::Connection::open_with_flags(local_dir.join("node.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    for table in ["tasks", "work", "results"] {
        assert_eq!(connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_, i64>(0))?, 1);
    }
    drop(connection);
    local.reconcile_peer_experience(&experience)?;
    assert_eq!(exported(&mut local)?["receipts"], first["receipts"], "replay cannot duplicate or downgrade retained evidence");
    Ok(())
}
