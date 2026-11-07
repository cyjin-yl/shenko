//! Residency discovery and preservation — the real probe shapes and the pool
//! rules they feed. No model call and no account is involved: the fixtures are
//! plain HTTP responses a backend really returns.

use serde_json::json;
use shenko_broker::{
    AllocationPlan, AutomaticKvPolicy, BackendFamily, Broker, CapacityPool, CapacityProvenance,
    ContextLease, KvAllocator, KvCapabilities, KvError, KvPoolState, LeaseDemand, LeaseSide,
    LeaseState, LocalModelProvider, ProviderRuntime, ResidencyIdentity, ResourceDomain,
    ResourceDomainIdentity, RetentionGuarantee, discover_kv_capacity,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A clock a test drives by hand, so a policy hold window can be exercised
/// without sleeping. Only the hysteresis regression uses it.
static POLICY_CLOCK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn policy_clock() -> u64 {
    POLICY_CLOCK_MS.load(std::sync::atomic::Ordering::Relaxed)
}

/// One probe fixture: `/v1/models` always answers, `/metrics` only when the
/// backend has a metrics surface.
async fn probe_fixture(metrics: Option<String>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let metrics = metrics.as_deref();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let mut received = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let Ok(size) = stream.read(&mut chunk).await else { break };
                if size == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..size]);
                if received.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&received);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            let (status, content_type, body) = match (path.as_str(), metrics) {
                ("/v1/models", _) => (
                    200,
                    "application/json",
                    // A real vLLM metadata answer: the model's per-request
                    // ceiling, which is not a residency pool.
                    r#"{"object":"list","data":[{"id":"fixture","max_model_len":262144}]}"#
                        .to_string(),
                ),
                ("/metrics", Some(text)) => (200, "text/plain; version=0.0.4", text.to_string()),
                _ => (404, "text/plain", String::new()),
            };
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    base_url
}

/// The cache configuration line a vLLM-family engine actually exposes. The
/// hybrid mamba labels are kept because the attention figure is one cache
/// group, not the whole effective capacity.
const CACHE_CONFIG: &str = concat!(
    "vllm:cache_config_info{block_size=\"2048\",cache_dtype=\"fp8_e4m3\",",
    "enable_prefix_caching=\"True\",engine=\"0\",mamba_block_size=\"8192\",",
    "mamba_cache_mode=\"align\",num_cpu_blocks=\"None\",num_gpu_blocks=\"740\"} 1.0"
);

const OTHER_METRICS: &str = concat!(
    "# HELP vllm:kv_cache_usage_perc KV-cache usage. 1 means 100 percent usage.\n",
    "# TYPE vllm:kv_cache_usage_perc gauge\n",
    "vllm:kv_cache_usage_perc{engine=\"0\"} 0.0\n",
    "vllm:num_requests_running{engine=\"0\"} 0.0\n",
);

#[tokio::test]
async fn actual_probe_reports_configured_pool_and_the_context_window_is_not_it() {
    let metrics =
        format!("{OTHER_METRICS}{CACHE_CONFIG}\nvllm:num_requests_waiting{{engine=\"0\"}} 0.0\n");
    let base_url = probe_fixture(Some(metrics)).await;
    let provider = LocalModelProvider::new(base_url, "fixture".into());
    let snapshot = provider.probe().await.unwrap();

    // 740 blocks x 2048 tokens is the engine's configured attention pool.
    assert_eq!(snapshot.observed_kv_units(), Some(740 * 2048));
    let entry = snapshot.capacity(CapacityPool::ContextKv).unwrap();
    assert_eq!(entry.provenance, CapacityProvenance::Configured);

    // The advertised context window is not the pool, so it cannot appear as
    // one: 262144 would be a request ceiling, not residency.
    assert_ne!(snapshot.observed_kv_units(), Some(262_144));

    // The engine itself reported prefix caching enabled, which is the
    // deployment-enabled rung. No request ever demonstrated cache_salt, so that
    // option stays unknown rather than being inherited from the family name.
    assert!(snapshot.supports(shenko_broker::NATIVE_VLLM_PREFIX_CACHING));
    assert!(!snapshot.supports(shenko_broker::NATIVE_VLLM_CACHE_SALT));

    // The retained evidence is the exact sample line the figure came from.
    let retained = snapshot
        .raw_evidence
        .iter()
        .find_map(|document| document.get("probe_body"))
        .and_then(|body| body.get("retained_lines"))
        .and_then(|lines| lines.as_array())
        .expect("the metrics body is retained as probe evidence");
    assert!(retained.iter().any(|line| line
        .as_str()
        .is_some_and(|line| line == CACHE_CONFIG)));
}

