//! One archive consumer, compiled twice: once against the schema-5 tree
//! (a `d6eb9fb` checkout, `ARCHIVE_SCHEMA == 5`) and once against the current
//! schema-6 tree. Every mode uses only APIs that both trees export, so the
//! two binaries are the *actual* old writer/reader and the *actual* current
//! writer/reader, not a simulated version switch.
//!
//! Build the old binary from a crate whose `[[bin]] path` is this file while
//! its `shenko-store` dependency points at the old tree (the retained
//! `old5-reader` probe crate is exactly that). Build the current binary with
//! `cargo build --example obligation_archive_consumer`.
//!
//! Modes (positional arguments and `--name=value` options):
//!
//! ```text
//! seed   ROOT
//! export ROOT ARCHIVE [--expect-schema=N]
//! facts  ROOT
//! import ROOT ARCHIVE [--expect-schema=N] [--expect-fingerprint=HEX] [--expect-bytes=N]
//! refuse ROOT ARCHIVE [--expect-schema=N] [--expect-fingerprint=HEX] [--expect-error=TEXT]
//! relay  ROOT ARCHIVE OUTPUT [--expect-schema=N] [--expect-fingerprint=HEX]
//! ```
//!
//! `ROOT` is a store root directory holding `store.db` and `cas`; it is
//! created when absent. Every mode prints exactly one JSON object describing
//! what it observed; a mode that cannot establish its claim exits non-zero.
//!
//! This consumer never edits an archive and never rewrites a serialized
//! schema number: it reports the schema the bytes declare and the schema its
//! own build writes, so the driver can see which build did what. It also never
//! calls a model or a provider; the facts it carries are authored fixtures
//! created through the real Store and archive code of its own build.
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use shenko_store::{
    CognitionImportOptions, CognitionSnapshotOptions, MemoryEntry, MemoryIntentRecord, Store,
    ARCHIVE_SCHEMA,
};

/// The sealed policy contract an obligation terminal settlement is stored
/// under. It is a wire constant, not a private crate item, so both builds
/// can name it.
const TERMINAL_CONTRACT: &str = "subject/obligation-terminal/1";
/// The replicated fact classes reported verbatim by `facts`, so two builds
/// compare exactly the same typed facts.
const TYPED_TABLES: &[&str] = &[
    "channel_record_migrations",
    "inbox_events",
    "memory_entries",
    "memory_intent_bindings",
    "memory_intents",
    "subject_branch_heads",
    "subject_event_dispositions",
    "subject_genesis",
    "subject_intent_steps",
    "subject_intents",
    "subject_obligations",
    "subject_raw_inputs",
    "subject_steps",
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().context("mode is required")?.clone();
    if mode == "seed" {
        let root = path_arg(&args, 1)?;
        return print(seed(&root)?);
    }
    if mode == "export" {
        let root = path_arg(&args, 1)?;
        let archive = path_arg(&args, 2)?;
        let store = open(&root)?;
        return print(export(&store, &root, &archive, &args)?);
    }
    if mode == "facts" {
        let root = path_arg(&args, 1)?;
        let store = open(&root)?;
        return print(facts(&store)?);
    }
    if !matches!(mode.as_str(), "import" | "refuse" | "relay") {
        bail!("unknown consumer mode {mode}");
    }
    let root = path_arg(&args, 1)?;
    let archive = path_arg(&args, 2)?;
    // Authenticate the input bytes before the library reinterprets them: the
    // driver recorded these values when it produced the archive, so a swapped
    // or rewritten file is refused instead of silently consumed.
    let declared = declared_archive(&archive)?;
    if let Some(expected) = flag(&args, "expect-schema") {
        ensure!(
            declared["schema"].as_u64() == Some(expected.parse()?),
            "archive declares schema {}, the caller expected {expected}",
            declared["schema"]
        );
    }
    if let Some(expected) = flag(&args, "expect-fingerprint") {
        ensure!(
            declared["fingerprint"].as_str() == Some(expected.as_str()),
            "archive fingerprint {} is not {expected}",
            declared["fingerprint"]
        );
    }
    if let Some(expected) = flag(&args, "expect-bytes") {
        let bytes = fs::metadata(&archive)?.len();
        ensure!(
            bytes == expected.parse::<u64>()?,
            "archive is {bytes} bytes, the caller expected {expected}"
        );
    }
    let store = open(&root)?;
    if mode == "import" {
        return print(import(&store, &root, &archive, &declared)?);
    }
    if mode == "refuse" {
        return print(refuse(&store, &root, &archive, &declared, &args)?);
    }
    let output = path_arg(&args, 3)?;
    print(relay(&store, &root, &archive, &output, &declared, &args)?)
}

