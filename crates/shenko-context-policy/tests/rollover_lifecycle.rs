//! The explicit-rollover lifecycle at the registry boundary: a request is an
//! armed fact that never cuts history by itself, the reminder is read without
//! being spent and answered exactly once per committed window, and a strategy
//! retired between publication and adoption cannot commit its prepared window.
use std::sync::Arc;

use shenko_context_policy::{
    Availability, ContextInput, ContextPolicy, CostReport, ModelCaps, PolicyError, PolicyManifest,
    PolicyRegistry, PreparedWindow, TriggerInput, WindowEntry, WindowLedger, estimate_tokens,
    HistorySource,
};
use shenko_types::ids::EntityId;

const CAPS: ModelCaps = ModelCaps { model_id: String::new(), max_context: 1000, vision: None };

/// A strategy that always prepares one pointer entry, so the registry contract
/// — not a real strategy's rendering — is what these cases observe.
struct Fixture;

impl ContextPolicy for Fixture {
    fn manifest(&self) -> PolicyManifest {
        PolicyManifest {
            id: "fixture".into(),
            version: "1".into(),
            contract: shenko_context_policy::CONTRACT_VERSION,
            summary: "fixture".into(),
            ..PolicyManifest::default()
        }
    }

    fn availability(&self, _caps: &ModelCaps) -> Availability {
        Availability::Available
    }

    fn should_compact(&self, trigger: &TriggerInput<'_>) -> bool {
        trigger.used_tokens as f32 > trigger.caps.max_context as f32 * 0.5
    }

    fn prepare(&self, input: &ContextInput<'_>) -> Result<PreparedWindow, PolicyError> {
        let note = "fixture stand-in".to_string();
        let to = input.items.last().map_or(input.base.leaf_seq, |item| item.at.seq);
        let entries = vec![WindowEntry::Pointer { from: 0, to, note: note.clone(), tokens: estimate_tokens(&note) }];
        Ok(PreparedWindow {
            base: input.base,
            policy: self.manifest(),
            entries,
            carried_calls: Vec::new(),
            derivatives: Vec::new(),
            cost: CostReport {
                source_tokens: input.items.iter().map(|item| item.tokens).sum(),
                window_tokens: estimate_tokens(&note),
                model_calls: 0,
                ..CostReport::default()
            },
        })
    }
}

fn registry() -> PolicyRegistry {
    PolicyRegistry::with_ledger(WindowLedger::new(Arc::new(
        shenko_context_policy::MemoryWindowLog::default(),
    )))
}

#[test]
fn a_request_is_armed_once_and_never_cuts_a_window_by_itself() {
    let registry = registry();
    let scope = registry.ledger().scope_id();
    let other = EntityId::new();
    let error = registry.request_rollover(other).unwrap_err();
    assert!(
        error.to_string().contains("rollover requested for scope"),
        "a request for another branch is refused: {error}"
    );
    assert!(!registry.needs_preparation(&CAPS, 100));
    let first = registry.request_rollover(scope).unwrap();
    let second = registry.request_rollover(scope).unwrap();
    assert_eq!(first.request_id, second.request_id, "a repeated request keeps the first");
    assert!(registry.rollover_pending());
    assert!(registry.needs_preparation(&CAPS, 100), "an explicit request asks for a boundary");
    // Arming is not cutting: the committed head is untouched.
    assert_eq!(registry.ledger().current().unwrap(), registry.ledger().current().unwrap());
}

