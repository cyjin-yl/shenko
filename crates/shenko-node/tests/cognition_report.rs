//! The installation-to-node cognition report boundary.
//!
//! SPEC §51.1: a node keeps one mechanism observation of the installation's own
//! Store snapshot — the immutable content digest over a declared scope and the
//! ledger watermark it covered. It is not cognitive content, not an attitude,
//! not a knowledge kind and not a confidence; it is also not a liveness,
//! capacity or recoverability claim. Re-observing identical content is the same
//! revision, so `observed_at` is the instant that content was first observed
//! here.
//!
//! Every read below goes through a separate `shenko-node` process, so a report
//! that is visible here survived a real process reopen. What this fixture does
//! not claim: it never opens a Store (the producer is the installation's own
//! process — that chain is covered by the `shenko` binary fixture), and it does
//! not measure the peer's liveness.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shenko_node::{
    CognitionObservation, CognitionScope, CognitionSnapshotReport, Node, PeerExperience, Request,
};
use shenko_types::{ids::EntityId, object::Digest};
use std::io::Write;
use std::path::{Path, PathBuf};

const BIN: &str = env!("CARGO_BIN_EXE_shenko-node");

/// One request to one node process: one process per request.
fn run(dir: &Path, request: &Request) -> Result<Value> {
    let bytes = serde_json::to_vec(request)?;
    let mut child = std::process::Command::new(BIN)
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn node process")?;
    {
        let mut stdin = child.stdin.take().context("node stdin")?;
        stdin.write_all(&bytes)?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "node process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("decode node response")
}

/// One refused request: the process must report the refusal and write nothing.
fn run_refused(dir: &Path, request: &Request) -> Result<String> {
    let bytes = serde_json::to_vec(request)?;
    let mut child = std::process::Command::new(BIN)
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn node process")?;
    {
        let mut stdin = child.stdin.take().context("node stdin")?;
        stdin.write_all(&bytes)?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        !output.status.success(),
        "the node accepted a request it must refuse: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    ensure!(
        output.stdout.is_empty(),
        "a refused request printed a result body"
    );
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

fn private_dir(root: &Path, name: &str) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = root.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// A node installation whose identity was established by its own process.
fn installation(root: &Path, name: &str) -> Result<(PathBuf, EntityId)> {
    let dir = private_dir(root, name)?;
    let id = EntityId::new();
    run(
        &dir,
        &Request::Initialize {
            node_id: id,
            identity: None,
        },
    )?;
    Ok((dir, id))
}

/// The report the node's own authenticated route serves right now.
fn reported(dir: &Path) -> Result<Option<CognitionSnapshotReport>> {
    let status = run(dir, &Request::Status { operation_id: None })?;
    let Some(value) = status.get("cognition").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(value.clone())?))
}

fn instant(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_780_000_000 + seconds, 0)
        .expect("fixture instant")
}

fn scope(workspaces: usize, git_history: bool) -> CognitionScope {
    CognitionScope {
        memory_root: Some(PathBuf::from("/fixture/memory")),
        workspace_roots: (0..workspaces)
            .map(|index| PathBuf::from(format!("/fixture/workspace-{index}")))
            .collect(),
        include_git_history: git_history,
        local_boundary_preserved: false,
    }
}

fn observation(
    content: &str,
    high_water: u64,
    scope: CognitionScope,
    at: i64,
) -> CognitionObservation {
    CognitionObservation {
        fingerprint: Digest::blake3(content.as_bytes()),
        high_water,
        scope,
        observed_at: instant(at),
        provenance: "fixture: this installation's own Store snapshot".into(),
    }
}

fn experience(peer: EntityId, report: Option<&CognitionSnapshotReport>) -> PeerExperience {
    PeerExperience {
        peer_node_id: peer,
        peer_branch_id: None,
        events: vec![],
        receipts: vec![],
        cognition_fingerprint: report.map(|report| report.fingerprint.clone()),
        cognition_high_water: report.map(|report| report.high_water),
        cognition_origin: report.map(CognitionSnapshotReport::origin),
        peer_high_water: None,
        peer_manifest: None,
        note: "fixture: the peer's route reported this cognition observation".into(),
    }
}

/// Publish one observation the way the installation's own process does, then
/// close the handle so every later read is a new process against the file.
fn publish(
    dir: &Path,
    node_id: EntityId,
    observation: &CognitionObservation,
) -> Result<CognitionSnapshotReport> {
    let mut node = Node::open(dir)?;
    node.handle(Request::Initialize {
        node_id,
        identity: None,
    })?;
    let report = node.publish_cognition_report(observation)?;
    drop(node);
    Ok(report)
}

#[test]
fn report_survives_process_reopen_and_unknown_stays_unknown() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (dir, node_id) = installation(root.path(), "installation")?;

    // Nothing has been observed yet: the route says unknown, not a default.
    assert_eq!(reported(&dir)?, None);

    let first = publish(&dir, node_id, &observation("snapshot-a", 7, scope(0, true), 0))?;
    assert_eq!(first.node_id, node_id);
    assert_eq!(first.high_water, 7);
    assert_eq!(first.generation, 1);

    // A different process reads the same bytes off the file and serves them.
    assert_eq!(reported(&dir)?, Some(first.clone()));

    // Reopening the installation keeps the report and its identity.
    let mut node = Node::open(&dir)?;
    assert_eq!(node.cognition_report()?, Some(first.clone()));
    node.handle(Request::Initialize {
        node_id,
        identity: None,
    })?;
    Ok(())
}

