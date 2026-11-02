use std::collections::BTreeSet;

use shenko_context_policy::contribution::{ContributionPayload, ContributionRegistry, scope_wide};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use shenko_types::plugin_runtime::{
    ContextContribution, ContributionKind, ContributionLifetime, ContributionScope,
    ContributionSlot, ContributionSource,
};

fn id(label: &str) -> EntityId {
    EntityId::derived("contribution-registry-test", label.as_bytes())
}

fn material(scope: &ContributionScope, producer: &str, text: &str) -> ContextContribution {
    let bytes = ContributionPayload::seal(text).unwrap();
    ContextContribution {
        key: "reminder".into(),
        slot: ContributionSlot::TransientReminder,
        source: ContributionSource {
            plugin_id: id(producer),
            plugin_version: "1".into(),
            artifact_digest: Some(Digest::blake3(producer.as_bytes())),
            kind: ContributionKind::Reminder,
        },
        scope: scope.clone(),
        input_revision: "revision-1".into(),
        lifetime: ContributionLifetime::ThisRequest,
        required: false,
        budget_tokens: 10,
        tokens: 1,
        data: ObjectRef {
            digest: Digest::blake3(&bytes),
            bytes: bytes.len() as u64,
            media_type: "application/json".into(),
            store_namespace: "fixture".into(),
            locator: "opaque:fixture".into(),
            codec: SchemaRef {
                name: "ContributionPayload".into(),
                major: 1,
                digest: Digest::blake3(b"ContributionPayload/v1"),
            },
        },
        prepared_at_ms: 1,
    }
}

fn publish(
    registry: &ContributionRegistry,
    scope: &ContributionScope,
    materials: Vec<ContextContribution>,
) -> Vec<EntityId> {
    let mut groups: Vec<(ContributionSource, Vec<ContextContribution>)> = Vec::new();
    for material in materials {
        if let Some((_, items)) = groups
            .iter_mut()
            .find(|(source, _)| *source == material.source)
        {
            items.push(material);
        } else {
            groups.push((material.source.clone(), vec![material]));
        }
    }
    groups
        .into_iter()
        .map(|(source, materials)| {
            let registration = registry.register_publisher(source);
            let publication = registry.begin_publication(registration, scope).unwrap();
            registry.publish(publication, materials).unwrap();
            registration
        })
        .collect()
}

#[test]
fn a_conflicted_identity_cannot_be_resurrected_by_a_third_producer() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    publish(
        &registry,
        &scope,
        vec![
            material(&scope, "A", "first"),
            material(&scope, "B", "second"),
            material(&scope, "C", "third"),
        ],
    );

    let plan = registry.collect(&scope, 2);
    assert!(plan.contributions.is_empty());
    assert_eq!(
        plan.omitted
            .iter()
            .map(|item| item.source.plugin_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([id("A"), id("B"), id("C")])
    );
}

#[test]
fn another_tasks_requirement_does_not_block_scope_wide_collection() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let task_scope = ContributionScope {
        scope_id: scope.scope_id,
        task_id: Some(id("task")),
    };
    let mut task_material = material(&task_scope, "producer", "task requirement");
    task_material.required = true;
    publish(&registry, &task_scope, vec![task_material]);

    let plan = registry.collect(&scope, 2);
    assert!(plan.contributions.is_empty());
    assert!(plan.missing_required.is_empty());
    assert_eq!(plan.omitted[0].source.plugin_id, id("producer"));
}

#[test]
fn single_use_consumption_is_bound_to_the_collected_scope() {
    let registry = ContributionRegistry::new();
    let first_scope = scope_wide(id("first-scope"));
    let other_scope = scope_wide(id("other-scope"));
    let other = material(&other_scope, "producer", "same bytes");
    publish(
        &registry,
        &first_scope,
        vec![material(&first_scope, "producer", "same bytes")],
    );
    publish(&registry, &other_scope, vec![other.clone()]);
    let plan = registry.collect(&first_scope, 2);
    registry
        .record_usage(plan.usage(id("request"), 1, "revision-1", Digest::blake3(b"seal")))
        .unwrap();

    assert!(registry.collect(&first_scope, 2).contributions.is_empty());
    assert_eq!(registry.collect(&other_scope, 2).contributions, vec![other]);
}