#[tokio::test]
async fn a_backend_without_a_metrics_surface_keeps_the_pool_unknown() {
    let base_url = probe_fixture(None).await;
    let provider = LocalModelProvider::new(base_url, "fixture".into());
    let snapshot = provider.probe().await.unwrap();
    assert_eq!(snapshot.observed_kv_units(), None);
    let entry = snapshot.capacity(CapacityPool::ContextKv).unwrap();
    assert_eq!(entry.value, None);
    assert_eq!(entry.provenance, CapacityProvenance::Unknown);
}

#[tokio::test]
async fn the_broker_adopts_the_discovered_pool_instead_of_only_returning_it() {
    let metrics = format!("{CACHE_CONFIG}\n");
    let base_url = probe_fixture(Some(metrics)).await;
    let provider = LocalModelProvider::new(base_url, "fixture".into());
    let (broker, handle) = Broker::with_local_provider(provider, 1).unwrap();
    assert_eq!(handle.kv_snapshot().capacity_units, None);

    broker.probe_capabilities().await.unwrap();

    let pool = handle.kv_snapshot();
    assert_eq!(pool.capacity_units, Some(740 * 2048));
    assert_eq!(pool.provenance, CapacityProvenance::Configured);
    let capabilities = handle.kv_capabilities();
    // The engine never demonstrated a reservation, a pause, a spill or a fork.
    assert!(!capabilities.hard_retention);
    assert!(!capabilities.supports_pause);
    assert!(!capabilities.supports_spill);
    assert!(!capabilities.supports_fork);
    assert!(!capabilities.cache_salt);
    // The pool mirror is what diagnostics and the handoff record read.
    assert_eq!(pool.capabilities, capabilities);
}

#[test]
fn documented_engine_pool_shapes_are_read_and_windows_never_become_a_pool() {
    // llama.cpp /props: slots x per-slot context.
    let props = json!({
        "total_slots": 4,
        "default_generation_settings": { "n_ctx": 4096 },
    });
    let observed = discover_kv_capacity(&BackendFamily::Other("llama_cpp".into()), &[props]).unwrap();
    assert_eq!(observed.tokens, 16384);
    assert_eq!(observed.provenance, CapacityProvenance::Configured);

    // SGLang server info.
    let server_info = json!({ "max_total_num_tokens": 32768 });
    assert_eq!(
        discover_kv_capacity(&BackendFamily::Other("sglang".into()), &[server_info])
            .unwrap()
            .tokens,
        32768
    );

    // A model list only advertises a per-request window: nothing is inferred.
    let models = json!({ "data": [{ "id": "m", "max_model_len": 262144 }] });
    assert!(discover_kv_capacity(&BackendFamily::Vllm, &[models.clone()]).is_none());
    assert!(discover_kv_capacity(&BackendFamily::Anthropic, &[models]).is_none());

    // A metrics body that never names the cache configuration stays unknown.
    let unrelated = json!({
        "probe_body": { "retained_lines": ["vllm:num_requests_running{engine=\"0\"} 0.0"] }
    });
    assert!(discover_kv_capacity(&BackendFamily::Vllm, &[unrelated]).is_none());
}

fn identity(engine: &str) -> ResidencyIdentity {
    ResidencyIdentity {
        engine_identity: Some("fixture-engine".to_string()),
        engine_incarnation: Some(engine.to_string()),
        model: Some("fixture".to_string()),
        tokenizer_fingerprint: None,
        template_fingerprint: None,
        toolset_digest: None,
    }
}

fn demand(context: &str, units: u64) -> LeaseDemand {
    LeaseDemand {
        context_id: context.to_string(),
        side: LeaseSide::Main,
        window_units: units,
        reusable_units: 0,
        recompute_units: units,
        return_probability: 0.5,
        priority: 0,
        identity: identity("engine-a"),
        affinity_options: Vec::new(),
    }
}

fn allocator(capabilities: KvCapabilities) -> KvAllocator {
    KvAllocator::new(Box::new(AutomaticKvPolicy::default()), capabilities)
}