fn print(value: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

fn path_arg(args: &[String], index: usize) -> Result<PathBuf> {
    Ok(PathBuf::from(
        args.get(index).with_context(|| format!("argument {index} is required"))?,
    ))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    args.iter().find_map(|arg| arg.strip_prefix(&prefix).map(str::to_owned))
}

fn open(root: &Path) -> Result<Store> {
    fs::create_dir_all(root)?;
    Ok(Store::open(root.join("store.db"), root.join("cas"))?)
}

/// No memory files and no git bundle: the proof is about replicated facts,
/// so the archive carries exactly the durable rows and their object closure.
fn snapshot_options() -> CognitionSnapshotOptions {
    CognitionSnapshotOptions { include_git_history: false, ..Default::default() }
}

/// The library mints every identity this consumer needs, so the consumer
/// itself never names an id type nor invents one.
fn minted(store: &Store, purpose: &str) -> shenko_store::MemoryEntry {
    MemoryEntry::new(store.ledger.ledger_id(), purpose)
}

fn executable_fingerprint() -> Result<serde_json::Value> {
    let path = std::env::current_exe()?;
    let bytes = fs::read(&path)?;
    Ok(serde_json::json!({
        "path": path.to_string_lossy(),
        "bytes": bytes.len(),
        "declared_archive_schema": ARCHIVE_SCHEMA,
    }))
}

/// A genuine, version-native fact set: one typed memory save with its
/// committed entry, plus a second entry that no intent produced. Both builds
/// create these through their own Store; nothing is copied or hand-written.
fn seed(root: &Path) -> Result<serde_json::Value> {
    let store = open(root)?;
    let agent = store.ledger.ledger_id();
    let intent = minted(&store, "archive intent identity").entry_id;
    let entry_id = minted(&store, "archive entry identity").entry_id;
    let body = "a typed memory fact authored by the archive writer's own build";
    let record = MemoryIntentRecord::accepted(
        intent, agent, "save", "personal", body, None, Some(entry_id));
    store.admit_memory_intent(&record, None)?;
    let mut entry = MemoryEntry::new(agent, body);
    entry.entry_id = entry_id;
    entry.kind = "note".into();
    store.commit_memory_intent(&entry, intent)?;
    let plain = MemoryEntry::new(agent, "a second retained fact with no producing intent");
    store.insert_memory_entry(&plain)?;
    Ok(serde_json::json!({
        "mode": "seed",
        "sender_schema": ARCHIVE_SCHEMA,
        "root": root.to_string_lossy(),
        "agent_id": agent.to_string(),
        "memory_intent_id": intent.to_string(),
        "memory_entry_id": entry_id.to_string(),
        "second_memory_entry_id": plain.entry_id.to_string(),
        "executable": executable_fingerprint()?,
        "facts": facts(&store)?,
    }))
}

fn export(store: &Store, root: &Path, archive: &Path, args: &[String]) -> Result<serde_json::Value> {
    let export = store.export_cognition(&snapshot_options())?;
    ensure!(
        !export.manifest.gaps.iter().any(|gap| gap.blocking),
        "the source cannot claim a complete archive: {:?}",
        export.manifest.gaps
    );
    if let Some(expected) = flag(args, "expect-schema") {
        ensure!(
            ARCHIVE_SCHEMA == expected.parse::<u32>()?,
            "this build writes schema {ARCHIVE_SCHEMA}, the caller expected {expected}"
        );
    }
    if let Some(parent) = archive.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(archive)?;
    export.write_archive(&mut file)?;
    file.flush()?;
    drop(file);
    let declared = declared_archive(archive)?;
    ensure!(
        declared["schema"].as_u64() == Some(ARCHIVE_SCHEMA as u64),
        "the written archive declares schema {}, this build writes {ARCHIVE_SCHEMA}",
        declared["schema"]
    );
    Ok(serde_json::json!({
        "mode": "export",
        "writer_schema": ARCHIVE_SCHEMA,
        "root": root.to_string_lossy(),
        "archive": archive_summary(archive)?,
        "declared": declared,
        "complete": true,
        "executable": executable_fingerprint()?,
        "facts": facts(store)?,
    }))
}

fn import(store: &Store, root: &Path, archive: &Path, declared: &serde_json::Value) -> Result<serde_json::Value> {
    let peer = minted(store, "archive peer identity").entry_id;
    let before = facts(store)?;
    let outcome = store.import_cognition_archive(
        File::open(archive)?,
        &CognitionImportOptions { peer_id: Some(peer), ..Default::default() },
    )?;
    let ack = outcome.ack(peer);
    ensure!(
        ack.verified,
        "import was not verified: conflicts={:?} gaps={:?}",
        outcome.conflicts, outcome.gaps
    );
    let reopened = reopen_and_validate(root)?;
    let after = facts(&reopened)?;
    Ok(serde_json::json!({
        "mode": "import",
        "reader_schema": ARCHIVE_SCHEMA,
        "writer_schema": declared["schema"],
        "root": root.to_string_lossy(),
        "archive": archive_summary(archive)?,
        "declared": declared,
        "verified": ack.verified,
        "inserted": outcome.inserted,
        "updated": outcome.updated,
        "unchanged": outcome.unchanged,
        "conflicts": outcome.conflicts,
        "verified_objects": outcome.verified_objects,
        "fingerprint": outcome.fingerprint.to_string(),
        "executable": executable_fingerprint()?,
        "facts_before": before,
        "facts_after_reopen": after,
    }))
}

/// The reader must refuse the archive the caller names, for a reason the
/// caller predicted, and publish nothing while refusing. The refusal happens
/// at manifest admission, so the store's replicated fact set is compared
/// before and after on the same open store.
fn refuse(
    store: &Store,
    root: &Path,
    archive: &Path,
    declared: &serde_json::Value,
    args: &[String],
) -> Result<serde_json::Value> {
    let peer = minted(store, "refused archive peer identity").entry_id;
    let expected = match flag(args, "expect-error") {
        Some(expected) => expected,
        None => {
            let writer: u32 = declared["schema"].as_u64().context("archive declares no schema")? as u32;
            ensure!(
                writer > ARCHIVE_SCHEMA,
                "this reader supports schema {ARCHIVE_SCHEMA} and the archive declares {writer}: \
                 run this mode only against a newer archive"
            );
            format!("unsupported cognition archive schema {writer}")
        }
    };
    let before = facts(store)?;
    let error = match store.import_cognition_archive(
        File::open(archive)?,
        &CognitionImportOptions { peer_id: Some(peer), ..Default::default() },
    ) {
        Ok(outcome) => bail!(
            "the store accepted an archive that must be refused: inserted={} updated={} conflicts={:?}",
            outcome.inserted, outcome.updated, outcome.conflicts
        ),
        Err(error) => format!("{error:#}"),
    };
    ensure!(error.contains(&expected), "refused for a different reason than {expected}: {error}");
    let after = facts(store)?;
    ensure!(
        replicated(&before) == replicated(&after),
        "a refused archive changed the replicated facts of the receiving store"
    );
    let reopened = reopen_and_validate(root)?;
    ensure!(
        replicated(&facts(&reopened)?) == replicated(&before),
        "the refused store differs after open + dispatcher recovery"
    );
    Ok(serde_json::json!({
        "mode": "refuse",
        "reader_schema": ARCHIVE_SCHEMA,
        "writer_schema": declared["schema"],
        "root": root.to_string_lossy(),
        "archive": archive_summary(archive)?,
        "declared": declared,
        "refused": true,
        "error": error,
        "expected_error": expected,
        "facts_unchanged": true,
        "reopen_valid": true,
        "executable": executable_fingerprint()?,
        "facts": before,
    }))
}

/// The replicated part of a fact report. Export bookkeeping (`high_water`,
/// `source_snapshot_seq` and the content fingerprint of this particular
/// export) moves on every export and is not a replicated fact, so the refusal
/// comparison uses exactly the rows, their counts and the typed settlements.
fn replicated(facts: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "row_counts": facts["row_counts"],
        "rows": facts["rows"],
        "obligations": facts["obligations"],
        "settlements": facts["settlements"],
    })
}

