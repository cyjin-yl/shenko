//! Mechanical admission of hash-bound original observations, not a truth judge.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::coverage::{EvidenceRecord, Scope};

const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct AdmittedReceipt {
    pub cases: BTreeMap<String, BTreeSet<Scope>>,
    pub binding: Value,
}

/// Repository-relative retained paths are anchored to the record's worktree,
/// never to the reporting process's current directory. Standalone inventories
/// resolve them relative to the record. Receipt-local bindings use its parent.
pub(crate) fn receipt_path(record: &EvidenceRecord) -> Option<PathBuf> {
    let named = Path::new(record.native_receipt.as_deref()?);
    if named.is_absolute() {
        return Some(named.to_path_buf());
    }
    let parent = Path::new(&record.source_path).parent()?;
    let root = parent
        .ancestors()
        .find(|root| root.join(".git").exists())
        .unwrap_or(parent);
    Some(root.join(named))
}

pub(crate) fn read_json(path: &Path) -> Result<Value, String> {
    let bytes = read_json_bytes(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid JSON at {}: {error}", path.display()))
}

fn read_json_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(format!("not a regular file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(format!(
            "JSON exceeds {MAX_JSON_BYTES} bytes: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| format!("missing or invalid {name}"))
}

fn digest_matches(bytes: &[u8], expected: &str) -> bool {
    let actual = Sha256::digest(bytes);
    hex_matches(&actual, expected)
}

fn hex_matches(actual: &[u8], expected: &str) -> bool {
    expected.len() == actual.len() * 2
        && expected
            .as_bytes()
            .chunks_exact(2)
            .zip(actual)
            .all(|(hex, byte)| {
                let digit = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    _ => None,
                };
                digit(hex[0])
                    .zip(digit(hex[1]))
                    .is_some_and(|(high, low)| high * 16 + low == *byte)
            })
}

fn bound_file(base: &Path, binding: &Value) -> Result<PathBuf, String> {
    let path = base.join(field(binding, "path")?);
    let expected = field(binding, "sha256")?;
    let mut file =
        File::open(&path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(format!("not a regular file: {}", path.display()));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let len = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
        if len == 0 {
            break;
        }
        hasher.update(&buffer[..len]);
    }
    if !hex_matches(&hasher.finalize(), expected) {
        return Err(format!("SHA-256 mismatch: {}", path.display()));
    }
    Ok(path)
}

fn passed(value: &Value) -> bool {
    value.get("status").and_then(Value::as_str) == Some("passed") && successful_markers(value)
}

fn successful_markers(value: &Value) -> bool {
    value
        .get("failed")
        .is_none_or(|failed| failed == false || failed == 0)
        && value.get("valid").is_none_or(|valid| valid == true)
        && value.get("passed").is_none_or(|passed| passed == true)
        && value.get("success").is_none_or(|success| success == true)
        && value.get("checks").is_none_or(|checks| {
            checks.as_object().is_some_and(|items| {
                !items.is_empty() && items.values().all(|check| check.as_bool() == Some(true))
            })
        })
}

fn has_fields(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}

pub(crate) fn admit_receipt(record: &EvidenceRecord) -> Result<AdmittedReceipt, String> {
    let path = receipt_path(record).ok_or("native receipt is missing")?;
    // The descriptor supplies the content identity; a path alone is no proof.
    let descriptor = read_json(Path::new(&record.source_path))?;
    if EvidenceRecord::from_json(&descriptor, &record.source_path).as_ref() != Some(record) {
        return Err("evidence descriptor changed since it was inventoried".to_string());
    }
    let expected = field(&descriptor, "native_receipt_sha256")?;
    let bytes = read_json_bytes(&path)?;
    if !digest_matches(&bytes, expected) {
        return Err(format!(
            "native receipt SHA-256 mismatch: {}",
            path.display()
        ));
    }
    let receipt: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid native receipt: {error}"))?;
    if receipt.get("kind").and_then(Value::as_str) != Some("shenko_native_case_receipt")
        || receipt.get("schema_version").and_then(Value::as_u64) != Some(1)
    {
        return Err("original receipt has no supported case-admission schema".to_string());
    }
    if field(&receipt, "evidence_id")? != record.evidence_id
        || Some(field(&receipt, "candidate_commit")?) != record.candidate_commit.as_deref()
        || !passed(&receipt)
    {
        return Err(
            "original receipt identity or terminal outcome disagrees with the record".to_string(),
        );
    }
    let base = path.parent().ok_or("receipt has no parent")?;
    let source = &receipt["source"];
    let artifact = &receipt["artifact"];
    bound_file(base, source)?;
    bound_file(base, artifact)?;
    if !has_fields(&receipt["build"])
        || !has_fields(&receipt["configuration"])
        || !has_fields(&receipt["environment"])
        || !receipt["boundaries"].is_array()
    {
        return Err("receipt lacks build/configuration/environment/boundary context".to_string());
    }
    let run = &receipt["run"];
    let started = run["started_unix_ms"].as_u64().ok_or("missing run start")?;
    let finished = run["finished_unix_ms"]
        .as_u64()
        .ok_or("missing run finish")?;
    if finished < started
        || run["exit_code"].as_i64() != Some(0)
        || !successful_markers(run)
        || run.get("status").is_some_and(|status| status != "passed")
    {
        return Err("run has no successful terminal outcome".to_string());
    }
    let rows = receipt["cases"].as_array().ok_or("missing receipt cases")?;
    let mut cases = BTreeMap::new();
    for case in rows {
        let id = field(case, "case_id")?;
        if cases.contains_key(id) {
            return Err(format!("duplicate receipt case: {id}"));
        }
        if !passed(case) {
            return Err(format!("original case did not pass: {id}"));
        }
        let observations = case["observations"]
            .as_object()
            .filter(|items| !items.is_empty())
            .ok_or_else(|| format!("case has no original scope observations: {id}"))?;
        let mut scopes = BTreeSet::new();
        for (name, observation) in observations {
            let scope =
                Scope::parse(name).ok_or_else(|| format!("unknown receipt scope: {name}"))?;
            let checks = observation["checks"]
                .as_object()
                .filter(|items| !items.is_empty())
                .ok_or_else(|| format!("{id}/{name} has no observed checks"))?;
            if !passed(observation) || !checks.values().all(|check| check.as_bool() == Some(true)) {
                return Err(format!(
                    "{id}/{name} contains a failed or unobserved condition"
                ));
            }
            bound_file(base, &observation["output"])?;
            scopes.insert(scope);
        }
        cases.insert(id.to_string(), scopes);
    }
    if record
        .full_cases_passed
        .iter()
        .any(|id| !cases.contains_key(id))
    {
        return Err("declared full-pass cases are absent from the original receipt".to_string());
    }
    Ok(AdmittedReceipt {
        cases,
        binding: serde_json::json!({
            "path": path,
            "sha256": expected,
            "source": source,
            "artifact": artifact,
            "candidate_commit": record.candidate_commit,
        }),
    })
}
