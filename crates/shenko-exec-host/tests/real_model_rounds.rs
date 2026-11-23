//! Repeated real-model turns in one real Shenko sandbox (R077, V1-11/V1-12/V1-14).
//!
//! The consumer never builds a broker, subject loop or provider of its own. It
//! starts the real production sandbox with the configured OpenAI-compatible
//! endpoint, injects N fixed turns into the same Store and subject branch,
//! and retains every sealed input, raw model answer, committed decision and
//! parser result below the run directory.
//!
//! The endpoint comes from the environment; the harness records the model and
//! the binding digest, never the private address:
//!
//! ```sh
//! SHENKO_USE_LOCAL_MODEL=1 \
//! SHENKO_LOCAL_MODEL_URL=http://127.0.0.1:8000 \
//! SHENKO_LOCAL_MODEL=swift-qwen3.8-27b-nvfp4 \
//! SHENKO_SOURCE_REVISION=$(git rev-parse HEAD) \
//! SHENKO_REAL_MODEL_ROUNDS=4 \
//! cargo test -p shenko-exec-host --test real_model_rounds -- --nocapture
//! ```
//!
//! A run without a source revision or with an absent bound source file is
//! retained as an `unbound` observation and fails the completion assertion.
//! That is deliberate: an unbound model run is not acceptance evidence.

use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde_json::json;
use sha2::{Digest as Sha2Digest, Sha256};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::broker::Broker;
use shenko_exec_host::{
    CandidateBinding, CognitionReadHandler, PreparedSandbox, RoundInputs,
    RoundObservationStatus, SandboxRoundSpec, SandboxRuntime, run_sandbox_rounds,
};
use shenko_identity::{DefaultIdentityPolicy, IdentityService};
use shenko_kernel::Supervisor;
use shenko_kernel::task::NativeTaskHandler;
use shenko_node::Node;
use shenko_subject::{NativeTaskHandlerPlugin, canonical_system_prompt};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest as ContentDigest;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The source files this consumer's claims depend on. Every one is hashed at
/// the declared candidate revision; a missing file stays visible as `absent`.
const BOUND_SOURCES: &[&str] = &[
    "crates/shenko-broker/src/broker.rs",
    "crates/shenko-broker/src/local_provider.rs",
    "crates/shenko-broker/src/request.rs",
    "crates/shenko-subject/src/context_projection.rs",
    "crates/shenko-subject/src/loop_runner.rs",
    "crates/shenko-subject/src/output.rs",
    "crates/shenko-exec-host/src/cognition_trial.rs",
    "crates/shenko-exec-host/src/cognition_trial/rounds.rs",
    "docs/specs/verification.md",
    "docs/specs/nodes.md",
    "docs/development/local-model-behavior.md",
];

/// The fixed turn table. `None` observes the subject's next idle continuation;
/// it does not inject a synthetic prompt. The table is declared before the run
/// and is never changed in response to model output.
const FIXED_ROUNDS: [(&str, Option<&str>); 16] = [
    (
        "greet_and_state_fact",
        Some("你好。请记住这句话：我的生日是 3 月 14 日。只回复一句确认。"),
    ),
    ("recall_birthday", Some("我刚才让你记住的生日是哪一天？只回答日期。")),
    ("idle_after_recall", None),
    ("recap_birthday", Some("用一句话复述刚才关于生日的对话。")),
    ("state_city", Some("再记住一件事：我住在杭州。只回复一句确认。")),
    ("recall_city", Some("我住在哪里？只回答城市。")),
    ("idle_after_city", None),
    ("suggest_activity", Some("根据你了解到的这些，简短说说今天适合做什么。")),
    ("recall_both", Some("把我请你记住的两件事一起复述。")),
    ("idle_after_both", None),
    ("combined_fact", Some("我的生日和常住城市分别是什么？一句话回答。")),
    ("arithmetic", Some("只回答：1 加 1 等于几？")),
    ("idle_after_arithmetic", None),
    ("recall_arithmetic", Some("你刚才回答的算术题是什么？一句话。")),
    ("idle_after_recall_arithmetic", None),
    ("closing_summary", Some("最后，用一句话总结这次对话。")),
];

/// Install the production cognition reader so the model can discover the
/// seeded units through its real native capability surface.
struct RealModelRoundRuntime;