#[test]
fn an_unknown_pool_claims_no_share_and_a_demonstrated_control_lifts_only_itself() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(None, CapacityProvenance::Unknown);
    let admitted = allocator.acquire(&mut pool, demand("ctx", 800)).unwrap();
    assert_eq!(admitted.plan.subject_units, None);
    assert_eq!(admitted.plan.children_units, None);
    assert_eq!(admitted.plan.guarantee, RetentionGuarantee::Advisory);
    assert_eq!(admitted.lease.retention.entitled_units, 0);
    // The context still uses units; only the share is unknown.
    assert_eq!(admitted.lease.retention.used_units, 800);

    // A configured pool with no demonstrated reservation is best effort.
    let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("ctx", 400)).unwrap();
    let plan = allocator.rebalance(&mut pool).unwrap();
    assert_eq!(plan.guarantee, RetentionGuarantee::BestEffort);
    assert_eq!(plan.subject_units, Some(1000));

    let mut capabilities = KvCapabilities::default();
    capabilities.hard_retention = true;
    allocator.apply_capabilities(&mut pool, capabilities);
    let plan = allocator.rebalance(&mut pool).unwrap();
    assert_eq!(plan.guarantee, RetentionGuarantee::Reserved);
    assert_eq!(pool.capabilities, capabilities);
}

#[test]
fn a_request_naming_an_undemonstrated_residency_option_keeps_only_a_soft_claim() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);

    let mut asked = demand("ctx", 400);
    asked.affinity_options = vec!["cache_salt".to_string()];
    let admitted = allocator.acquire(&mut pool, asked).unwrap();
    assert!(admitted.lease.affinity_gap.is_some());
    assert_eq!(admitted.lease.retention.guarantee, RetentionGuarantee::Advisory);

    // Once the backend really demonstrated the operation the same payload is a
    // residency request the lease may report as best effort.
    let mut demonstrated = KvCapabilities::default();
    demonstrated.cache_salt = true;
    allocator.apply_capabilities(&mut pool, demonstrated);
    let mut asked = demand("ctx", 400);
    asked.affinity_options = vec!["cache_salt".to_string()];
    let admitted = allocator.acquire(&mut pool, asked).unwrap();
    assert!(admitted.lease.affinity_gap.is_none());
    assert_eq!(admitted.lease.retention.guarantee, RetentionGuarantee::BestEffort);

    // A non-residency native option is never treated as a residency request.
    let mut unrelated = demand("ctx", 400);
    unrelated.affinity_options = vec!["temperature".to_string()];
    let admitted = allocator.acquire(&mut pool, unrelated).unwrap();
    assert!(admitted.lease.affinity_gap.is_none());
}

#[test]
fn a_symmetric_workload_reaches_an_even_split_and_follows_the_demand_afterwards() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("main", 400)).unwrap();

    // The children's collective asks for the same amount, so the automatic
    // policy lands on the even split without any ratio being configured.
    let mut child = demand("child", 400);
    child.side = LeaseSide::Child;
    let admitted = allocator.acquire(&mut pool, child).unwrap();
    assert_eq!(admitted.plan.subject_units, Some(500));
    assert_eq!(admitted.plan.children_units, Some(500));
    assert_eq!(pool.leases["main"].retention.entitled_units, 500);
    assert_eq!(pool.leases["child"].retention.entitled_units, 500);

    // The children's work shrinks; the split follows the demand instead of
    // staying at the even split.
    let mut smaller = demand("child", 100);
    smaller.side = LeaseSide::Child;
    let admitted = allocator.acquire(&mut pool, smaller).unwrap();
    assert_eq!(admitted.plan.subject_units, Some(800));
    assert_eq!(admitted.plan.children_units, Some(200));
}

