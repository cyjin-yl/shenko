//! Native consumer for the exact-version half of the perception runtime.
//!
//! docs/specs/cognition-units.md#spec-45-1, docs/specs/cognition-units.md#spec-45-2, docs/specs/cognition-units.md#spec-45-4, docs/specs/cognition-units.md#spec-45-10-1: a perception unit is a definition, not a
//! fact. What this node knows is whatever a source bound to one exact
//! installed revision actually observed; the observation is archived against
//! that revision and outlives it. Nothing here is scored from source text:
//! every case below drives the real `Node` installation authority, the real
//! `PerceptionRegistry`, the real `InstallationFactsSource` and the real
//! `UnitRequest::RegisterObservation` archive, and reports the identities the
//! calls produced.
//!
//! The reopen case is a genuinely separate process: this test binary
//! re-executes its own `reopen_probe` test with the node directory, the
//! persisted node identity, expected binding and archived observation digest,
//! so the process-local binding of the parent is provably absent in the child.
//!
//! Everything here is source-only until the parent runs it; no case claims
//! model or product acceptance beyond the facts it reads back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use shenko_awareness_core::{
    CognitionCatalog, INSTALLATION_UNITS, INSTALLATION_UNITS_SOURCE, INSTALLATION_UNITS_TARGET,
    PerceptionBinding, PerceptionFact, PerceptionObserver, PerceptionRegistry, PerceptionStatus,
    PerceptionPerspective, bind_builtin_perception_sources, cognition_package_root,
    parse_cognition_entry, reconcile_builtin_perception_sources, seed_packages,
};
use shenko_node::{Node, Request as NodeRequest, UnitRequest};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

const CUSTOM_UNIT: &str = "perception.consumer.fixture";
const CUSTOM_SOURCE: &str = "consumer:fixture-file";
const MAX_FIXTURE_BYTES: u64 = 4096;

struct Sandbox {
    dir: PathBuf,
    node: Arc<Mutex<Node>>,
    catalog: Arc<CognitionCatalog>,
}

