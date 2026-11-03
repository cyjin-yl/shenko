//! PMA material must survive the same payload reader and usage lifecycle as
//! any other prepared contribution; a TTL does not make it reusable.
use std::sync::Arc;

use shenko_context_policy::contribution::{
    ContributionPayload, ContributionProvider, ContributionRegistry, scope_wide,
};
use shenko_memory::PmaContributionProvider;
use shenko_store::{InterventionDecisionKind, InterventionReminder, Store};
use shenko_types::{ids::EntityId, object::Digest};

fn fixture() -> (Arc<Store>, InterventionReminder) {
    let root = std::env::temp_dir().join(format!("shenko-pma-material-{}", EntityId::new()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("store.db"), root.join("cas")).unwrap());
    let reminder = InterventionReminder {
        reminder_id: EntityId::new(), epoch_id: EntityId::new(), lineage_id: EntityId::new(),
        decision: InterventionDecisionKind::Remind,
        policy: "test-policy".into(), policy_revision: "1".into(),
        content: Some("Retain the original constraint: 中文 / unicode λ".into()),
        provenance: vec!["source:original".into()], relevance: Some("still applies".into()),
        scope: "subject".into(), expires_at_ms: Some(200), transient: true,
        bank_entry_ids: vec![EntityId::new()], reason: None, created_at_ms: 100,
        used_step_id: None, used_at_ms: None,
    };
    (store, reminder)
}

#[test]
fn reminder_payload_is_readable_without_losing_source_evidence() {
    let (store, reminder) = fixture();
    let provider = PmaContributionProvider::new(store.clone());
    let item = provider.contribution_for(&reminder, &scope_wide(EntityId::new()), 100).unwrap();
    let bytes = store.cas.read_object(&item.data.digest).unwrap();
    assert_eq!(ContributionPayload::read(&bytes).unwrap(), reminder.content.as_deref().unwrap());
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["reminder_id"], reminder.reminder_id.to_string());
    assert_eq!(body["provenance"], serde_json::json!(reminder.provenance));
    assert_eq!(body["bank_entry_ids"], serde_json::json!(reminder.bank_entry_ids));
}

#[test]
fn expiring_reminder_is_consumed_once_and_cannot_return_after_recovery() {
    let (store, reminder) = fixture();
    let provider = PmaContributionProvider::new(store);
    let scope = scope_wide(EntityId::new());
    let item = provider.contribution_for(&reminder, &scope, 100).unwrap();
    let registry = ContributionRegistry::new();
    let registration = registry.register_publisher(provider.manifest());
    let ticket = registry.begin_publication(registration, &scope).unwrap();
    registry.publish(ticket, vec![item.clone()]).unwrap();
    assert_eq!(registry.collect(&scope, 199).contributions, vec![item.clone()]);
    let expired = registry.collect(&scope, 200);
    assert!(expired.contributions.is_empty());
    assert_eq!(expired.omitted[0].key, item.key);
    assert!(expired.omitted[0].reason.starts_with("expired:"));
    // Collection alone must not consume material; only an accepted usage does.
    let plan = registry.collect(&scope, 150);
    assert_eq!(plan.contributions, vec![item]);
    let usage = plan.usage(EntityId::new(), 1, "accepted", Digest::blake3(b"sealed input"));
    registry.record_usage(usage.clone()).unwrap();
    assert!(registry.collect(&scope, 151).contributions.is_empty());
    let fresh = provider.contribution_for(&reminder, &scope, 152).unwrap();
    let ticket = registry.begin_publication(registration, &scope).unwrap();
    registry.publish(ticket, vec![fresh.clone()]).unwrap();
    assert!(registry.collect(&scope, 152).contributions.is_empty());

    // A restored usage precedes producer installation, as in subject restart.
    let restored = ContributionRegistry::new();
    let persisted = serde_json::to_vec(&usage).unwrap();
    restored.record_usage(serde_json::from_slice(&persisted).unwrap()).unwrap();
    let registration = restored.register_publisher(provider.manifest());
    let ticket = restored.begin_publication(registration, &scope).unwrap();
    restored.publish(ticket, vec![fresh]).unwrap();
    assert!(restored.collect(&scope, 153).contributions.is_empty());
}

#[test]
fn explicitly_reusable_material_keeps_its_deadline_without_being_consumed() {
    let (store, mut reminder) = fixture();
    reminder.transient = false;
    let provider = PmaContributionProvider::new(store);
    let scope = scope_wide(EntityId::new());
    let item = provider.contribution_for(&reminder, &scope, 100).unwrap();
    let registry = ContributionRegistry::new();
    let registration = registry.register_publisher(provider.manifest());
    let ticket = registry.begin_publication(registration, &scope).unwrap();
    registry.publish(ticket, vec![item.clone()]).unwrap();
    let plan = registry.collect(&scope, 150);
    registry.record_usage(plan.usage(EntityId::new(), 1, "accepted", Digest::blake3(b"seal"))).unwrap();
    assert_eq!(registry.collect(&scope, 199).contributions, vec![item]);
    assert!(registry.collect(&scope, 200).contributions.is_empty());
}

#[test]
fn reminder_addresses_and_provenance_are_bound_to_the_visible_observation() {
    use shenko_memory::{BankSnapshot, InterventionBudget, InterventionDecision,
        InterventionObservation, ModelMemoryInterventionPolicy};
    let epoch = EntityId::new();
    let visible_id = EntityId::new();
    let hidden_id = EntityId::new();
    let observation = InterventionObservation {
        epoch_id: epoch, lineage_id: EntityId::new(), now_ms: 100,
        budget: InterventionBudget { max_bank_entries: 1, ..Default::default() },
        trajectory: vec![],
        bank: vec![
            BankSnapshot { entry_id: visible_id, kind: "requirement".into(), content: "Preserve IDs.".into(),
                source_refs: vec!["record:visible".into()], scope: format!("epoch:{epoch}"), confidence: Some(1.0), age_ms: 1 },
            BankSnapshot { entry_id: hidden_id, kind: "requirement".into(), content: "Not shown to the worker.".into(),
                source_refs: vec!["record:hidden".into()], scope: format!("epoch:{epoch}"), confidence: Some(1.0), age_ms: 1 },
        ],
    };
    let answer = |scope: &str, source: &str| serde_json::json!({
        "decision":"remind", "content":"Preserve IDs.", "relevance":"current work",
        "scope":scope, "provenance":[source], "expires_in_secs":600,
    }).to_string();
    for source in [visible_id.to_string(), "record:visible".into()] {
        let decision = ModelMemoryInterventionPolicy::parse_decision(&answer(&format!("epoch:{epoch}"), &source), &observation).unwrap();
        let InterventionDecision::Remind { provenance, scope, .. } = decision else { panic!("lost reminder") };
        assert_eq!(provenance, vec![source]);
        assert_eq!(scope, format!("epoch:{epoch}"));
    }
    for scope in ["current migration".into(), format!("epoch:{}", EntityId::new()), format!("epoch:{epoch} / lineage:extra")] {
        assert!(ModelMemoryInterventionPolicy::parse_decision(&answer(&scope, "record:visible"), &observation).is_err());
    }
    for source in [hidden_id.to_string(), "record:hidden".into(), format!("entry_id:{visible_id}")] {
        assert!(ModelMemoryInterventionPolicy::parse_decision(&answer("subject", &source), &observation).is_err());
    }
}
