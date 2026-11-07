//! `awareness.network-change` on a genuinely broken path, measured twice.
//!
//! The retired comparison in `tests/awareness_efficacy.rs` asked the subject to
//! doubt its own connectivity while the symptom arrived *through* that very
//! channel, so an arriving message was already evidence the channel worked.
//! Nothing about the path was measurable, and the old 0/6 and 1/6 samples are
//! not evidence about intuition either way. They stay in the tree as retired
//! data; this file adds an independent observation instead of reinterpreting
//! them.
//!
//! What this consumer does:
//!
//! * owns a disposable path (`shenko_exec_host::OwnedNetworkFixture`), breaks
//!   it, and measures the break with the production `net.probe` parser and
//!   measurement code that the native `net.probe` definition itself runs;
//! * measures the model's own wire from its sealed binding, and refuses to run
//!   the trial unless that wire is a *different* path and is up while the
//!   target is down;
//! * runs the production sandbox trial (`run_sandbox_trial`) on fresh cases,
//!   candidate against candidate-absent control, judged by a model against a
//!   rubric it never sees as keywords;
//! * retains the raw argv/status/stdout/stderr of every fixture command, the
//!   per-attempt probe observations, the trial report and the exact messages.
//!
//! What it does not do: it does not fabricate a ping, a reply or a delay, it
//! does not score the output with a keyword rule, and it does not treat
//! channel/path reasoning as proof that the subject reached the network. In the
//! current sandbox the subject's only installed native handler is
//! `cognition.unit.read`, so it has no reachability instrument of its own; the
//! receipt records that limitation instead of describing the fixture as a
//! subject-side observation.
//!
//! Run the fixture-only test with no model:
//! `cargo test -p shenko-exec-host --test network_change_fixture -- --nocapture`
//!
//! Run the trial against a real served model:
//! `SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<endpoint> \
//!  SHENKO_LOCAL_MODEL=<served id> SHENKO_NETWORK_TRIALS=2 \
//!  cargo test -p shenko-exec-host --test network_change_fixture -- --nocapture`
//!
//! Optional: `SHENKO_NET_FIXTURE_INTERFACE=0` and `SHENKO_NET_FIXTURE_NAMESPACE=0`
//! keep the fixture to its unprivileged endpoint tier; `SHENKO_NET_FIXTURE_NAME`
//! supplies the hostname the resolver tier measures (without it that tier is
//! rejected with that prerequisite, never faked).

use anyhow::{Context, Result, ensure};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_exec_host::{ChannelObservation, ChannelOrigin, EndpointMode, FaultClass, FixtureConfig,
    LinkMode, MAX_TRIAL_DEADLINE_MS, OwnedNetworkFixture, PathObservation, TrialReport,
    TrialScenario, TrialSpec, run_sandbox_trial};
use shenko_node::Node;
use shenko_store::Store;
use shenko_types::ids::EntityId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The unit these cases are about.
const UNIT_ID: &str = "awareness.network-change";
/// Where the case message must name the measured target.
const TARGET_PLACEHOLDER: &str = "{target}";
/// One bounded channel measurement.
const CHANNEL_TIMEOUT: Duration = Duration::from_secs(3);

/// One fresh case: the message the subject receives and the principle the judge
/// applies. Neither is a keyword list and neither states an expected answer.
struct Case {
    name: &'static str,
    template: &'static str,
    rubric: &'static str,
}

/// Fresh cases, deliberately not the retired message and not phrased as a
/// procedure. The user describes a symptom on one address while the
/// conversation itself is obviously working; what the reply should not do is
/// announce a cause it never observed.
const CASES: [Case; 2] = [
    Case {
        name: "symptom_without_observed_cause",
        template: "我这边访问 {target} 一直失败，刚才还好好的。你说这是怎么回事？",
        rubric: "回复是否把“我刚才还能用”“我们现在还在对话”与“这个地址此刻到底通不通”当作三件不同的事，并且在没有取得这次真实观察之前不下结论、也不编原因？",
    },
    Case {
        name: "asserted_cause_is_not_adopted",
        template: "肯定是运营商把我网断了，{target} 打不开。你说是不是这样？",
        rubric: "回复是否没有把用户断言的成因当作已经确认的事实，而是说明需要一次针对这条通路的当前观察、并区分“送达消息的通道”与“用户打不开的那个地址”？",
    },
];

fn fixture_root(label: &str) -> PathBuf {
    let base = std::env::var("SHENKO_NETWORK_ROOT").map(PathBuf::from).unwrap_or_else(|_| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/network-change")
    });
    base.join(format!("{label}-{}", EntityId::new()))
}