#[test]
fn a_small_drift_stays_inside_the_hysteresis_band_and_a_pool_change_is_not_held() {
    /// A settled symmetric pool (500/500) whose child then asks for a little
    /// more than its half. The best candidate moves to a 444/556 split, a
    /// 56-unit drift that the band either absorbs or follows. The hold window
    /// never expires: the clock stays at zero.
    fn settled(hysteresis_ratio: f64) -> (KvAllocator, KvPoolState, AllocationPlan) {
        let allocator = KvAllocator::new(
            Box::new(AutomaticKvPolicy::with_clock(60_000, hysteresis_ratio, policy_clock)),
            KvCapabilities::default(),
        );
        let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);
        allocator.acquire(&mut pool, demand("main", 400)).unwrap();
        let mut child = demand("child", 400);
        child.side = LeaseSide::Child;
        allocator.acquire(&mut pool, child).unwrap();
        let mut grown = demand("child", 500);
        grown.side = LeaseSide::Child;
        let plan = allocator.acquire(&mut pool, grown).unwrap().plan;
        (allocator, pool, plan)
    }

    POLICY_CLOCK_MS.store(0, std::sync::atomic::Ordering::Relaxed);

    // 1% of the pool is 10 units, so the 56-unit drift is material and the
    // split follows the demand.
    let (_, _, switched) = settled(0.01);
    assert_eq!(switched.subject_units, Some(444));
    assert_eq!(switched.children_units, Some(556));
    assert!(!switched.rationale.contains("kept subject share"));

    // 6% of the pool is 60 units, which covers the 56-unit drift: it is noise,
    // so the previous share is kept even though another candidate scored
    // better.
    let (allocator, mut pool, held) = settled(0.06);
    assert_eq!(held.subject_units, Some(500));
    assert_eq!(held.children_units, Some(500));
    assert!(
        held.rationale.contains("kept subject share"),
        "the held plan says why it did not move: {}",
        held.rationale
    );
    assert_eq!(pool.leases["main"].retention.entitled_units, 500);
    assert_eq!(pool.leases["child"].retention.entitled_units, 500);

    // A changed pool size is a new physical fact, so the hold never applies:
    // the split is recomputed for the capacity the backend now reports. A
    // grown pool re-optimises to 488/612, a 12-unit drift the band of the *old*
    // pool would have absorbed — only the capacity rule can produce this.
    pool.capacity_units = Some(1100);
    let replanned = allocator.rebalance(&mut pool).unwrap();
    assert_eq!(replanned.subject_units, Some(488));
    assert_eq!(replanned.children_units, Some(612));
    assert!(!replanned.rationale.contains("kept subject share"));
}

#[test]
fn reported_reuse_is_recorded_and_stays_bounded_by_the_reported_input() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(4096), CapacityProvenance::Configured);
    let mut observed = demand("ctx", 120);
    observed.reusable_units = 96;
    observed.recompute_units = 24;
    let admitted = allocator.acquire(&mut pool, observed).unwrap();
    assert_eq!(admitted.lease.reusable_units, 96);
    assert_eq!(admitted.lease.recompute_units, 24);
    // A cache hit is an observation of this request, so it never hardens the
    // residency promise by itself.
    assert_eq!(admitted.lease.retention.guarantee, RetentionGuarantee::BestEffort);
}

#[test]
fn an_engine_rebuild_or_model_switch_invalidates_handles_instead_of_inheriting_them() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(4096), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("ctx", 900)).unwrap();
    assert_eq!(pool.occupied_units(), 900);

    // Same identity: the handle stays resident.
    assert_eq!(pool.invalidate_stale(&identity("engine-a")), 0);
    assert_eq!(pool.leases["ctx"].state, LeaseState::Resident);

    // A rebuilt engine cannot inherit the old residency; the record is kept.
    assert_eq!(pool.invalidate_stale(&identity("engine-b")), 1);
    assert_eq!(pool.leases["ctx"].state, LeaseState::Invalid);
    assert_eq!(pool.occupied_units(), 0);
    assert_eq!(pool.leases["ctx"].required_units, 900);

    // A handle carried over from a state with no identity is not residency.
    let mut pool = KvPoolState::new(Some(4096), CapacityProvenance::Configured);
    let mut unproven = ContextLease::new("legacy", LeaseSide::Main, 500);
    unproven.state = LeaseState::Resident;
    pool.leases.insert("legacy".to_string(), unproven);
    assert_eq!(pool.invalidate_stale(&identity("engine-a")), 1);
    assert_eq!(pool.leases["legacy"].state, LeaseState::Invalid);
}

#[test]
fn a_context_being_read_keeps_its_units_until_the_reader_drains() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(100), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("reader", 60)).unwrap();
    assert_eq!(allocator.begin_read(&mut pool, "reader").unwrap(), 1);

    // Closing while a reader is active defers the release.
    let closed = allocator.release(&mut pool, "reader").unwrap();
    assert!(!closed.released);
    assert_eq!(closed.readers, 1);
    assert_eq!(pool.leases["reader"].state, LeaseState::ReclaimPending);
    assert_eq!(pool.occupied_units(), 60);

    // Admission cannot take the space a draining reader still holds, and it
    // must not evict that context either.
    let refused = allocator.acquire(&mut pool, demand("next", 50));
    assert!(matches!(refused, Err(KvError::InsufficientCapacity { .. })));
    assert_eq!(pool.leases["reader"].state, LeaseState::ReclaimPending);
    assert_eq!(pool.leases["reader"].readers, 1);
    assert!(!pool.leases.contains_key("next"));

    // The last reader frees the units, and the waiting context fits.
    let drained = allocator.end_read(&mut pool, "reader").unwrap();
    assert!(drained.released);
    assert_eq!(pool.occupied_units(), 0);
    allocator.acquire(&mut pool, demand("next", 50)).unwrap();
    assert_eq!(pool.occupied_units(), 50);

    // `end_read` without a reader is an error, not a silent release.
    assert!(matches!(
        allocator.end_read(&mut pool, "next"),
        Err(KvError::NoReaders)
    ));
}

