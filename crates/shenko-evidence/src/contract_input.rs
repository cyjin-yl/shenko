//! Read current contract blocks; historical snapshots are never a denominator.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Component, Path};

pub struct ContractInput {
    pub text: String,
    segments: Vec<SourceSegment>,
}

struct SourceSegment {
    first_line: usize,
    lines: usize,
    path: String,
    source_line: usize,
}

impl ContractInput {
    /// Add the actual document locations without changing the text parser's
    /// `spec_line` coordinate (a line in the composed current-contract input).
    pub fn annotate_report(&self, report: &mut Value) {
        let Some(index) = report.get_mut("spec_index") else {
            return;
        };
        for collection in ["requirements", "behaviors", "acceptance_cases"] {
            if let Some(rows) = index.get_mut(collection).and_then(Value::as_array_mut) {
                for row in rows {
                    let Some(line) = row.get("spec_line").and_then(Value::as_u64) else {
                        continue;
                    };
                    if let Some(source) = self.segments.iter().find(|source| {
                        line >= source.first_line as u64
                            && line < (source.first_line + source.lines) as u64
                    }) {
                        row["source_path"] = Value::String(source.path.clone());
                        row["source_markdown_line"] =
                            Value::from(source.source_line as u64 + line - source.first_line as u64);
                    }
                }
            }
        }
    }
}

fn text_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value.get(field).and_then(Value::as_str)
        .ok_or_else(|| format!("contract manifest needs string {field}"))
}

fn number_field(value: &Value, field: &str) -> Result<usize, String> {
    value.get(field).and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| format!("contract manifest needs integer {field}"))
}

