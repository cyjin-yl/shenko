//! Behavioural seams for the composed policy namespaces: real durable policy
//! selection and live context registry adoption, including restart/failure.
use super::*;
use shenko_types::ids::EntityId;
use shenko_ui::config_service::{ConfigActor, ConfigApplyRequest, ConfigService};

fn request(namespace: &str, key: &str, desired: Value, base: u64) -> ConfigApplyRequest {
    ConfigApplyRequest {
        namespace: namespace.into(),
        logical_key: key.into(),
        origin_identity: format!("policy-test/{}", EntityId::new()),
        actor: ConfigActor::Subject {
            task_id: EntityId::new(),
            delegation_ref: None,
        },
        desired,
        expected_base_revision: Some(base),
        coordinating_task_id: None,
    }
}

fn fixture() -> Result<(std::path::PathBuf, Arc<Store>)> {
    let root = std::env::temp_dir().join(format!("shenko-policy-config-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    Ok((root, store))
}

fn quota_policy() -> shenko_quota::QuotaPolicy {
    use shenko_quota::*;
    QuotaPolicy {
        policy_id: EntityId::new(),
        revision: 1,
        profiles: vec![ModelProfile {
            profile_id: "local".into(),
            model_ref: "test-model".into(),
            local: true,
            cost_per_request: Vec::new(),
            effort_levels: Vec::new(),
            max_output_tokens: None,
        }],
        assumptions: Vec::new(),
        min_progress_per_hour: 0.0,
        switching_hysteresis: 0.0,
        on_unknown: UnknownQuota::Block,
        allow_overage: false,
        spending_authority_ref: None,
        decision_ttl: chrono::Duration::minutes(5),
        observation_sources: Vec::new(),
        objective: PolicyObjective::Balanced,
        target_utilization_range: None,
        planning_horizon: None,
        emergency_reserve_by_window: Vec::new(),
        uncertainty_margin: None,
        admission_pacing: AdmissionPacing::default(),
    }
}

#[test]
fn quota_config_changes_the_admission_policy_and_reopens_without_replaying_history() -> Result<()> {
    let (root, store) = fixture()?;
    let service = ConfigService::new(Arc::clone(&store));
    service.register(Arc::new(QuotaNamespace(Arc::clone(&store))));
    let first = quota_policy();
    let first_request = request(QUOTA_NAMESPACE, QUOTA_KEY, json!(first), 0);
    let result = service.apply(first_request.clone())?;
    ensure!(result.change.state == "applied" && result.change.adopted.len() == 1);
    let replay = service.apply(first_request)?;
    ensure!(replay.replayed && replay.change.change_id == result.change.change_id);
    let mut second = first.clone();
    second.revision += 1;
    second.profiles[0].profile_id = "replacement-local".into();
    service.apply(request(QUOTA_NAMESPACE, QUOTA_KEY, json!(second), 1))?;
    let policy = store
        .latest_quota_policy()?
        .context("quota policy not selected")?;
    let decision = shenko_quota::decide(
        &policy,
        &[],
        &std::collections::HashMap::from([(
            "replacement-local".into(),
            shenko_quota::BackendState::Available,
        )]),
        None,
        chrono::Utc::now(),
    )?;
    ensure!(
        matches!(decision.choice, shenko_quota::Choice::Local { ref profile_id }
        if profile_id == "replacement-local")
    );
    // Idempotent storage of an older policy is not selection of that policy.
    let historical = service.apply(request(QUOTA_NAMESPACE, QUOTA_KEY, json!(first), 2))?;
    ensure!(historical.change.state == "failed" && historical.change.adopted.is_empty());
    let view = service.read(QUOTA_NAMESPACE, QUOTA_KEY)?;
    ensure!(view.desired_value == Some(json!(first)) && view.current_value == Some(json!(second)));
    ensure!(
        view.adoptions
            .iter()
            .all(|adoption| adoption.adopted_revision == 2)
    );
    drop(service);
    drop(store);
    let reopened = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let service = ConfigService::new(Arc::clone(&reopened));
    service.register(Arc::new(QuotaNamespace(Arc::clone(&reopened))));
    ensure!(
        service.read(QUOTA_NAMESPACE, QUOTA_KEY)?.current_value == Some(json!(second)),
        "reopen must not replay the failed historical policy request"
    );
    // Rollback is an appended revision with the desired former inputs.
    let mut restored = first;
    restored.revision = 3;
    let rollback = service.apply(request(QUOTA_NAMESPACE, QUOTA_KEY, json!(restored), 3))?;
    ensure!(
        rollback.change.state == "applied" && reopened.latest_quota_policy()? == Some(restored)
    );
    drop(service);
    drop(reopened);
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn context_preference_is_live_and_restart_retains_unknown_strategy_requests() -> Result<()> {
    let (root, store) = fixture()?;
    let registry = Arc::new(PolicyRegistry::new());
    registry.install(Arc::new(shenko_compact_budget::BudgetCompact::new(
        Default::default(),
    )))?;
    registry.install(Arc::new(shenko_compact_snap::SnapCompact::new(
        Default::default(),
    )))?;
    let service = ConfigService::new(Arc::clone(&store));
    service.register(Arc::new(ContextPreferenceNamespace::open(
        Arc::clone(&store),
        Arc::clone(&registry),
    )?));
    let desired = json!([
        shenko_compact_snap::POLICY_ID,
        "future.compactor",
        shenko_compact_budget::POLICY_ID
    ]);
    let result = service.apply(request(
        CONTEXT_NAMESPACE,
        PREFERENCE_KEY,
        desired.clone(),
        0,
    ))?;
    ensure!(result.change.state == "applied" && result.change.adopted.len() == 1);
    ensure!(json!(registry.preference()) == desired);
    let inspected = inspect_context_preference(&root, &[])?;
    ensure!(inspected["preference"] == desired);
    ensure!(inspected["preference_source"] == "last recorded adoption");
    ensure!(
        inspected["desired_revision"] == 1 && inspected["adoptions"][0]["adopted_revision"] == 1
    );
    ensure!(inspected["runtime_observed"] == false);
    let receipt = result
        .change
        .receipt
        .context("missing context adoption receipt")?;
    ensure!(receipt.detail.contains("future.compactor"));
    let observation: Value =
        serde_json::from_slice(&store.cas.read_object(&receipt.observation_refs[0].digest)?)?;
    ensure!(observation["selection"]["unknown"] == json!(["future.compactor"]));
    ensure!(
        observation["selection"]["adopted"]
            == json!([
                shenko_compact_snap::POLICY_ID,
                shenko_compact_budget::POLICY_ID
            ])
    );
    let rejected = service.apply(request(CONTEXT_NAMESPACE, PREFERENCE_KEY, json!([""]), 1))?;
    ensure!(rejected.change.state == "failed" && rejected.change.adopted.is_empty());
    let inspected = inspect_context_preference(&root, &[])?;
    ensure!(inspected["preference"] == desired && inspected["adopted_value"] == desired);
    ensure!(inspected["preference_source"] == "last recorded adoption");
    ensure!(inspected["adopted_revision"] == 1 && inspected["adopted_value_error"].is_null());
    ensure!(inspected["desired_value"] == json!([""]) && inspected["desired_revision"] == 2);
    ensure!(
        inspected["desired_change_state"] == "failed"
            && inspected["desired_value_error"].is_string()
    );
    ensure!(
        inspected["runtime_observed"] == false,
        "offline durable adoption is not a live registry observation"
    );
    ensure!(
        service
            .read(CONTEXT_NAMESPACE, PREFERENCE_KEY)?
            .current_value
            == Some(desired.clone())
    );
    ensure!(
        json!(registry.preference()) == desired,
        "invalid preference mutated live selection"
    );
    drop(service);
    drop(registry);
    drop(store);
    let reopened = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let registry = Arc::new(PolicyRegistry::new());
    let service = ConfigService::new(Arc::clone(&reopened));
    service.register(Arc::new(ContextPreferenceNamespace::open(
        Arc::clone(&reopened),
        Arc::clone(&registry),
    )?));
    let restored = service.read(CONTEXT_NAMESPACE, PREFERENCE_KEY)?;
    ensure!(restored.desired_value == Some(json!([""])));
    ensure!(restored.current_value == Some(desired.clone()));
    ensure!(restored.current_value_error.is_none());
    ensure!(restored.adoptions.len() == 1 && restored.adoptions[0].adopted_revision == 1);
    ensure!(
        json!(registry.preference()) == desired,
        "restart must retain the actual adoption after a newer desired value failed"
    );
    ensure!(
        reopened
            .config_change(rejected.change.change_id)?
            .is_some_and(|change| change.state == "failed" && change.adopted.is_empty())
    );
    drop(service);
    drop(registry);
    drop(reopened);
    std::fs::remove_dir_all(root)?;
    Ok(())
}