impl SandboxRuntime for RealModelRoundRuntime {
    async fn install(
        &self,
        supervisor: &Supervisor,
        sandbox: &PreparedSandbox,
        catalog: &Arc<CognitionCatalog>,
    ) -> Result<()> {
        let reader: Arc<dyn NativeTaskHandler> = Arc::new(CognitionReadHandler::new(
            Arc::clone(sandbox.store()),
            Arc::clone(catalog),
        ));
        supervisor.spawn(NativeTaskHandlerPlugin, reader).await?;
        Ok(())
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SHA-256 and byte length of one file, read without loading the whole
/// executable into memory.
fn digest_file(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open candidate executable {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((hex(hasher.finalize()), bytes))
}

fn declared_prompt_digest() -> String {
    let identity = IdentityService::new(Arc::new(DefaultIdentityPolicy));
    // The throwaway sandbox publishes the default identity service without a
    // chosen profile, so the declared component is the unnamed persona.
    let profile = DefaultIdentityPolicy::genesis(EntityId::nil(), None);
    let mut prompt = canonical_system_prompt(&shenko_channel_napcat::qq_test_registry());
    prompt.push_str("\n\n");
    prompt.push_str(&identity.persona_section(&profile));
    format!("sha256:{}", hex(Sha256::digest(prompt.as_bytes())))
}

fn candidate_binding(model: &str) -> Result<CandidateBinding> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut source_files = BTreeMap::new();
    let mut aggregate = Sha256::new();
    for relative in BOUND_SOURCES {
        let path = root.join(relative);
        let value = match std::fs::read(&path) {
            Ok(bytes) => {
                let digest = hex(Sha256::digest(&bytes));
                aggregate.update(relative.as_bytes());
                aggregate.update([0]);
                aggregate.update(digest.as_bytes());
                aggregate.update(b"\n");
                format!("sha256:{digest}")
            }
            Err(_) => {
                aggregate.update(relative.as_bytes());
                aggregate.update([0]);
                aggregate.update(b"absent\n");
                "absent".to_string()
            }
        };
        source_files.insert((*relative).to_string(), value);
    }
    let executable = std::env::current_exe()?;
    let (executable_sha256, executable_bytes) = digest_file(&executable)?;
    Ok(CandidateBinding {
        source_revision: std::env::var("SHENKO_SOURCE_REVISION").unwrap_or_default(),
        source_digest: format!("sha256:{}", hex(aggregate.finalize())),
        source_files,
        executable_path: executable.display().to_string(),
        executable_sha256,
        executable_bytes,
        declared_prompt_digest: declared_prompt_digest(),
        model: model.to_string(),
    })
}

fn sandbox_root() -> PathBuf {
    std::env::var_os("SHENKO_SANDBOX_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/real-model-rounds")
        })
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_real_model_turns_run_in_the_production_sandbox() -> Result<()> {
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") {
        eprintln!(
            "SKIP real_model_rounds: set SHENKO_USE_LOCAL_MODEL=1, SHENKO_LOCAL_MODEL_URL, \
             SHENKO_LOCAL_MODEL and SHENKO_SOURCE_REVISION to run the real-model rounds"
        );
        return Ok(());
    }
    let endpoint = std::env::var("SHENKO_LOCAL_MODEL_URL")
        .context("SHENKO_LOCAL_MODEL_URL is required for a reported repeated-model run")?;
    ensure!(!endpoint.trim().is_empty(), "SHENKO_LOCAL_MODEL_URL is empty");
    let model = std::env::var("SHENKO_LOCAL_MODEL")
        .context("SHENKO_LOCAL_MODEL is required for a reported repeated-model run")?;
    ensure!(!model.trim().is_empty(), "SHENKO_LOCAL_MODEL is empty");
    let rounds_count: usize = std::env::var("SHENKO_REAL_MODEL_ROUNDS")
        .unwrap_or_else(|_| "4".into())
        .parse()
        .context("SHENKO_REAL_MODEL_ROUNDS must be an integer")?;
    ensure!(
        (1..=FIXED_ROUNDS.len()).contains(&rounds_count),
        "SHENKO_REAL_MODEL_ROUNDS must be 1..={}",
        FIXED_ROUNDS.len()
    );
    let default_seconds = 240_u64 * rounds_count as u64 + 60;
    let timeout_secs: u64 = std::env::var("SHENKO_REAL_MODEL_TIMEOUT_SECS")
        .ok()
        .map(|value| value.parse())
        .transpose()
        .context("SHENKO_REAL_MODEL_TIMEOUT_SECS must be an integer")?
        .unwrap_or(default_seconds);
    ensure!(
        (rounds_count as u64..=7200).contains(&timeout_secs),
        "SHENKO_REAL_MODEL_TIMEOUT_SECS must be at least one second per round and at most 7200"
    );

    let base = sandbox_root();
    std::fs::create_dir_all(&base)?;
    let root = base.join(format!("run-{}", EntityId::new()));
    std::fs::create_dir(&root)?;

    let catalog_node = Arc::new(Mutex::new(Node::open(&root.join("catalog"))?));
    let catalog = Arc::new(CognitionCatalog::new(catalog_node));
    for package in shenko_awareness_core::seed_packages()? {
        catalog.install(&package, &root.join("cognition"))?;
    }

    let prepared = PreparedSandbox::fresh("real-model-rounds", &root)?;
    let artifacts = prepared.installation().join("rounds");
    let candidate = candidate_binding(&model)?;
    let rounds: Vec<SandboxRoundSpec> = FIXED_ROUNDS
        .iter()
        .take(rounds_count)
        .map(|(name, message)| match message {
            Some(message) => SandboxRoundSpec::message(*name, *message),
            None => SandboxRoundSpec::idle(*name),
        })
        .collect();