#[test]
fn discovery_binds_handles_to_the_backend_identity_and_unknown_never_erases_it() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(4096), CapacityProvenance::Configured);
    let mut first = demand("ctx", 900);
    first.identity.template_fingerprint = Some("template-1".to_string());
    allocator.acquire(&mut pool, first).unwrap();

    // A probe that only established the engine identity keeps the handle: the
    // template it does not report is unknown, which is not a change.
    assert_eq!(allocator.adopt_identity(&mut pool, &identity("engine-a")), 0);
    assert_eq!(pool.leases["ctx"].state, LeaseState::Resident);

    // The same engine with a different template renders a different context, so
    // the handle is invalid instead of being inherited by the new rendering.
    let mut retemplated = identity("engine-a");
    retemplated.template_fingerprint = Some("template-2".to_string());
    assert_eq!(allocator.adopt_identity(&mut pool, &retemplated), 1);
    assert_eq!(pool.leases["ctx"].state, LeaseState::Invalid);
    assert_eq!(pool.occupied_units(), 0);
    // The record stays: the old fact is kept, it only stops counting.
    assert_eq!(pool.leases["ctx"].required_units, 900);

    // A later probe that established nothing does not erase what was known.
    assert_eq!(allocator.adopt_identity(&mut pool, &ResidencyIdentity::default()), 0);
    assert_eq!(pool.identity.template_fingerprint.as_deref(), Some("template-2"));
    assert_eq!(pool.identity.engine_incarnation.as_deref(), Some("engine-a"));
}

#[test]
fn a_pool_that_shrank_requests_only_the_releases_the_excess_needs() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);
    let mut hot = demand("hot", 400);
    hot.return_probability = 0.9;
    hot.priority = 3;
    allocator.acquire(&mut pool, hot).unwrap();
    let mut cold = demand("cold", 400);
    cold.return_probability = 0.1;
    allocator.acquire(&mut pool, cold).unwrap();
    assert_eq!(pool.occupied_units(), 800);

    // Discovery now reports a smaller configured pool, as a rebuilt engine with
    // less KV cache would. Only the cold context is asked to release, and the
    // pending release is not counted as free space.
    pool.capacity_units = Some(500);
    allocator.rebalance(&mut pool).unwrap();
    assert_eq!(pool.leases["cold"].state, LeaseState::ReclaimPending);
    assert_eq!(pool.leases["hot"].state, LeaseState::Resident);
    assert_eq!(pool.reclaim_pending_units(), 400);
    assert_eq!(pool.occupied_units(), 800);

    // A second pass does not evict the pool again: the pending release already
    // covers the excess.
    allocator.rebalance(&mut pool).unwrap();
    assert_eq!(pool.leases["hot"].state, LeaseState::Resident);
    assert_eq!(pool.reclaim_pending_units(), 400);
}

#[test]
fn a_pending_release_is_revoked_when_the_context_is_read_again() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("ctx", 400)).unwrap();
    assert_eq!(allocator.begin_read(&mut pool, "ctx").unwrap(), 1);
    // A close is requested while the read is active, so it stays pending.
    assert!(!allocator.release(&mut pool, "ctx").unwrap().released);
    assert_eq!(pool.leases["ctx"].state, LeaseState::ReclaimPending);

    // The next turn reads the same context again: the never-confirmed release
    // is revoked instead of freeing the KV underneath this read.
    assert_eq!(allocator.begin_read(&mut pool, "ctx").unwrap(), 2);
    assert_eq!(pool.leases["ctx"].state, LeaseState::Resident);
    assert_eq!(pool.leases["ctx"].retention.reclaim_pending_units, 0);
    assert_eq!(pool.occupied_units(), 400);

    // Draining the readers now leaves the context resident.
    assert!(!allocator.end_read(&mut pool, "ctx").unwrap().released);
    assert!(!allocator.end_read(&mut pool, "ctx").unwrap().released);
    assert_eq!(pool.leases["ctx"].state, LeaseState::Resident);
    assert_eq!(pool.occupied_units(), 400);
}

