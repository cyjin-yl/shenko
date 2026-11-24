//! Shared canonical subject-step fixtures over the real Store and CAS.
use anyhow::Result;
use shenko_store::Store;
use std::sync::Arc;

pub fn recorded_result_with_docket(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    mut step: shenko_types::subject::SubjectStep,
    semantic_wait: Option<shenko_types::subject::SemanticWait>,
    reconciliation_docket: &[serde_json::Value],
) -> Result<shenko_types::subject::SubjectStep> {
    use shenko_types::{ids::*, subject::*};
    let initial_revision = step.record_revision.0;
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(Arc::clone(store), EntityId(head.branch_id.0)),
    ));
    let profile = shenko_types::object::Digest::blake3(b"store-roundtrip-profile");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    let staged = store.stage_subject_inputs(head, &step, &[])?;
    let input_raw_event_ids: Vec<_> = staged.iter().map(|record| record.event_id).collect();
    let events = staged
        .iter()
        .map(|record| {
            serde_json::from_str::<serde_json::Value>(&record.data)
                .map(|data| data["inbox_event"].clone())
        })
        .collect::<serde_json::Result<Vec<_>>>()?;
    let plan_schema = if reconciliation_docket.is_empty() {
        12
    } else {
        13
    };
    let plan = serde_json::json!({
        "schema_version":plan_schema, "reconciliation_docket":reconciliation_docket,
        "step_id":step.step_id, "step_no":step.step_no, "events":events, "provider_selection":{"schema_version":1,"binding":null,"model":null,"profile_id":null,"profile_revision":null,"credential_fingerprint":null,"resource_domain_id":null,"serializer_revision":null}, "request_id":"00000000-0000-0000-0000-000000000000",
        "provider_binding":null, "semantic_wait":semantic_wait, "identity":null,
        "identity_provider_revision":"shenko.identity/default@1",
        "context":{"source":{"scope_id":EntityId(head.branch_id.0),
            "source_commit_seq":store.current_commit_seq().unwrap(),
            "leaf_seq":store.session_leaf_seq(EntityId(head.branch_id.0)).unwrap(),
            "input_raw_event_ids":input_raw_event_ids, "window_id":window.id,
            "window_generation":window.generation, "model_generation":1},
        "profile_digest":profile,
        "estimated_input_tokens":null, "budget_input_tokens":null,
        "trigger_budget_input_tokens":null,
        "budget_provenance":"store fixture without measured current input",
        "reserved_output_tokens":4096, "unfinished_call_ids":[],
        "capabilities":null, "observed_input_tokens":null,
        "token_counter":"heuristic:cjk-1-other-4@1; not a provider tokenizer",
        "policy_contract":shenko_context_policy::CONTRACT_VERSION,
        "selection":{"outcome":"unavailable","reason":"store_fixture_without_model_capabilities"},
        },
        "payload":{"system_prompt":"store recovery fixture", "messages":[],
            "max_tokens":4096, "tools":[]},
    });
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(initial_revision + 1);
    step.input_plan_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&plan).unwrap(),
                "application/json",
                "input_plans",
                "subject_step_input",
                plan_schema,
            )
            .unwrap()
            .object,
    );
    store
        .upsert_step(&step, head, RecordRevision(initial_revision))
        .unwrap();
    step.state = SubjectStepState::AwaitingModel;
    step.record_revision = RecordRevision(initial_revision + 2);
    step.active_request_id = Some(EntityId::new());
    store
        .upsert_step(&step, head, RecordRevision(initial_revision + 1))
        .unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.record_revision = RecordRevision(initial_revision + 3);
    step.model_result_ref = Some(
        store
            .cas
            .seal_object("继续先前关注".as_bytes(), "text/plain", "test", "raw", 1)
            .unwrap()
            .object,
    );
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: step.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    store
        .upsert_step(&step, head, RecordRevision(initial_revision + 2))
        .unwrap();
    Ok(step)
}

pub fn committed_candidate_with(
    store: &Store,
    mut step: shenko_types::subject::SubjectStep,
    next: shenko_types::ids::EntityId,
    publications: Vec<shenko_types::subject::PublicationIntent>,
) -> shenko_types::subject::SubjectStep {
    use shenko_types::{ids::*, subject::*};
    let decision = StepDecision {
        schema_version: 8,
        output: shenko_types::output::ParsedOutput {
            schema_version: 5,
            step_id: step.step_id,
            raw_ref: step.model_result_ref.clone().unwrap(),
            text: "继续先前关注".into(),
            controls: vec![],
        },
        identity_intent: None,
        step_id: step.step_id,
        input_plan_ref: step.input_plan_ref.clone().unwrap(),
        model_result_ref: step.model_result_ref.clone(),
        applied_steering_refs: vec![],
        event_dispositions: vec![EventDisposition {
            event_id: step.step_id,
            disposition: Disposition::Acknowledged,
            reason: None,
            priority: None,
            review_after: None,
        }],
        delegated_task_intents: vec![],
        child_dialogue_intents: vec![],
        memory_intents: vec![],
        policy_intents: vec![],
        publication_intents: publications,
        pending_obligations: vec![],
        attention_policy: None,
        evidence_refs: vec![],
        unknown_acknowledgements: vec![],
        successor_step_id: next,
    };
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "test",
                "StepDecision",
                8,
            )
            .unwrap()
            .object,
    );
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(step.record_revision.0 + 1);
    step
}