    let (broker, handle) = Broker::new()?;
    ensure!(
        handle.binding().model() == model.as_str(),
        "the configured provider serves '{}' but the run declares '{}'",
        handle.binding().model(),
        model
    );
    let broker_model = handle.binding().model().to_string();
    let broker_task = tokio::spawn(broker.run());
    let stop = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let outcome = run_sandbox_rounds(
        prepared,
        &handle,
        catalog,
        RoundInputs {
            rounds: &rounds,
            artifacts_root: &artifacts,
        },
        candidate,
        stop,
        &RealModelRoundRuntime,
    )
    .await;

    // The local broker stays alive until its readers and receipt writers have
    // drained. An incomplete drain is retained as an explicit unknown, never
    // rewritten into a clean shutdown.
    let mut broker_failures = Vec::new();
    let shutdown = match tokio::time::timeout(Duration::from_secs(120), handle.quiesce_for_handoff()).await
    {
        Ok(Ok(snapshot)) => serde_json::to_value(snapshot)?,
        Ok(Err(error)) => {
            let error = format!("{error:#}");
            broker_failures.push(format!("broker drain incomplete: {error}"));
            json!({ "error": error, "remote_termination_claimed": false })
        }
        Err(_) => {
            broker_failures.push("broker drain exceeded 120 s".to_string());
            json!({ "error": "broker drain exceeded 120 s", "remote_termination_claimed": false })
        }
    };
    std::fs::create_dir_all(&artifacts)?;
    std::fs::write(
        artifacts.join("broker-shutdown.json"),
        serde_json::to_vec_pretty(&shutdown)?,
    )?;
    broker_task.abort();
    match broker_task.await {
        Ok(_) => {}
        Err(error) if error.is_cancelled() => {}
        Err(error) => broker_failures.push(format!("join local broker task: {error}")),
    }

    let report = match outcome {
        Ok(report) => Some(report),
        Err(error) => {
            eprintln!("real_model_rounds: no report: {error:#}");
            None
        }
    };
    let summary = json!({
        "rounds_requested": rounds_count,
        "timeout_secs": timeout_secs,
        "model": broker_model,
        "report": report.as_ref().map(|_| artifacts.join("report.json").display().to_string()),
        "verdict": report.as_ref().map(|report| report.verdict.clone()),
        "reason": report.as_ref().map(|report| report.reason.clone()),
        "broker_cleanup_failures": &broker_failures,
        "fatal_error": report.is_none().then(|| "the runner returned no report".to_string()),
    });
    std::fs::write(
        artifacts.join("test-summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    let report = report.context("the repeated-model run produced no report")?;
    eprintln!("real-model round report: {}", artifacts.join("report.json").display());

    ensure!(
        broker_failures.is_empty(),
        "the local broker did not drain cleanly: {}",
        broker_failures.join("; ")
    );
    ensure!(
        report.cleanup_failures.is_empty(),
        "sandbox cleanup failed: {}",
        report.cleanup_failures.join("; ")
    );
    ensure!(
        report.planned_rounds == rounds_count && report.rounds.len() == rounds_count,
        "the report does not carry all {} planned rounds",
        rounds_count
    );
    ensure!(
        report.verdict == "complete",
        "the repeated-model run did not complete: {}",
        report.reason
    );
    for observation in &report.candidate_observations {
        ensure!(
            observation.status != RoundObservationStatus::Failed,
            "candidate observation {} failed: {}",
            observation.name,
            observation.detail
        );
    }
    for round in &report.rounds {
        ensure!(
            round.started,
            "planned round {} did not start: {:?}",
            round.name,
            round.failure
        );
        ensure!(
            round.step_id.is_some() && round.output.is_some(),
            "round {} did not produce a committed parsed decision: {:?}",
            round.name,
            round.failure
        );
        for observation in &round.observations {
            ensure!(
                observation.status != RoundObservationStatus::Failed,
                "round {} observation {} failed: {}",
                round.name,
                observation.name,
                observation.detail
            );
        }
    }
    ensure!(
        report.completed_rounds == rounds_count
            && report.failed_rounds == 0
            && report.unstarted_rounds == 0,
        "the report counts do not match a complete run: {}",
        report.reason
    );

    // Every retained artifact is re-read and checked against the report. A
    // correct name or a well-formed digest string is not enough: the file must
    // still contain exactly those bytes.
    let artifacts_root = Path::new(&report.artifacts_root);
    for artifact in &report.artifacts {
        let path = artifacts_root.join(&artifact.path);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read retained artifact {}", path.display()))?;
        ensure!(
            bytes.len() as u64 == artifact.bytes,
            "retained artifact {} has {} bytes, report says {}",
            artifact.path,
            bytes.len(),
            artifact.bytes
        );
        ensure!(
            ContentDigest::blake3(&bytes).0 == artifact.digest,
            "retained artifact {} does not match its report digest",
            artifact.path
        );
    }
    Ok(())
}