/// Load one explicit Markdown document or the repository's current-contract
/// manifest. A malformed manifest never falls back to Markdown or old archives.
pub fn read_contract_input(path: &Path) -> Result<ContractInput, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
        return Ok(ContractInput {
            segments: vec![SourceSegment {
                first_line: 1,
                lines: content.lines().count(),
                path: path.display().to_string(),
                source_line: 1,
            }],
            text: content,
        });
    }
    let manifest: Value = serde_json::from_str(&content)
        .map_err(|error| format!("invalid contract manifest: {error}"))?;
    if manifest.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported contract manifest schema".into());
    }
    let originals = manifest.get("originals").and_then(Value::as_array)
        .ok_or("contract manifest has no original source inventory")?;
    let spec_originals: Vec<_> = originals.iter()
        .filter(|original| original.get("name").and_then(Value::as_str) == Some("SPEC.md"))
        .collect();
    if spec_originals.len() != 1 {
        return Err("contract manifest must name one original specification".into());
    }
    let source_lines = number_field(spec_originals[0], "lines")?;
    let chunks = manifest.get("chunks").and_then(Value::as_array)
        .ok_or("contract manifest has no chunks")?;
    let mut ids = BTreeSet::new();
    let mut ordered = Vec::new();
    for chunk in chunks {
        let id = text_field(chunk, "id")?;
        if !ids.insert(id) {
            return Err(format!("duplicate contract chunk: {id}"));
        }
        if text_field(chunk, "original")? == "SPEC.md" {
            ordered.push((number_field(chunk, "start_line")?, chunk));
        }
    }
    ordered.sort_by_key(|(start, _)| *start);
    let canonical_manifest = path.canonicalize().map_err(|error| error.to_string())?;
    let root = canonical_manifest.parent().and_then(Path::parent)
        .ok_or("contract manifest must be inside the repository docs directory")?;
    if canonical_manifest.parent().and_then(Path::file_name).and_then(|s| s.to_str()) != Some("docs") {
        return Err("contract manifest must be inside the repository docs directory".into());
    }
    let mut output = ContractInput { text: String::new(), segments: Vec::new() };
    let mut cursor = 1;
    let mut composed_line = 1;
    for (start, chunk) in ordered {
        let id = text_field(chunk, "id")?;
        let end = number_field(chunk, "end_line")?;
        if start != cursor || end < start || end > source_lines {
            return Err(format!("source coverage gap or overlap at {id}"));
        }
        cursor = end.checked_add(1).ok_or("source line overflow")?;
        let relative = Path::new(text_field(chunk, "path")?);
        if !relative.starts_with("docs") || relative.components().any(|part| !matches!(part, Component::Normal(_))) {
            return Err(format!("contract path must stay in docs: {}", relative.display()));
        }
        if relative.starts_with("docs/history") || relative.starts_with("docs/research/archive") {
            return Err(format!("historical archive is not a current contract: {}", relative.display()));
        }
        let document = root.join(relative).canonicalize()
            .map_err(|error| format!("cannot locate contract {}: {error}", relative.display()))?;
        if !document.starts_with(root) {
            return Err(format!("contract path escapes repository: {}", relative.display()));
        }
        if document.starts_with(root.join("docs/history"))
            || document.starts_with(root.join("docs/research/archive"))
        {
            return Err(format!("contract resolves to a historical archive: {}", relative.display()));
        }
        let text = std::fs::read_to_string(&document)
            .map_err(|error| format!("cannot read contract {}: {error}", relative.display()))?;
        let opening = format!("<!-- migrated-contract: {id} -->\n");
        let closing = format!("<!-- /migrated-contract: {id} -->");
        let openings: Vec<_> = text.match_indices(&opening)
            .filter(|(offset, _)| *offset == 0 || text.as_bytes()[offset - 1] == b'\n')
            .collect();
        let closings: Vec<_> = text.match_indices(&closing)
            .filter(|(offset, _)| (*offset == 0 || text.as_bytes()[offset - 1] == b'\n')
                && text.as_bytes().get(offset + closing.len()).is_none_or(|byte| *byte == b'\n'))
            .collect();
        if openings.len() != 1 || closings.len() != 1 {
            return Err(format!("contract marker missing or duplicated: {id}"));
        }
        let body_start = openings[0].0 + opening.len();
        let body_end = closings[0].0;
        if body_end < body_start {
            return Err(format!("contract markers out of order: {id}"));
        }
        let body = &text[body_start..body_end];
        if format!("{:x}", Sha256::digest(body.as_bytes())) != text_field(chunk, "migrated_sha256")? {
            return Err(format!("current contract hash mismatch: {id}"));
        }
        let lines = body.lines().count().max(1);
        output.segments.push(SourceSegment {
            first_line: composed_line,
            lines,
            path: relative.display().to_string(),
            source_line: text[..body_start].bytes().filter(|byte| *byte == b'\n').count() + 1,
        });
        output.text.push_str(body);
        if !body.ends_with('\n') {
            output.text.push('\n');
        }
        composed_line += lines;
    }
    if source_lines == 0 || cursor != source_lines.checked_add(1).ok_or("source line overflow")? {
        return Err("contract manifest does not cover the complete specification".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(root: &Path) -> (std::path::PathBuf, String) {
        let docs = root.join("docs");
        std::fs::create_dir(&docs).unwrap();
        let body = "| R001 | fixture | current | B01 |\n| B01 fixture | R001 | contract | A01 |\n| A01 | scenario | condition |\n";
        let document = format!("# Current\n\n<!-- migrated-contract: SPEC:1 -->\n{body}<!-- /migrated-contract: SPEC:1 -->\n");
        std::fs::write(docs.join("current.md"), document).unwrap();
        let manifest = json!({
            "schema_version": 1,
            "originals": [{"name": "SPEC.md", "lines": 3}],
            "chunks": [{
                "id": "SPEC:1", "original": "SPEC.md", "start_line": 1,
                "end_line": 3, "path": "docs/current.md",
                "migrated_sha256": format!("{:x}", Sha256::digest(body.as_bytes())),
            }],
        });
        let path = docs.join("contract-migration.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        (path, body.to_string())
    }

    #[test]
    fn current_blocks_are_parsed_without_an_archive_and_keep_document_locations() {
        let directory = tempfile::tempdir().unwrap();
        let (path, body) = fixture(directory.path());
        let input = read_contract_input(&path).unwrap();
        assert_eq!(input.text, body);
        let index = crate::SpecIndex::parse(&input.text).unwrap();
        assert_eq!(index.counts().requirements, 1);
        assert_eq!(index.counts().behaviors, 1);
        assert_eq!(index.counts().acceptance_cases, 1);
        let mut report = json!({"spec_index": crate::spec_index::index_to_json(&index)});
        input.annotate_report(&mut report);
        let requirement = &report["spec_index"]["requirements"][0];
        assert_eq!(requirement["source_path"], "docs/current.md");
        assert_eq!(requirement["source_markdown_line"], 4);
        assert_eq!(report["spec_index"]["acceptance_cases"][0]["source_markdown_line"], 6);
    }

    #[test]
    fn changed_current_bytes_and_incomplete_manifest_coverage_are_errors() {
        let directory = tempfile::tempdir().unwrap();
        let (path, _) = fixture(directory.path());
        let current = directory.path().join("docs/current.md");
        let original = std::fs::read_to_string(&current).unwrap();
        std::fs::write(&current, original.replace("scenario", "different scenario")).unwrap();
        assert!(matches!(read_contract_input(&path), Err(error) if error.contains("hash mismatch")));
        std::fs::write(&current, original).unwrap();
        let mut manifest: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        manifest["originals"][0]["lines"] = Value::from(4);
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(read_contract_input(&path), Err(error) if error.contains("complete specification")));
    }
}
