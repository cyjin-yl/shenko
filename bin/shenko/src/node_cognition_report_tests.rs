// The real Store-origin cognition report: producer, peer route, merge, reopen.
//
// SPEC §51.1: the node keeps one mechanism observation of *this installation's
// own* Store snapshot — the immutable content digest over a declared scope plus
// the ledger watermark it covered. It is not content, not an attitude, not a
// knowledge kind and not a confidence; it is not a liveness, capacity or
// recoverability claim either. This consumer runs the whole producer chain
// against a real Store and a real node installation, reads the report back the
// way a peer's route does, merges that peer experience, and reopens the
// installation to prove the report is durable.
//
// What this fixture does not claim: it runs no SSH transfer (the route body is
// the exact `Status` shape the peer reads, imported here directly), it does not
// measure peer liveness, and it does not measure model behaviour.
mod node_cognition_report {
    use super::*;
    use serde_json::json;
    use shenko_node::{CognitionComparison, CognitionSnapshotReport, PeerExperience};
    use shenko_types::ids::EntityId;

    fn cognition_report_fixture(name: &str) -> (std::path::PathBuf, shenko_store::store::Store) {
        let dir = std::env::temp_dir().join(format!("shenko-{name}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = open_native_fixture(&dir);
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        bootstrap_genesis(
            &store,
            None,
            &home.manifest,
            &shenko_seed::ModelProfile::from_env(),
        )
        .unwrap();
        drop(home);
        (dir, store)
    }

    #[test]
    fn store_origin_cognition_report_reaches_a_peer_route_and_survives_reopen() {
        let (dir, store) = cognition_report_fixture("cognition-report");
        let mut node = node_sync::open_node(&dir, &store).unwrap();

        // No installation observed this Store's snapshot yet.
        assert!(node.cognition_report().unwrap().is_none());

        let report = node_sync::publish_cognition_report(&store, &mut node, &dir).unwrap();

        // The report is the Store's own snapshot over the installation's own
        // recovery scope: recomputing it independently yields the same digest and
        // ledger watermark, and the declared coverage is the one that was read.
        let options = node_sync::cognition_report_options(&dir);
        let (fingerprint, high_water) = store.cognition_fingerprint(&options).unwrap();
        assert_eq!(report.fingerprint, fingerprint);
        assert_eq!(report.high_water, high_water.0);
        assert_eq!(report.node_id, node.local_node_id().unwrap().unwrap());
        assert_eq!(report.generation, 1);
        assert_eq!(report.scope.memory_root, options.memory_root);
        assert_eq!(report.scope.workspace_roots, options.workspace_roots);
        assert_eq!(report.scope.include_git_history, options.include_git_history);
        assert_eq!(
            report.scope.local_boundary_preserved,
            options.local_boundary_preserved
        );
        assert!(!report.scope.local_boundary_preserved);

        // A second tick that observes the same Store content is the same revision,
        // not a new one: unchanged bytes never open a new version (§51.1).
        let again = node_sync::publish_cognition_report(&store, &mut node, &dir).unwrap();
        assert_eq!(again, report);

        // The peer route serves exactly this report…
        let route = json!({"node_id": report.node_id, "cognition": report});
        assert_eq!(
            node_sync::peer_cognition_report(report.node_id, &route).unwrap(),
            Some(report.clone())
        );
        // …bound to the peer it travelled from: a report naming another node is
        // refused instead of being recorded as this peer's observation.
        assert!(node_sync::peer_cognition_report(EntityId::new(), &route).is_err());
        // A peer whose installation published nothing stays unknown.
        assert_eq!(
            node_sync::peer_cognition_report(
                report.node_id,
                &json!({"node_id": report.node_id, "cognition": null})
            )
            .unwrap(),
            None
        );
        // A route body without the key at all is equally unknown, never an error
        // that a caller could mistake for a default.
        assert_eq!(
            node_sync::peer_cognition_report(report.node_id, &json!({"node_id": report.node_id}))
                .unwrap(),
            None
        );

        // A second, independent installation produces its own Store observation.
        let (peer_dir, peer_store) = cognition_report_fixture("cognition-report-peer");
        let mut peer_node = node_sync::open_node(&peer_dir, &peer_store).unwrap();
        let peer_report =
            node_sync::publish_cognition_report(&peer_store, &mut peer_node, &peer_dir).unwrap();

        // The merge carries the peer's report — fingerprint, watermark, revision and
        // coverage — and states how the two snapshots relate. Two independent
        // installations either hold the same replicated snapshot or they do not;
        // the answer is derived from the bytes, never assumed.
        let record = node
            .reconcile_peer_experience(&PeerExperience {
                peer_node_id: peer_report.node_id,
                peer_branch_id: None,
                events: vec![],
                receipts: vec![],
                cognition_fingerprint: Some(peer_report.fingerprint.clone()),
                cognition_high_water: Some(peer_report.high_water),
                cognition_origin: Some(peer_report.origin()),
                peer_high_water: None,
                peer_manifest: None,
                note: "fixture: real Store-origin report pulled over the peer route".into(),
            })
            .unwrap();
        assert_eq!(
            record.cognition_fingerprint,
            Some(peer_report.fingerprint.clone())
        );
        assert_eq!(record.cognition_high_water, Some(peer_report.high_water));
        assert_eq!(record.cognition_origin, Some(peer_report.origin()));
        let expected_comparison = if peer_report.fingerprint == report.fingerprint {
            CognitionComparison::SameSnapshot
        } else {
            CognitionComparison::DifferentSnapshot
        };
        assert_eq!(record.cognition_comparison, Some(expected_comparison));
        // The merge reports the peer; it never adopts the peer's snapshot as this
        // installation's own observation.
        assert_eq!(node.cognition_report().unwrap(), Some(report.clone()));

        // Reopen: the report and the merge record are durable facts of this node.
        drop(node);
        drop(store);
        let reopened_store = open_native_fixture(&dir);
        let reopened = node_sync::open_node(&dir, &reopened_store).unwrap();
        assert_eq!(reopened.cognition_report().unwrap(), Some(report.clone()));
        let records = reopened.reconciliation_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].reconciliation_id, record.reconciliation_id);
        assert_eq!(
            records[0].cognition_fingerprint,
            record.cognition_fingerprint
        );
        assert_eq!(
            records[0].cognition_comparison,
            record.cognition_comparison
        );

        // An installation that never published a report compares as unknown even
        // when the peer published a real one — absence is not a default.
        let (silent_dir, silent_store) = cognition_report_fixture("cognition-report-silent");
        let mut silent = node_sync::open_node(&silent_dir, &silent_store).unwrap();
        let record = silent
            .reconcile_peer_experience(&PeerExperience {
                peer_node_id: peer_report.node_id,
                peer_branch_id: None,
                events: vec![],
                receipts: vec![],
                cognition_fingerprint: Some(peer_report.fingerprint.clone()),
                cognition_high_water: Some(peer_report.high_water),
                cognition_origin: Some(peer_report.origin()),
                peer_high_water: None,
                peer_manifest: None,
                note: "fixture: peer report against an installation with no own report".into(),
            })
            .unwrap();
        assert_eq!(
            record.cognition_comparison,
            Some(CognitionComparison::Unobserved)
        );
        assert!(silent.cognition_report().unwrap().is_none());

        drop(peer_node);
        drop(silent);
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&peer_dir).unwrap();
        std::fs::remove_dir_all(&silent_dir).unwrap();
    }
}