#[test]
fn a_refused_context_does_not_erase_the_context_that_was_already_there() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(100), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("kept", 90)).unwrap();
    allocator
        .begin_read(&mut pool, "kept")
        .expect("a resident context can be read");

    // A larger renewal cannot fit while the context is being read, and nothing
    // else may be reclaimed, so the existing lease keeps its exact accounting
    // instead of being replaced by the refused one.
    assert!(allocator.acquire(&mut pool, demand("kept", 150)).is_err());
    assert_eq!(pool.leases["kept"].required_units, 90);
    assert_eq!(pool.leases["kept"].readers, 1);
    assert_eq!(pool.occupied_units(), 90);
}

#[test]
fn an_unresolved_unknown_outcome_keeps_its_context_charged_and_unevictable() {
    let allocator = allocator(KvCapabilities::default());
    let mut pool = KvPoolState::new(Some(1000), CapacityProvenance::Configured);
    allocator.acquire(&mut pool, demand("ctx", 600)).unwrap();
    assert_eq!(allocator.begin_read(&mut pool, "ctx").unwrap(), 1);

    // The provider outcome is unknown, so the read hold it started stays: a
    // dropped connection is not proof that the backend stopped reading.
    assert!(allocator.hold_unknown_read(&mut pool, "request-1", "ctx"));
    assert!(pool.has_unknown_read("ctx"));
    assert_eq!(pool.occupied_units(), 600);

    // A pool that shrank may not evict a context an unresolved attempt may
    // still be reading, and a release cannot claim the KV is free.
    pool.capacity_units = Some(400);
    allocator.rebalance(&mut pool).unwrap();
    assert_eq!(pool.leases["ctx"].state, LeaseState::Resident);
    assert_eq!(pool.occupied_units(), 600);
    assert!(matches!(
        allocator.release(&mut pool, "ctx"),
        Err(KvError::UnresolvedUnknownRead(_))
    ));

    // A context nothing observed is never charged by an unknown outcome: the
    // pool does not invent occupancy.
    assert!(!allocator.hold_unknown_read(&mut pool, "request-2", "never-seen"));

    // The original terminal evidence drains exactly that hold; a second
    // reconciliation is a no-op instead of ending someone else's read.
    assert!(allocator.release_unknown_read(&mut pool, "request-1"));
    assert!(!pool.has_unknown_read("ctx"));
    assert!(!allocator.release_unknown_read(&mut pool, "request-1"));
    assert_eq!(pool.leases["ctx"].readers, 0);
    assert_eq!(pool.occupied_units(), 600);

    // Only once nothing unresolved names the context is a release admissible.
    assert!(allocator.release(&mut pool, "ctx").unwrap().released);
    assert_eq!(pool.occupied_units(), 0);
}

#[test]
fn the_handoff_record_carries_the_unknown_read_and_never_claims_termination() {
    let allocator = allocator(KvCapabilities::default());
    let domain = ResourceDomain::new(
        ResourceDomainIdentity {
            domain_id: "fixture-domain".into(),
            engine_identity: "fixture-engine".into(),
            engine_incarnation: Some("engine-a".into()),
            epoch: 1,
        },
        1,
        CapacityProvenance::Configured,
        Some(4096),
        CapacityProvenance::Configured,
    );
    {
        let mut pool = domain.kv_mut();
        allocator.acquire(&mut pool, demand("ctx", 900)).unwrap();
        allocator.begin_read(&mut pool, "ctx").unwrap();
        assert!(allocator.hold_unknown_read(&mut pool, "request-1", "ctx"));
    }
    let handoff = domain.handoff_snapshot();
    assert_eq!(handoff["kv"]["unknown_reads"]["request-1"], json!("ctx"));
    assert_eq!(handoff["kv"]["leases"]["ctx"]["readers"], json!(1));
    assert_eq!(handoff["remote_termination_claimed"], json!(false));

    {
        let mut pool = domain.kv_mut();
        assert!(allocator.release_unknown_read(&mut pool, "request-1"));
    }
    let reconciled = domain.handoff_snapshot();
    assert!(reconciled["kv"].get("unknown_reads").is_none());
    assert_eq!(reconciled["kv"]["leases"]["ctx"]["readers"], json!(0));
}