#[test]
fn older_usage_cannot_consume_a_new_input_revision_with_identical_bytes() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let first = material(&scope, "producer", "same bytes");
    let registration = publish(&registry, &scope, vec![first.clone()])[0];
    let plan = registry.collect(&scope, 2);
    let mut replacement = first;
    replacement.input_revision = "revision-2".into();
    let publication = registry.begin_publication(registration, &scope).unwrap();
    registry
        .publish(publication, vec![replacement.clone()])
        .unwrap();
    registry
        .record_usage(plan.usage(id("request"), 1, "revision-1", Digest::blake3(b"old seal")))
        .unwrap();

    assert_eq!(registry.collect(&scope, 2).contributions, vec![replacement]);
}

#[test]
fn another_slot_does_not_satisfy_an_omitted_required_identity() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let mut required = material(&scope, "producer", "required");
    required.required = true;
    required.tokens = required.budget_tokens + 1;
    let mut other_slot = material(&scope, "producer", "unrelated slot");
    other_slot.slot = ContributionSlot::SelectedEvent;
    publish(
        &registry,
        &scope,
        vec![required.clone(), other_slot.clone()],
    );

    let plan = registry.collect(&scope, 2);
    assert_eq!(plan.contributions, vec![other_slot]);
    assert_eq!(plan.missing_required, vec![required.key]);
}

#[test]
fn aggregate_token_cost_cannot_wrap_or_panic() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let mut first = material(&scope, "producer", "first");
    first.key = "a".into();
    first.tokens = u64::MAX;
    first.budget_tokens = u64::MAX;
    let mut overflow = material(&scope, "producer", "overflow");
    overflow.key = "b".into();
    publish(&registry, &scope, vec![first.clone(), overflow]);

    let plan = registry.collect(&scope, 2);
    assert_eq!(plan.total_tokens, u64::MAX);
    assert_eq!(plan.contributions, vec![first]);
    assert_eq!(plan.omitted[0].key, "b");
    assert!(plan.omitted[0].reason.starts_with("budget_exceeded:"));
}

#[test]
fn late_publication_cannot_revive_an_unregistered_producer() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let old = material(&scope, "producer", "old observation");
    let registration = publish(&registry, &scope, vec![old.clone()])[0];
    let late = registry.begin_publication(registration, &scope).unwrap();
    assert!(registry.unregister(registration));
    assert!(registry.collect(&scope, 2).contributions.is_empty());
    let replacement = material(&scope, "producer", "new observation");
    publish(&registry, &scope, vec![replacement.clone()]);
    assert!(registry.publish(late, vec![old]).is_err());
    assert!(!registry.unregister(registration));
    assert_eq!(registry.collect(&scope, 2).contributions, vec![replacement]);
}

#[test]
fn newer_preparation_fences_an_older_completion() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let old = material(&scope, "producer", "old observation");
    let registration = registry.register_publisher(old.source.clone());
    let late = registry.begin_publication(registration, &scope).unwrap();
    let current = registry.begin_publication(registration, &scope).unwrap();
    let replacement = material(&scope, "producer", "new observation");
    registry
        .publish(current, vec![replacement.clone()])
        .unwrap();
    assert!(registry.publish(late, vec![old]).is_err());
    assert_eq!(registry.collect(&scope, 2).contributions, vec![replacement]);
}

#[test]
fn usage_replay_cannot_rewrite_a_request_or_resurrect_its_material() {
    let registry = ContributionRegistry::new();
    let scope = scope_wide(id("scope"));
    let item = material(&scope, "producer", "once");
    let registration = publish(&registry, &scope, vec![item.clone()])[0];
    let usage =
        registry
            .collect(&scope, 2)
            .usage(id("request"), 1, "revision-1", Digest::blake3(b"seal"));
    registry.record_usage(usage.clone()).unwrap();
    registry.record_usage(usage.clone()).unwrap();
    let mut conflict = usage.clone();
    conflict.sent.clear();
    assert!(registry.record_usage(conflict).is_err());
    assert_eq!(registry.usage(&usage.request_id), Some(usage));
    assert!(registry.unregister(registration));
    let mut later = item;
    later.prepared_at_ms += 100;
    publish(&registry, &scope, vec![later]);
    assert!(registry.collect(&scope, 102).contributions.is_empty());
}