fn relay(
    store: &Store,
    root: &Path,
    archive: &Path,
    output: &Path,
    declared: &serde_json::Value,
    args: &[String],
) -> Result<serde_json::Value> {
    let peer = minted(store, "relay peer identity").entry_id;
    let outcome = store.import_cognition_archive(
        File::open(archive)?,
        &CognitionImportOptions { peer_id: Some(peer), ..Default::default() },
    )?;
    ensure!(
        outcome.ack(peer).verified,
        "relay source import was not verified: conflicts={:?}",
        outcome.conflicts
    );
    let reopened = reopen_and_validate(root)?;
    let preserved = facts(&reopened)?;
    let export = reopened.export_cognition(&snapshot_options())?;
    ensure!(
        !export.manifest.gaps.iter().any(|gap| gap.blocking),
        "the relayed snapshot is incomplete: {:?}",
        export.manifest.gaps
    );
    if let Some(expected) = flag(args, "expect-schema") {
        ensure!(
            ARCHIVE_SCHEMA == expected.parse::<u32>()?,
            "this build writes schema {ARCHIVE_SCHEMA}, the caller expected {expected}"
        );
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(output)?;
    export.write_archive(&mut file)?;
    file.flush()?;
    drop(file);
    let relayed = declared_archive(output)?;
    ensure!(
        relayed["schema"].as_u64() == Some(ARCHIVE_SCHEMA as u64),
        "the relayed archive declares schema {}",
        relayed["schema"]
    );
    Ok(serde_json::json!({
        "mode": "relay",
        "reader_schema": ARCHIVE_SCHEMA,
        "writer_schema": ARCHIVE_SCHEMA,
        "input_writer_schema": declared["schema"],
        "root": root.to_string_lossy(),
        "archive": archive_summary(archive)?,
        "relayed": archive_summary(output)?,
        "relayed_declared": relayed,
        "executable": executable_fingerprint()?,
        "facts_after_reopen": preserved,
    }))
}

/// Reopen the same durable root and run the subject dispatcher, so a state
/// that only "worked" inside one process cannot pass as durable.
fn reopen_and_validate(root: &Path) -> Result<Store> {
    let store = Store::open(root.join("store.db"), root.join("cas"))?;
    store.start_subject_dispatcher()?;
    Ok(store)
}

/// The typed facts this store would replicate: every row of the compared
/// classes with its exact stored text, the terminal settlements with their
/// typed receipts, and the obligations they settle. `export_cognition`
/// allocates a local export-order row, which is transfer bookkeeping and not
/// a replicated fact, so the compared row set is stable across calls.
fn facts(store: &Store) -> Result<serde_json::Value> {
    let export = store.export_cognition(&snapshot_options())?;
    let mut rows: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut settlements = Vec::new();
    let mut obligations = Vec::new();
    for row in &export.manifest.rows {
        *counts.entry(row.table.clone()).or_default() += 1;
        let value = serde_json::json!({ "key": row.key.clone(), "values": row.values.clone() });
        if TYPED_TABLES.contains(&row.table.as_str()) {
            rows.entry(row.table.clone()).or_default().push(value.clone());
        }
        if row.table == "subject_obligations" {
            obligations.push(value);
        }
        if row.table == "subject_intents"
            && row.values.iter().any(|(name, value)| {
                name == "contract" && value.as_deref() == Some(TERMINAL_CONTRACT)
            })
        {
            let result = row
                .values
                .iter()
                .find(|(name, _)| name == "result_ref")
                .and_then(|(_, value)| value.clone());
            let typed: Option<serde_json::Value> =
                result.as_deref().and_then(|text| serde_json::from_str(text).ok());
            settlements.push(serde_json::json!({
                "key": row.key.clone(),
                "values": row.values.clone(),
                "receipt": typed,
            }));
        }
    }
    for list in rows.values_mut() {
        list.sort_by(|left, right| left["key"].as_str().cmp(&right["key"].as_str()));
    }
    Ok(serde_json::json!({
        "reader_schema": ARCHIVE_SCHEMA,
        "fingerprint": export.manifest.fingerprint.to_string(),
        "source_ledger_id": export.manifest.source_ledger_id.to_string(),
        "high_water": export.manifest.high_water.0,
        "source_snapshot_seq": export.manifest.source_snapshot_seq,
        "fact_classes": export.manifest.fact_classes,
        "row_counts": counts,
        "rows": rows,
        "obligations": obligations,
        "settlements": settlements,
        "gaps": export.manifest.gaps,
    }))
}

fn archive_summary(path: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "path": path.to_string_lossy(),
        "bytes": fs::metadata(path)?.len(),
    }))
}