impl Sandbox {
    fn open(label: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("shenko-perception-consumer-{label}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir)?;
        let mut node = Node::open(&dir.join("node"))?;
        node.handle(NodeRequest::Initialize {
            node_id: EntityId::new(),
            identity: None,
        })?;
        let node = Arc::new(Mutex::new(node));
        let catalog = Arc::new(CognitionCatalog::new(Arc::clone(&node)));
        let root = cognition_package_root(&dir);
        let installed: Vec<String> = catalog.units()?.into_iter().map(|unit| unit.unit_id).collect();
        for package in seed_packages()? {
            if installed.contains(&package.unit_id) {
                continue;
            }
            catalog.install(&package, &root)?;
        }
        Ok(Self { dir, node, catalog })
    }

    fn package_root(&self) -> PathBuf {
        cognition_package_root(&self.dir)
    }

    /// The exact installed identity of one unit version, as the authority
    /// reports it.
    fn fact(&self, unit_id: &str) -> Result<shenko_node::InstalledUnitFact> {
        self.node
            .lock()
            .installed_unit_fact(unit_id)?
            .with_context(|| format!("unit '{unit_id}' is not installed"))
    }

    fn binding(&self, unit_id: &str) -> Result<PerceptionBinding> {
        Ok(PerceptionBinding::of(&self.fact(unit_id)?))
    }

    fn card_perception(&self, unit_id: &str) -> Result<Option<PerceptionStatus>> {
        Ok(self
            .catalog
            .cards()?
            .into_iter()
            .find(|card| card.unit_id == unit_id)
            .with_context(|| format!("unit '{unit_id}' has no card"))?
            .perception)
    }

    /// The archived observation bytes the node holds for one digest, checked
    /// against the digest the observation was registered under.
    fn archived_bytes(&self, digest: &str) -> Result<Vec<u8>> {
        let bytes = self
            .node
            .lock()
            .unit_object_bytes(&Digest(digest.to_string()))?;
        ensure!(
            Digest::blake3(&bytes).0 == digest,
            "archived observation bytes do not hash to their registered digest"
        );
        Ok(bytes)
    }

    fn installed_observations(&self, unit_id: &str) -> Result<Vec<shenko_awareness_core::CognitionObservation>> {
        self.catalog.observations(unit_id)
    }
}

/// A real, bounded local source: it reports bytes of one fixture file it
/// actually read, under the exact installed binding it was bound to. It is
/// not a canned fact — removing the file makes it genuinely unavailable.
struct FileFactSource {
    binding: PerceptionBinding,
    path: PathBuf,
    /// Declared shelf life relative to the observation, in seconds.
    validity_secs: Option<i64>,
}

impl FileFactSource {
    fn new(binding: PerceptionBinding, path: PathBuf, validity_secs: Option<i64>) -> Self {
        Self { binding, path, validity_secs }
    }
}

impl PerceptionObserver for FileFactSource {
    fn observe(&self) -> Result<PerceptionFact> {
        let metadata = std::fs::symlink_metadata(&self.path)
            .with_context(|| format!("fixture source cannot stat {}", self.path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "fixture source observes one regular file"
        );
        ensure!(
            metadata.len() <= MAX_FIXTURE_BYTES,
            "fixture source read exceeds its declared bound"
        );
        let bytes = std::fs::read(&self.path)
            .with_context(|| format!("fixture source cannot read {}", self.path.display()))?;
        let observed_at = Utc::now();
        let valid_until = self.validity_secs.map(|secs| {
            (observed_at + chrono::Duration::seconds(secs))
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        });
        Ok(PerceptionFact {
            binding: self.binding.clone(),
            source: CUSTOM_SOURCE.to_string(),
            // Name the exact file independently of the sandbox root's length.
            // Hash the path, not its contents: changed bytes are still the same target.
            target: format!("file.blake3:{}", Digest::blake3(self.path.as_os_str().as_encoded_bytes()).0),
            observed_at: observed_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            valid_until,
            summary: format!(
                "固定观测对象 {} 返回 {} 字节，工件摘要 {}",
                self.path.file_name().and_then(|name| name.to_str()).unwrap_or("(unnamed)"),
                bytes.len(),
                Digest::blake3(&bytes).0
            ),
            evidence_digest: Some(Digest::blake3(&bytes).0),
            // This consumer observes its own fixture file: the reading is this
            // installation's own observation, and it says so.
            perspective: Some(PerceptionPerspective::Own),
        })
    }
}

fn custom_perception_entry(revision: &str) -> String {
    format!(
        "---\nid: {CUSTOM_UNIT}\nversion: {revision}\nkind: perception\ntitle: 消费者固定观测对象\n\
         capability_card: 消费者夹具读取的一个本机固定文件，用来观察有来源、对象和时间的事实\n\
         why: 一个感知单元只说明观察什么；事实必须由绑定到这个确切版本的来源自己取得。\n\
         cues: [\"需要检查一个具名本机对象的当前字节\"]\n\
         provenance: [\"consumer fixture\"]\n---\n\
         这是一个消费者夹具感知定义：它观察一个具名本机文件的当前字节。\n\
         来源不可读时报告不可用，而不是报告正常；安装或阅读本定义本身不产生任何观察。\n"
    )
}

/// One authored awareness unit, installed only to change the installation
/// state an `InstallationFactsSource` reports.
const STATE_CHANGING_UNIT: &str = "---\nid: awareness.consumer.state-change\nversion: 0.1.0\nkind: awareness\n\
     title: 消费者状态变更单元\ncapability_card: 仅用于改变本机安装计数的夹具单元\n\
     provenance: [\"consumer fixture\"]\n---\n这是一个夹具单元，不携带任何判断规则。\n";

#[test]
fn distinct_installation_observations_keep_their_own_lineage() -> Result<()> {
    let sandbox = Sandbox::open("lineage")?;
    let bound = bind_builtin_perception_sources(Arc::clone(&sandbox.node), sandbox.catalog.perception())?;
    ensure!(
        bound.len() == 1 && bound[0] == format!("{INSTALLATION_UNITS}@0.1.0"),
        "the shipped perception source binds exactly its installed revision: {bound:?}"
    );
    let binding = sandbox.binding(INSTALLATION_UNITS)?;
    ensure!(binding.unit_id == INSTALLATION_UNITS, "unexpected binding {binding:?}");

    let first = match sandbox.catalog.perception().observe(&binding) {
        PerceptionStatus::Observed { fact } => fact,
        other => bail!("the shipped source did not observe: {}", other.line(Utc::now())),
    };
    let first_rows = sandbox.installed_observations(INSTALLATION_UNITS)?;
    ensure!(first_rows.len() == 1, "one observation after the first read: {first_rows:?}");
    ensure!(
        first_rows[0].bytes_held && first_rows[0].kind == "perception_observation",
        "the archived row is held bytes of the perception kind: {first_rows:?}"
    );
    let first_digest = first.evidence_digest.clone().context("observed fact carries its digest")?;
    ensure!(
        first_rows[0].digest == first_digest,
        "the registered row is the digest the source reported"
    );
    let first_bytes = sandbox.archived_bytes(&first_digest)?;
    let first_archived: PerceptionFact = serde_json::from_slice(&first_bytes)?;
    ensure!(
        first_archived.binding == binding
            && first_archived.source == INSTALLATION_UNITS_SOURCE
            && first_archived.target == INSTALLATION_UNITS_TARGET,
        "the archived bytes name this exact binding, source and target"
    );
    chrono::DateTime::parse_from_rfc3339(&first_archived.observed_at)?;
    ensure!(
        first_archived.evidence_digest.is_none(),
        "the archived observation is the fact itself, without its own evidence reference"
    );

    // The same unchanged installation read again reports the record it already
    // archived: a second observation time for identical content would be a
    // summary, not an independent observation.
    let repeated = match sandbox.catalog.perception().observe(&binding) {
        PerceptionStatus::Observed { fact } => fact,
        other => bail!("the repeated read did not observe: {}", other.line(Utc::now())),
    };
    ensure!(
        repeated.evidence_digest.as_deref() == Some(first_digest.as_str())
            && repeated.observed_at == first.observed_at,
        "an unchanged reading is the same archived observation"
    );
    ensure!(
        sandbox.installed_observations(INSTALLATION_UNITS)?.len() == 1,
        "an unchanged reading mints no second archived row"
    );

    // A real installation change is a different fact: it must carry its own
    // source, target, time and digest, and the earlier bytes must stay intact.
    let package = parse_cognition_entry(STATE_CHANGING_UNIT, Path::new("consumer/STATE_AWARENESS.md"))?;
    sandbox.catalog.install(&package, &sandbox.package_root())?;
    let second = match sandbox.catalog.perception().observe(&binding) {
        PerceptionStatus::Observed { fact } => fact,
        other => bail!("the changed installation did not observe: {}", other.line(Utc::now())),
    };
    let second_digest = second.evidence_digest.clone().context("changed fact carries its digest")?;
    ensure!(second_digest != first_digest, "a distinct fact has its own digest");
    let second_bytes = sandbox.archived_bytes(&second_digest)?;
    ensure!(second_bytes != first_bytes, "the distinct observation is not a copy");
    let second_archived: PerceptionFact = serde_json::from_slice(&second_bytes)?;
    ensure!(
        second_archived.binding == first_archived.binding
            && second_archived.source == first_archived.source
            && second_archived.target == first_archived.target,
        "both observations speak for the same binding, source and target"
    );
    ensure!(
        second_archived.observed_at >= first_archived.observed_at,
        "the later fact is not backdated"
    );
    ensure!(
        sandbox.archived_bytes(&first_digest)? == first_bytes,
        "the earlier archived observation is retained byte for byte"
    );
    let rows = sandbox.installed_observations(INSTALLATION_UNITS)?;
    ensure!(rows.len() == 2, "two distinct observations are two rows: {rows:?}");
    ensure!(
        rows.iter().all(|row| row.bytes_held)
            && rows.iter().any(|row| row.digest == first_digest)
            && rows.iter().any(|row| row.digest == second_digest),
        "both distinct observations remain held: {rows:?}"
    );

    let receipt = serde_json::json!({
        "schema_version": 1,
        "consumer": "perception-version-lineage",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "binding": binding,
        "catalog_revision": sandbox.catalog.catalog_revision()?,
        "observations": [
            {"digest": first_digest, "observed_at": first_archived.observed_at, "bytes": first_bytes.len()},
            {"digest": second_digest, "observed_at": second_archived.observed_at, "bytes": second_bytes.len()},
        ],
        "limits": {"max_fixture_bytes": MAX_FIXTURE_BYTES, "summary_bytes": shenko_awareness_core::MAX_SUMMARY_BYTES},
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

#[test]
fn a_custom_perception_unit_is_unknown_until_a_source_observes() -> Result<()> {
    let sandbox = Sandbox::open("custom")?;
    let package = parse_cognition_entry(
        &custom_perception_entry("0.1.0"),
        Path::new("consumer/FIXTURE_PERCEPTION.md"),
    )?;
    sandbox.catalog.install(&package, &sandbox.package_root())?;
    let binding = sandbox.binding(CUSTOM_UNIT)?;

    // Installed and readable is not observed: with no bound source the card
    // says unknown, and the index line says so instead of reading as healthy.
    let unknown = sandbox.card_perception(CUSTOM_UNIT)?.context("perception card")?;
    ensure!(unknown.state() == "unknown", "an unbound unit is unknown: {unknown:?}");
    let index = sandbox.catalog.render_index(Some(
        shenko_awareness_core::CognitionCall::Tool { read: true, list: true },
    ))?;
    ensure!(
        index.lines().any(|line| line.starts_with(&format!("- {CUSTOM_UNIT}@0.1.0 "))
            && line_contains(&index, "未知")),
        "the disclosure reports the unknown state honestly: {index}"
    );

    // A real source bound to the exact revision observes real bytes.
    let fixture = sandbox.dir.join("observed-object.bin");
    std::fs::write(&fixture, b"consumer fixture bytes observed under the exact revision")?;
    sandbox
        .catalog
        .perception()
        .bind(binding.clone(), Arc::new(FileFactSource::new(binding.clone(), fixture.clone(), None)))?;
    let observed = sandbox.card_perception(CUSTOM_UNIT)?.context("perception card")?;
    let PerceptionStatus::Observed { fact } = &observed else {
        bail!("a bound source that read the file must observe: {}", observed.line(Utc::now()));
    };
    ensure!(
        fact.binding == binding
            && fact.source == CUSTOM_SOURCE
            && fact.target == format!("file.blake3:{}", Digest::blake3(fixture.as_os_str().as_encoded_bytes()).0),
        "the observation names its own source, target and binding: {fact:?}"
    );
    let expected_digest =
        Digest::blake3(b"consumer fixture bytes observed under the exact revision").0;
    ensure!(
        fact.evidence_digest.as_deref() == Some(expected_digest.as_str()),
        "the reported evidence digest is the digest of the bytes the source read"
    );

    // The observed object genuinely disappears: the honest answer is
    // unavailable, not the previous observation kept as if still current.
    std::fs::remove_file(&fixture)?;
    let unavailable = sandbox.catalog.perception().observe(&binding);
    ensure!(
        matches!(unavailable, PerceptionStatus::Unavailable { .. }),
        "a source whose object disappeared is unavailable: {}",
        unavailable.line(Utc::now())
    );
    let card = sandbox.card_perception(CUSTOM_UNIT)?.context("perception card")?;
    ensure!(
        card.state() == "unavailable" && line_contains(&card.line(Utc::now()), "不可用"),
        "the card discloses the unavailability: {}",
        card.line(Utc::now())
    );

    let receipt = serde_json::json!({
        "schema_version": 1,
        "consumer": "perception-custom-unit-states",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "binding": binding,
        "observed": {"source": CUSTOM_SOURCE, "evidence_digest": fact.evidence_digest,
            "observed_at": fact.observed_at, "target": fact.target},
        "unavailable_line": card.line(Utc::now()),
        "archived_rows": sandbox.installed_observations(CUSTOM_UNIT)?,
        "limits": {"max_fixture_bytes": MAX_FIXTURE_BYTES},
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

#[test]
fn an_expired_observation_is_never_upgraded_to_current() -> Result<()> {
    let sandbox = Sandbox::open("expiry")?;
    let package = parse_cognition_entry(
        &custom_perception_entry("0.1.0"),
        Path::new("consumer/FIXTURE_PERCEPTION.md"),
    )?;
    sandbox.catalog.install(&package, &sandbox.package_root())?;
    let binding = sandbox.binding(CUSTOM_UNIT)?;
    let fixture = sandbox.dir.join("expiring-object.bin");
    std::fs::write(&fixture, b"expiring observation")?;
    // The source declares a shelf life that has already elapsed: the
    // projection must show the expiry instead of presenting the fact as
    // current.
    sandbox.catalog.perception().bind(
        binding.clone(),
        Arc::new(FileFactSource::new(binding.clone(), fixture.clone(), Some(-3600))),
    )?;
    let status = sandbox.catalog.perception().observe(&binding);
    let PerceptionStatus::Observed { fact } = &status else {
        bail!("the source observed before its declared shelf life: {}", status.line(Utc::now()));
    };
    let valid_until = fact.valid_until.clone().context("the source declares its validity end")?;
    ensure!(
        chrono::DateTime::parse_from_rfc3339(&valid_until)?.with_timezone(&Utc) < Utc::now(),
        "the declared validity end is genuinely in the past"
    );
    let card = sandbox.card_perception(CUSTOM_UNIT)?.context("perception card")?;
    let line = card.line(Utc::now());
    ensure!(
        line_contains(&line, "有效期已过"),
        "an expired observation is disclosed as expired, not as current: {line}"
    );
    ensure!(
        !line_contains(&line, "有效期至"),
        "an elapsed validity end is not rendered as still running: {line}"
    );

    let receipt = serde_json::json!({
        "schema_version": 1,
        "consumer": "perception-expiry",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "binding": binding,
        "observed_at": fact.observed_at,
        "valid_until": valid_until,
        "line": line,
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

#[test]
fn an_updated_installation_units_version_retires_the_old_source() -> Result<()> {
    let sandbox = Sandbox::open("retire")?;
    let bound = bind_builtin_perception_sources(Arc::clone(&sandbox.node), sandbox.catalog.perception())?;
    ensure!(bound == vec![format!("{INSTALLATION_UNITS}@0.1.0")], "seed revision bound: {bound:?}");
    let old = sandbox.binding(INSTALLATION_UNITS)?;
    let old_status = sandbox.catalog.perception().observe(&old);
    ensure!(
        matches!(old_status, PerceptionStatus::Observed { .. }),
        "the shipped source observed the seed revision: {}",
        old_status.line(Utc::now())
    );
    let observation_before = sandbox.installed_observations(INSTALLATION_UNITS)?;

    // A new authored version of the same unit is installed through the real
    // import path; the declared supersession names the version it replaces.
    let entry = format!(
        "---\nid: {INSTALLATION_UNITS}\nversion: 0.2.0\nkind: perception\ntitle: 本机安装现状（v0.2.0）\n\
         capability_card: 本机实际安装了哪些认知单元，哪些已停用、仍是候选或必需依赖未解析\n\
         why: 安装状态是本机事实，定义更新后旧版本的观察不能回答新版本。\n\
         cues: [\"准备凭印象断言某项能力可用或不可用\"]\n\
         supersedes: [0.1.0@{}]\nprovenance: [SPEC §45.1]\n---\n\
         这是本安装单元定义的 0.2.0 版本：它定义了同样的观察对象，但它是另一个确切版本，\n\
         需要由绑定到这个版本的来源重新观察。\n",
        old.artifact_digest
    );
    let package = parse_cognition_entry(&entry, Path::new("consumer/INSTALLATION_UNITS.md"))?;
    sandbox.catalog.install(&package, &sandbox.package_root())?;
    let new = sandbox.binding(INSTALLATION_UNITS)?;
    ensure!(
        new.revision == "0.2.0" && new.artifact_digest != old.artifact_digest,
        "the installed revision moved to the authored version: {new:?}"
    );

    // Projecting the installation re-derives the sources from the installed
    // facts: the registration that spoke for 0.1.0 is retired, its cached
    // observation goes with it, and the new revision is bound and observed.
    let card = sandbox.card_perception(INSTALLATION_UNITS)?.context("perception card")?;
    ensure!(
        sandbox.catalog.perception().is_bound(&new),
        "the shipped source is bound to the installed revision"
    );
    ensure!(
        !sandbox.catalog.perception().is_bound(&old),
        "the previous revision's registration is retired"
    );
    ensure!(
        matches!(
            sandbox.catalog.perception().cached(&old),
            PerceptionStatus::Unknown { .. }
        ),
        "a retired registration keeps no observation that could answer for the new version"
    );
    let PerceptionStatus::Observed { fact } = &card else {
        bail!("the newly installed revision observed: {}", card.line(Utc::now()));
    };
    ensure!(
        fact.binding == new,
        "the observation belongs to the newly installed revision, not the retired one"
    );

    // The 0.1.0 observation is retained as a fact about 0.1.0 (docs/specs/cognition-units.md#spec-45-10-1)
    // and still names its own binding.
    let read = sandbox.catalog.read(INSTALLATION_UNITS, "0.1.0")?;
    ensure!(
        read.revision == "0.1.0" && !read.current && read.artifact_digest == old.artifact_digest,
        "the replaced version stays readable as retained history: {read:?}"
    );
    ensure!(
        !observation_before.is_empty() && observation_before.iter().all(|row| row.bytes_held),
        "the pre-update observations are held bytes: {observation_before:?}"
    );
    for row in &observation_before {
        let bytes = sandbox.archived_bytes(&row.digest)?;
        let archived: PerceptionFact = serde_json::from_slice(&bytes)?;
        ensure!(
            archived.binding == old,
            "an archived observation keeps the exact version it was taken under"
        );
    }
    let lineage = sandbox
        .node
        .lock()
        .handle_unit_request(UnitRequest::Lineage { unit_id: INSTALLATION_UNITS.to_string() })?;

    let receipt = serde_json::json!({
        "schema_version": 1,
        "consumer": "perception-version-retirement",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "retired": old,
        "installed": new,
        "new_observation": {"digest": fact.evidence_digest, "observed_at": fact.observed_at},
        "retained_observations": observation_before,
        "lineage": lineage,
        "catalog_revision": sandbox.catalog.catalog_revision()?,
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

/// A separate-process reopen: the parent test re-executes this binary with
/// exactly this test name and the node directory in the environment. Absent
/// that environment the case is inert, so a plain `cargo test` run reports it
/// as passed without claiming anything.
#[test]
fn reopen_probe() -> Result<()> {
    let Some(dir) = std::env::var_os("SHENKO_PERCEPTION_REOPEN_NODE_DIR") else {
        return Ok(());
    };
    let receipt_path = std::env::var("SHENKO_PERCEPTION_REOPEN_RECEIPT")
        .context("the reopen probe is given a receipt path")?;
    let expected_node_id: EntityId = serde_json::from_str(
        &std::env::var("SHENKO_PERCEPTION_REOPEN_NODE_ID")
            .context("the reopen probe is given the installation's stable node identity")?,
    )?;
    let expected_binding: PerceptionBinding = serde_json::from_str(
        &std::env::var("SHENKO_PERCEPTION_REOPEN_BINDING").context("the reopen probe is given a binding")?,
    )?;
    let expected_digest =
        std::env::var("SHENKO_PERCEPTION_REOPEN_DIGEST").context("the reopen probe is given a digest")?;

    // Read-only inspection first: this process has no live registration of
    // its own, so the persisted history must not read as a healthy fact.
    let inspect = Node::open_inspect(Path::new(&dir))?.context("the node installation exists")?;
    ensure!(
        inspect.local_node_id()? == Some(expected_node_id),
        "the separate process reopened the same persistent node identity"
    );
    let bytes = inspect.unit_object_bytes(&Digest(expected_digest.clone()))?;
    ensure!(
        Digest::blake3(&bytes).0 == expected_digest,
        "the reopened process reads the retained observation at its registered digest"
    );
    let archived: PerceptionFact = serde_json::from_slice(&bytes)?;
    ensure!(
        archived.binding == expected_binding,
        "the retained observation still names the exact revision it was taken under"
    );
    drop(inspect);

    let registry = PerceptionRegistry::new();
    let before = registry.cached(&expected_binding);
    ensure!(before.state() == "unknown", "a fresh process is unbound: {before:?}");
    let before_observe = registry.observe(&expected_binding);
    ensure!(
        matches!(before_observe, PerceptionStatus::Unknown { .. }),
        "an unbound revision stays unknown until legitimately rebound: {}",
        before_observe.line(Utc::now())
    );

    // The legitimate rebind: the installed facts of THIS process are read and
    // the shipped source is bound again, producing its own observation.
    // A new process does not create a new node identity.
    let mut node = Node::open(Path::new(&dir))?;
    node.handle(NodeRequest::Initialize {
        node_id: expected_node_id,
        identity: None,
    })?;
    let node = Arc::new(Mutex::new(node));
    let rebound = reconcile_builtin_perception_sources(&node, &registry)?;
    ensure!(
        rebound.contains(&expected_binding),
        "the installed revision is the one rebound: {rebound:?}"
    );
    let after = registry.observe(&expected_binding);
    let PerceptionStatus::Observed { fact } = &after else {
        bail!("the rebound source observed: {}", after.line(Utc::now()));
    };
    let after_digest = fact.evidence_digest.clone().context("the rebound observation carries its digest")?;
    ensure!(
        after_digest != expected_digest,
        "the rebound observation is this process's own record, not the retained one reused"
    );

    let receipt = serde_json::json!({
        "schema_version": 1,
        "consumer": "perception-reopen-probe",
        "process_id": std::process::id(),
        "node_dir": dir,
        "node_id": node.lock().local_node_id()?,
        "retained": {"digest": expected_digest, "bytes": bytes.len(), "binding": archived.binding,
            "observed_at": archived.observed_at},
        "unbound": {"cached_state": before.state(), "observe_state": before_observe.state()},
        "rebound": {"binding": fact.binding, "digest": after_digest, "observed_at": fact.observed_at},
    });
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)?;
    println!("RECEIPT:{}", receipt_path);
    Ok(())
}

#[test]
fn a_fresh_process_is_unbound_until_it_rebinds() -> Result<()> {
    let sandbox = Sandbox::open("reopen")?;
    let node_id = sandbox.node.lock().local_node_id()?.context("the parent node is initialized")?;
    let bound = bind_builtin_perception_sources(Arc::clone(&sandbox.node), sandbox.catalog.perception())?;
    ensure!(bound == vec![format!("{INSTALLATION_UNITS}@0.1.0")], "seed revision bound: {bound:?}");
    let binding = sandbox.binding(INSTALLATION_UNITS)?;
    let status = sandbox.catalog.perception().observe(&binding);
    let PerceptionStatus::Observed { fact } = &status else {
        bail!("the parent process observed: {}", status.line(Utc::now()));
    };
    let digest = fact.evidence_digest.clone().context("the observation carries its digest")?;
    ensure!(
        !sandbox.installed_observations(INSTALLATION_UNITS)?.is_empty(),
        "the observation is archived before the reopen"
    );

    // Change a real installation fact without changing this perception unit's
    // exact binding. Observation times have second resolution, so a reopen
    // alone cannot guarantee distinct bytes when the installation is unchanged.
    // The existing fixture unit makes the child's new observation distinct
    // even when both processes observe within the same second.
    let package = parse_cognition_entry(STATE_CHANGING_UNIT, Path::new("consumer/STATE_AWARENESS.md"))?;
    sandbox.catalog.install(&package, &sandbox.package_root())?;

    // The same installation identity, observed by a separate process. Its
    // receipt is the child's own reading of the retained bytes and of its own
    // binding state; nothing here is inferred from the parent's memory.
    let receipt_path = sandbox.dir.join("reopen-receipt.json");
    let binary = std::env::current_exe().context("the test binary path")?;
    let output = std::process::Command::new(&binary)
        .args(["--exact", "reopen_probe", "--nocapture", "--test-threads=1"])
        .env("SHENKO_PERCEPTION_REOPEN_NODE_DIR", sandbox.dir.join("node"))
        .env("SHENKO_PERCEPTION_REOPEN_NODE_ID", serde_json::to_string(&node_id)?)
        .env("SHENKO_PERCEPTION_REOPEN_RECEIPT", &receipt_path)
        .env("SHENKO_PERCEPTION_REOPEN_BINDING", serde_json::to_string(&binding)?)
        .env("SHENKO_PERCEPTION_REOPEN_DIGEST", &digest)
        .output()
        .context("re-execute the consumer binary as a separate process")?;
    ensure!(
        output.status.success(),
        "the reopen probe process failed: status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the reopen probe case ran in the child process: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let child: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt_path).context("the probe wrote its receipt")?)?;
    ensure!(
        child["consumer"] == "perception-reopen-probe"
            && child["node_id"] == serde_json::to_value(node_id)?
            && child["retained"]["digest"] == digest.as_str()
            && child["retained"]["binding"] == serde_json::to_value(&binding)?,
        "the separate process read the retained observation of this exact binding: {child}"
    );
    ensure!(
        child["unbound"]["cached_state"] == "unknown"
            && child["unbound"]["observe_state"] == "unknown",
        "the separate process was unbound until it rebound: {child}"
    );
    ensure!(
        child["rebound"]["digest"] != digest.as_str(),
        "the separate process archived its own observation on rebind: {child}"
    );
    // A reopen in this parent is the same fact: the durable history is not a
    // live binding.
    let reopened = PerceptionRegistry::new();
    ensure!(
        reopened.cached(&binding).state() == "unknown"
            && matches!(reopened.observe(&binding), PerceptionStatus::Unknown { .. }),
        "the parent's own fresh registry is unbound as well"
    );
    let rows_after = sandbox.installed_observations(INSTALLATION_UNITS)?;
    ensure!(
        rows_after.iter().any(|row| row.digest == digest)
            && rows_after
                .iter()
                .any(|row| Some(row.digest.as_str()) == child["rebound"]["digest"].as_str()),
        "both the parent's and the separate process's observations are durable: {rows_after:?}"
    );

    let receipt = serde_json::json!({
        "schema_version": 1,
        "consumer": "perception-separate-process-reopen",
        "parent_process_id": std::process::id(),
        "child": child,
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "node_id": node_id,
        "retained_digest": digest,
        "binding": binding,
        "catalog_revision": sandbox.catalog.catalog_revision()?,
        "observations_after": rows_after,
        "limits": {"probe_timeout": "parent stdout must show '1 passed'; no sleep-based wait"},
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

fn line_contains(text: &str, needle: &str) -> bool {
    text.lines().any(|line| line.contains(needle))
}