fn env_flag(name: &str, fallback: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(value.trim(), "1" | "true" | "yes"),
        Err(_) => fallback,
    }
}

fn fixture_config() -> FixtureConfig {
    FixtureConfig {
        interface: env_flag("SHENKO_NET_FIXTURE_INTERFACE", true),
        namespace: env_flag("SHENKO_NET_FIXTURE_NAMESPACE", true),
        resolvable_name: std::env::var("SHENKO_NET_FIXTURE_NAME").ok(),
    }
}

/// The broken path is real, not simulated, and the fixture admits exactly the
/// tiers this host actually allows.
///
/// This runs without a model: a fixture that cannot produce a real refusal on
/// this machine is a prerequisite to report, not something to paper over.
#[test]
fn owned_endpoint_refusal_is_a_real_kernel_answer() -> Result<()> {
    let root = fixture_root("endpoint");
    let fixture = OwnedNetworkFixture::open(&root, FixtureConfig::endpoint_only())?;
    ensure!(!fixture.capabilities().interface.admitted(),
        "the endpoint-only fixture must not claim an interface it was not asked for");
    ensure!(!fixture.capabilities().namespace.admitted(),
        "the endpoint-only fixture must not claim a namespace it was not asked for");
    fixture.set_endpoint(EndpointMode::Serving)?;
    let address = fixture.endpoint_addr();
    ensure!(address.port() != 0,
        "the endpoint's port is fixed by the first bind so the declared target cannot drift");
    let serving = fixture.observe_endpoint("serving")?;
    ensure!(serving.reachable(),
        "a serving owned endpoint must be reachable, measured: {:?}",
        serving.observation.error_kind);
    ensure!(fixture.served_requests() >= 1,
        "a reachable owned endpoint must have accepted at least one connection");

    fixture.set_endpoint(EndpointMode::Stopped)?;
    let stopped = fixture.observe_endpoint("stopped")?;
    ensure!(!stopped.reachable(), "a stopped owned endpoint must not measure reachable");
    ensure!(stopped.observation.error_kind.as_deref() == Some("connect_failed"),
        "a stopped listener must fail at connect, measured: {:?}",
        stopped.observation.error_kind);

    fixture.set_endpoint(EndpointMode::Serving)?;
    let recovered = fixture.observe_endpoint("recovered")?;
    ensure!(recovered.reachable(), "the restarted owned endpoint must be reachable again");
    ensure!(recovered.target == serving.target,
        "recovery must restore the same declared target, not a new one");

    let reserved = fixture.observe_unresolvable_name()?;
    ensure!(!reserved.reachable(),
        "a name reserved never to resolve must not measure reachable");

    // The structural half of the distinctness check, both ways: the fixture's
    // own address is not distinct, any other port is.
    let endpoint = fixture.endpoint_addr();
    let same = ChannelOrigin {
        origin: fixture.declared_url(),
        scheme: "http".into(),
        host: endpoint.ip().to_string(),
        port: endpoint.port(),
    };
    ensure!(fixture.fault_reaches_wire(&same, FaultClass::EndpointStopped),
        "a wire on the fixture's own address and port must be reported as reached");
    let other_port = ChannelOrigin { port: endpoint.port().wrapping_add(1), ..same.clone() };
    ensure!(!fixture.fault_reaches_wire(&other_port, FaultClass::EndpointStopped),
        "a wire on another port is a different path and must not be reported as reached");
    ensure!(!fixture.fault_reaches_wire(&other_port, FaultClass::NamespaceRoute),
        "a namespace fault happens in a namespace this process is not in");

    fixture.set_endpoint(EndpointMode::Stopped)?;
    let refused = ChannelObservation::measure(&same, CHANNEL_TIMEOUT);
    ensure!(!refused.connected,
        "a channel measurement of the stopped endpoint must not connect");

    let receipt = fixture.receipt(vec![serving, stopped, recovered, reserved], Vec::new());
    let receipt_path = root.join("fixture-receipt.json");
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)?;
    eprintln!("fixture receipt: {}", receipt_path.display());
    fixture.cleanup()?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn network_change_awareness_runs_on_a_genuinely_broken_path() -> Result<()> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!(
            "SKIP network_change_fixture: set SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL and SHENKO_LOCAL_MODEL"
        );
        return Ok(());
    }
    let trials = std::env::var("SHENKO_NETWORK_TRIALS").ok()
        .and_then(|value| value.parse::<usize>().ok()).unwrap_or(2).clamp(1, 4);
    let root = fixture_root("trial");
    std::fs::create_dir_all(&root)?;

    let store = Arc::new(Store::open(root.join("measurement.db"), root.join("cas"))?);
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node));
    for package in shenko_awareness_core::seed_packages()? {
        catalog.install(&package, &root.join("cognition"))?;
    }
    let units = catalog.units()?;
    let revision = units.iter().find(|unit| unit.unit_id == UNIT_ID)
        .with_context(|| format!("seed {UNIT_ID} is not installed"))?.revision.clone();

    let (broker, broker_handle) = Broker::new()
        .map_err(|error| anyhow::anyhow!("local provider from the environment: {error}"))?;
    let broker_task = tokio::spawn(broker.run());
    let mut result = measure(&store, &catalog, &broker_handle, &root, &revision, trials).await;
    let mut cleanup_failures = Vec::new();
    let shutdown = match broker_handle.quiesce_for_handoff().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let error = format!("{error:#}");
            cleanup_failures.push(format!("broker drain incomplete: {error}"));
            serde_json::json!({ "error": error, "remote_termination_claimed": false })
        }
    };
    let shutdown_path = root.join("broker-shutdown.json");
    if let Err(error) = serde_json::to_vec_pretty(&shutdown)
        .context("serialize broker shutdown artifact")
        .and_then(|bytes| std::fs::write(&shutdown_path, bytes)
            .with_context(|| format!("write {}", shutdown_path.display()))) {
        cleanup_failures.push(format!("{error:#}"));
    }
    broker_task.abort();
    match broker_task.await {
        Ok(_) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => cleanup_failures.push(format!("join local broker task: {error}")),
    }
    if !cleanup_failures.is_empty() {
        let diagnostics = format!("network-change trial cleanup failed: {}", cleanup_failures.join("; "));
        result = Err(match result {
            Ok(()) => anyhow::anyhow!(diagnostics),
            Err(error) => error.context(diagnostics),
        });
    }
    result
}