#[test]
fn unchanged_content_is_one_revision_and_a_change_is_a_new_one() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (dir, node_id) = installation(root.path(), "installation")?;
    let first = publish(&dir, node_id, &observation("snapshot-a", 7, scope(0, true), 0))?;
    assert_eq!(first.generation, 1);

    // Re-observing the same content at a later instant is the same revision:
    // §51.1 dedups by content, so nothing new is opened and the stored bytes —
    // including the instant this content was first observed — stay put.
    let again = publish(&dir, node_id, &observation("snapshot-a", 7, scope(0, true), 60))?;
    assert_eq!(again, first);
    assert_eq!(again.observed_at, instant(0));

    // A different watermark is different content, so it is a new revision.
    let advanced = publish(&dir, node_id, &observation("snapshot-a", 9, scope(0, true), 120))?;
    assert_eq!(advanced.generation, 2);
    assert_eq!(advanced.high_water, 9);
    assert_eq!(advanced.observed_at, instant(120));
    assert_eq!(reported(&dir)?, Some(advanced.clone()));

    // A different coverage is different content too: the same digest over a
    // wider scope is not the same statement.
    let wider = publish(&dir, node_id, &observation("snapshot-a", 9, scope(1, true), 180))?;
    assert_eq!(wider.generation, 3);

    // A content change cannot be walked backwards in time by a replayed
    // observation; the newest stored observation keeps its place.
    let mut node = Node::open(&dir)?;
    let stale = node.publish_cognition_report(&observation("snapshot-b", 11, scope(1, true), 30));
    assert!(
        stale.is_err(),
        "an observation from before the stored one replaced it"
    );
    drop(node);

    // The refusal left the published revision untouched.
    assert_eq!(reported(&dir)?, Some(wider));
    Ok(())
}

#[test]
fn peer_report_is_compared_by_content_and_coverage_never_by_liveness() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (dir_a, a_id) = installation(root.path(), "local")?;
    let (dir_b, b_id) = installation(root.path(), "peer")?;
    let (dir_c, _) = installation(root.path(), "silent")?;

    // This installation observed its own Store.
    let local = publish(&dir_a, a_id, &observation("shared-snapshot", 4, scope(0, true), 0))?;
    assert_eq!(local.generation, 1);

    // The peer observed the same replicated content over the same coverage.
    let peer_same = publish(&dir_b, b_id, &observation("shared-snapshot", 4, scope(0, true), 5))?;
    assert_eq!(peer_same.fingerprint, local.fingerprint);
    let record = run(
        &dir_a,
        &Request::Reconcile {
            request: experience(b_id, Some(&peer_same)),
        },
    )?;
    assert_eq!(record["peer_node_id"], json!(b_id));
    assert_eq!(record["cognition_fingerprint"], json!(local.fingerprint));
    assert_eq!(record["cognition_high_water"], json!(4u64));
    assert_eq!(record["cognition_origin"]["generation"], json!(1));
    assert_eq!(record["cognition_comparison"], json!("same_snapshot"));

    // Different content over the same coverage is a different snapshot.
    let peer_other = publish(&dir_b, b_id, &observation("other-snapshot", 5, scope(0, true), 60))?;
    let record = run(
        &dir_a,
        &Request::Reconcile {
            request: experience(b_id, Some(&peer_other)),
        },
    )?;
    assert_eq!(record["cognition_fingerprint"], json!(peer_other.fingerprint));
    assert_eq!(record["cognition_comparison"], json!("different_snapshot"));

    // A digest over different origins is not comparable at all.
    let peer_wider =
        publish(&dir_b, b_id, &observation("shared-snapshot", 6, scope(2, false), 120))?;
    let record = run(
        &dir_a,
        &Request::Reconcile {
            request: experience(b_id, Some(&peer_wider)),
        },
    )?;
    assert_eq!(record["cognition_comparison"], json!("coverage_differs"));

    // A peer that published nothing stays unknown — not equal, not different.
    let record = run(
        &dir_a,
        &Request::Reconcile {
            request: experience(b_id, None),
        },
    )?;
    assert_eq!(record["cognition_comparison"], json!("unobserved"));
    assert_eq!(record["cognition_fingerprint"], Value::Null);
    assert_eq!(record["cognition_high_water"], Value::Null);
    assert_eq!(record["cognition_origin"], Value::Null);

    // An installation that never published a report compares as unknown even
    // when the peer's report is a real one.
    let record = run(
        &dir_c,
        &Request::Reconcile {
            request: experience(b_id, Some(&peer_same)),
        },
    )?;
    assert_eq!(record["cognition_comparison"], json!("unobserved"));

    // Half a report states nothing observable and is refused outright.
    let mut fragment = experience(b_id, Some(&peer_same));
    fragment.cognition_origin = None;
    let refused = run_refused(
        &dir_a,
        &Request::Reconcile {
            request: fragment,
        },
    )?;
    assert!(
        refused.contains("must carry"),
        "the refusal did not name the missing provenance: {refused}"
    );

    // None of the merges replaced this installation's own observation.
    assert_eq!(reported(&dir_a)?, Some(local));
    assert_eq!(reported(&dir_c)?, None);
    Ok(())
}