/// Read the archive's own manifest frame without reinterpreting it: the
/// magic line and the manifest payload are the archive's declared identity,
/// so the caller can authenticate the bytes it is about to feed a reader.
fn declared_archive(path: &Path) -> Result<serde_json::Value> {
    let mut file = File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let magic = read_line(&mut file)?;
    ensure!(magic == "shenko-cognition-archive/1", "not a cognition archive: {magic}");
    let header = read_line(&mut file)?;
    let mut parts = header.split(' ').filter(|part| !part.is_empty());
    ensure!(
        parts.next() == Some("manifest"),
        "the archive does not start with its manifest: {header}"
    );
    let length: usize = parts
        .next()
        .context("manifest frame has no length")?
        .parse()
        .context("manifest frame length is not a number")?;
    ensure!(parts.next().is_none(), "manifest frame has trailing arguments: {header}");
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes)?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for row in manifest.get("rows").and_then(|rows| rows.as_array()).into_iter().flatten() {
        let table = row.get("table").and_then(|table| table.as_str()).unwrap_or("<unnamed>");
        *counts.entry(table.to_owned()).or_default() += 1;
    }
    Ok(serde_json::json!({
        "schema": manifest.get("schema"),
        "fingerprint": manifest.get("fingerprint"),
        "fact_classes": manifest.get("fact_classes"),
        "row_counts": counts,
        "rows": manifest.get("rows").and_then(|rows| rows.as_array()).map(Vec::len),
        "gaps": manifest.get("gaps"),
        "high_water": manifest.get("high_water"),
        "source_snapshot_seq": manifest.get("source_snapshot_seq"),
        "source_ledger_id": manifest.get("source_ledger_id"),
        "manifest_bytes": length,
    }))
}

fn read_line(reader: &mut impl Read) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        ensure!(line.len() < 8192, "archive frame header exceeds its bound");
        match reader.read(&mut byte)? {
            0 => bail!("archive ended inside a frame header"),
            _ if byte[0] == b'\n' => break,
            _ => line.push(byte[0]),
        }
    }
    Ok(String::from_utf8(line)?)
}