#[test]
fn the_reminder_is_read_without_being_spent_and_answered_once() {
    let registry = registry();
    registry.set_reminder_threshold(1, 0.7);
    registry.observe_pressure(&CAPS, 500).unwrap();
    assert!(registry.pending_rollover_reminder().is_none(), "below the threshold is silent");
    registry.observe_pressure(&CAPS, 800).unwrap();
    let reminder = registry
        .pending_rollover_reminder()
        .expect("one reminder above the threshold");
    assert_eq!(reminder.used_tokens, 800);
    assert_eq!(reminder.window, registry.ledger().current().unwrap().id);
    assert_eq!(reminder.policy_epoch, 1, "the answer is fenced on the issuing installation");
    // Reading is not answering: an assembly that was refused or aborted after
    // this point leaves the reminder available for the retry.
    assert_eq!(
        registry.pending_rollover_reminder(),
        Some(reminder),
        "prompt assembly must not spend the window's only reminder"
    );
    assert!(registry.acknowledge_rollover_reminder(&reminder).unwrap());
    assert!(
        registry.pending_rollover_reminder().is_none(),
        "one reminder per committed window"
    );
    assert!(
        !registry.acknowledge_rollover_reminder(&reminder).unwrap(),
        "a repeated acknowledgement keeps the first record"
    );
    // A committed window is a new reminder opportunity; the reminder itself
    // never prepared or committed anything.
    let window = registry.ledger().current().unwrap();
    assert_eq!(window.generation, 0);
}

#[test]
fn a_reminder_threshold_belongs_to_the_installation_that_registered_it() {
    let registry = registry();
    registry.set_reminder_threshold(2, 0.6);
    registry.observe_pressure(&CAPS, 900).unwrap();
    registry.clear_reminder_threshold(1);
    assert!(registry.pending_rollover_reminder().is_some(), "an older epoch cannot clear it");
    registry.set_reminder_threshold(3, 0.6);
    registry.clear_reminder_threshold(2);
    assert!(
        registry.pending_rollover_reminder().is_some(),
        "a late cleanup of a replaced generation cannot remove its successor's threshold"
    );
}

#[test]
fn a_retired_generation_cannot_answer_its_reminder_for_its_successor() {
    let registry = registry();
    registry.set_reminder_threshold(2, 0.6);
    registry.observe_pressure(&CAPS, 900).unwrap();
    let retired = registry.pending_rollover_reminder().expect("the generation is due a reminder");
    assert_eq!(retired.policy_epoch, 2);
    // The installation is replaced between projection and commit.
    registry.set_reminder_threshold(3, 0.6);
    assert!(
        !registry.acknowledge_rollover_reminder(&retired).unwrap(),
        "a retired generation cannot record an answer with its successor's threshold"
    );
    let successor = registry
        .pending_rollover_reminder()
        .expect("the successor issues its own once for the same window");
    assert_eq!(successor.policy_epoch, 3);
    assert_eq!(successor.window, retired.window);
    assert!(registry.acknowledge_rollover_reminder(&successor).unwrap());
    assert!(registry.pending_rollover_reminder().is_none());
}

#[test]
fn a_strategy_retired_between_publication_and_adoption_cannot_commit() {
    let history = shenko_context_policy::MemoryHistory::new();
    history.append(shenko_context_policy::ItemKind::User, "hello", 10);
    let registry = registry();
    let epoch = registry.install(Arc::new(Fixture)).unwrap();
    // The genesis window holds nothing, so the automatic gain guard has no
    // ceiling to satisfy; an explicit request is what legitimately cuts a
    // window here.
    registry.request_rollover(registry.ledger().scope_id()).unwrap();
    let candidate = registry.prepare_candidate(&history, &CAPS, 0).unwrap();
    assert!(candidate.rollover);
    let base_leaf = candidate.window.base.leaf_seq;
    registry.publish(candidate).unwrap();
    let head_before = registry.ledger().current().unwrap();

    // The strategy is replaced after publication: the ledger alone would still
    // accept the window, so the refusal has to come from the registry.
    assert!(registry.uninstall("fixture", epoch));
    assert!(registry.adopt_prepared(&history).unwrap().is_none());
    assert_eq!(registry.ledger().current().unwrap(), head_before, "no window was committed");
    assert!(base_leaf <= history.leaf_seq().unwrap().unwrap_or(0));
}