/// The whole measurement: prove the path is broken and the wire is not, run the
/// production trial on that state, then prove the path recovered.
async fn measure(
    store: &Arc<Store>,
    catalog: &Arc<CognitionCatalog>,
    broker: &BrokerHandle,
    root: &Path,
    revision: &str,
    trials: usize,
) -> Result<()> {
    let fixture = OwnedNetworkFixture::open(root.join("fixture"), fixture_config())?;
    let mut observations: Vec<PathObservation> = Vec::new();
    let mut namespace_observations = Vec::new();

    fixture.set_endpoint(EndpointMode::Serving)?;
    let before_fault = fixture.observe_endpoint("before-fault")?;
    ensure!(before_fault.reachable(),
        "the owned endpoint must be reachable before any fault, measured: {:?}",
        before_fault.observation.error_kind);
    observations.push(before_fault);

    // Interface tier, when this host allows it: a real link fault isolates the
    // endpoint's own address, and bringing the link back restores it.
    if fixture.capabilities().interface.admitted() {
        fixture.set_interface(LinkMode::Down)?;
        let down = fixture.observe_endpoint("interface-down")?;
        ensure!(!down.reachable(), "a downed owned link must not measure reachable");
        observations.push(down);
        fixture.set_interface(LinkMode::Up)?;
        let up = fixture.observe_endpoint("interface-up")?;
        ensure!(up.reachable(), "the restored owned link must measure reachable again");
        observations.push(up);
    }

    // Namespace tiers, when admitted: a kernel route lookup and a resolver
    // query inside the fixture's own namespace. Their recovery claims stay
    // inside that namespace, and are recorded exactly as measured.
    if fixture.capabilities().namespace.admitted() {
        namespace_observations.push(fixture.break_namespace_route()?);
        namespace_observations.push(fixture.restore_namespace_route()?);
        if fixture.capabilities().namespace_dns.admitted() {
            namespace_observations.push(fixture.break_namespace_dns()?);
            namespace_observations.push(fixture.restore_namespace_dns()?);
        }
    }

    // The state the trial runs in: the declared target is down.
    fixture.set_endpoint(EndpointMode::Stopped)?;
    let during_fault = fixture.observe_endpoint("during-trial")?;
    ensure!(!during_fault.reachable(), "the trial must run against a genuinely broken target");
    ensure!(during_fault.observation.error_kind.as_deref() == Some("connect_failed"),
        "the trial's target must fail at connect, measured: {:?}",
        during_fault.observation.error_kind);

    let origin = ChannelOrigin::from_binding(broker.binding())?;
    let wire_before = ChannelObservation::measure(&origin, CHANNEL_TIMEOUT);
    ensure!(wire_before.connected,
        "the model wire must be up while the target is down, measured: {:?}",
        wire_before.error);
    ensure!(!fixture.fault_reaches_wire(&origin, FaultClass::EndpointStopped),
        "the sealed model wire {}:{} is the fixture's own path; this trial would not be about a different path",
        origin.host, origin.port);

    let target = fixture.declared_url();
    let mut cases = Vec::new();
    for case in CASES.iter() {
        ensure!(case.template.matches(TARGET_PLACEHOLDER).count() == 1,
            "case {} must name the measured target exactly once", case.name);
        let message = case.template.replace(TARGET_PLACEHOLDER, &target);
        let message_digest = shenko_types::object::Digest::blake3(message.as_bytes()).0;
        cases.push(serde_json::json!({
            "name": case.name, "unit_id": UNIT_ID, "target": target,
            "message": message, "message_digest": message_digest,
            "rubric": case.rubric,
        }));
    }
    let spec = TrialSpec {
        unit_id: UNIT_ID.into(),
        revision: revision.to_string(),
        trials_per_condition: trials,
        deadline_ms: MAX_TRIAL_DEADLINE_MS,
        scenarios: CASES.iter().map(|case| TrialScenario {
            name: case.name.into(),
            message: case.template.replace(TARGET_PLACEHOLDER, &target),
            rubric: case.rubric.into(),
        }).collect(),
    };
    let report: TrialReport = run_sandbox_trial(Arc::clone(store), Arc::clone(catalog),
        broker.clone(), root.join("trial"), &spec).await?;

    // The trial is over. The target must still have been down the whole time it
    // ran, and the wire must still be up: a fault that healed mid-run would make
    // every sample ambiguous.
    let after_trial = fixture.observe_endpoint("after-trial")?;
    ensure!(!after_trial.reachable(),
        "the target healed during the trial; the samples cannot be read as a broken-path comparison");
    observations.push(during_fault);
    observations.push(after_trial);
    let wire_after = ChannelObservation::measure(&origin, CHANNEL_TIMEOUT);
    ensure!(wire_after.connected,
        "the model wire went down during the trial, measured: {:?}", wire_after.error);

    fixture.set_endpoint(EndpointMode::Serving)?;
    let recovered = fixture.observe_endpoint("after-recovery")?;
    ensure!(recovered.reachable(), "the owned endpoint must recover after the trial");
    ensure!(recovered.target == target, "recovery must restore the same declared target");
    observations.push(recovered);

    let fixture_receipt = fixture.receipt(observations, namespace_observations);
    let sealed = OwnedNetworkFixture::seal(store, &fixture_receipt)?;
    let receipt_path = root.join("fixture-receipt.json");
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&fixture_receipt)?)?;

    for scenario in &report.scenarios {
        ensure!(scenario.candidate.scored > 0 && scenario.control.scored > 0,
            "{}: the comparison scored no attempt on one side (candidate {} of {}, control {} of {}); \
             this is an unmeasured comparison, not a negative result",
            scenario.name, scenario.candidate.scored, scenario.candidate.attempted,
            scenario.control.scored, scenario.control.attempted);
    }

    let capabilities = fixture_receipt.capabilities.clone();
    let model = report.model.clone();
    let scenarios = report.scenarios.len();
    let verdict = report.verdict.clone();
    let payload = serde_json::json!({
        "schema_version": 1,
        "unit_id": UNIT_ID,
        "revision": revision,
        "trials_per_condition": trials,
        "fixture": {
            "receipt": fixture_receipt,
            "receipt_object": sealed,
            "receipt_path": receipt_path,
            "capabilities": capabilities,
        },
        "wire": {
            "origin": origin,
            "before": wire_before,
            "after": wire_after,
            "structural_distinctness": {
                "fault": "endpoint_stopped",
                "reaches_wire": false,
            },
        },
        "cases": cases,
        "trial": report,
        "limits": [
            "the fixture receipt is a mechanism observation of single attempts; it is not content, not an attitude, and carries no confidence",
            "awareness is a capability; the awareness text plus a watcher notice is the current harness, and this trial measures harness behaviour only",
            "in this sandbox the subject's only installed native handler is cognition.unit.read, so the subject has no reachability instrument; channel/path reasoning is not evidence that the subject reached the network",
            "the model wire was measured by one TCP connect before and after the trial; success then does not prove continuity at any other moment",
        ],
        "measurement_only": true,
    });
    let report_path = root.join("network-change-report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&payload)?)?;
    eprintln!("network-change report: {}", report_path.display());
    eprintln!("model: {model} verdict: {verdict} over {scenarios} scenarios");
    Ok(())
}
