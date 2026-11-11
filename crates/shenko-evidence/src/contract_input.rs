//! Read the full current contract inventory; migration history is not a denominator.

use serde_json::Value;
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
    /// `spec_line` coordinate (a line in the composed current-document input).
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

/// Load one explicit Markdown document or the repository's current-document
/// inventory. New obligations anywhere in a listed document are included; no
/// migration marker, old extent, checksum or archive determines that boundary.
/// Malformed inventories never fall back to a navigation page or old snapshot.
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
    let inventory: Value = serde_json::from_str(&content)
        .map_err(|error| format!("invalid contract inventory: {error}"))?;
    if inventory.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported contract inventory schema".into());
    }
    let documents = inventory.get("documents").and_then(Value::as_array)
        .filter(|documents| !documents.is_empty())
        .ok_or("current contract inventory needs a nonempty documents list")?;
    let canonical_inventory = path.canonicalize().map_err(|error| error.to_string())?;
    let root = canonical_inventory.parent().and_then(Path::parent)
        .ok_or("contract inventory must be inside the repository docs directory")?;
    if canonical_inventory.parent().and_then(Path::file_name).and_then(|s| s.to_str()) != Some("docs") {
        return Err("contract inventory must be inside the repository docs directory".into());
    }
    let mut output = ContractInput { text: String::new(), segments: Vec::new() };
    let mut seen = BTreeSet::new();
    let mut composed_line = 1;
    for entry in documents {
        let relative = Path::new(entry.as_str().ok_or("contract document must be a path string")?);
        if !relative.starts_with("docs") || relative.components().any(|part| !matches!(part, Component::Normal(_))) {
            return Err(format!("contract path must stay in docs: {}", relative.display()));
        }
        if relative.extension().and_then(|extension| extension.to_str()) != Some("md") {
            return Err(format!("contract document must be Markdown: {}", relative.display()));
        }
        let document = root.join(relative).canonicalize()
            .map_err(|error| format!("cannot locate contract {}: {error}", relative.display()))?;
        if !document.starts_with(root.join("docs")) {
            return Err(format!("contract path escapes docs: {}", relative.display()));
        }
        if document.starts_with(root.join("docs/history"))
            || document.starts_with(root.join("docs/research/archive"))
        {
            return Err(format!("historical archive is not a current contract: {}", relative.display()));
        }
        if !seen.insert(document.clone()) {
            return Err(format!("duplicate contract document: {}", relative.display()));
        }
        let text = std::fs::read_to_string(&document)
            .map_err(|error| format!("cannot read contract {}: {error}", relative.display()))?;
        let lines = text.lines().count();
        output.segments.push(SourceSegment {
            first_line: composed_line,
            lines,
            path: relative.display().to_string(),
            source_line: 1,
        });
        output.text.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            output.text.push('\n');
        }
        // A document boundary cannot keep the previous document's table open.
        output.text.push('\n');
        composed_line += lines + 1;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(root: &Path) -> std::path::PathBuf {
        let docs = root.join("docs");
        std::fs::create_dir(&docs).unwrap();
        let document = "# Current\n\n<!-- migrated-contract: SPEC:1 -->\n| R001 | first | current | B01 |\n| B01 first | R001 | contract | A01 |\n| A01 | scenario | condition |\n<!-- /migrated-contract: SPEC:1 -->\n\n| R002 | added outside old block | current | B02 |\n| B02 next | R002 | contract | A02 |\n| A02 | new scenario | condition |\n";
        std::fs::write(docs.join("current.md"), document).unwrap();
        // This document has no historical mapping or migration markers.
        std::fs::write(docs.join("new-domain.md"), "# New domain\n\n| R003 | new domain | current | B03 |\n| B03 new | R003 | contract | A03 |\n| A03 | new domain scenario | condition |\n").unwrap();
        let inventory = json!({
            "schema_version": 1,
            "documents": ["docs/current.md", "docs/new-domain.md"],
        });
        let path = docs.join("contracts.json");
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        path
    }

    #[test]
    fn current_inventory_includes_new_rows_and_documents_without_migration_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path());
        let input = read_contract_input(&path).unwrap();
        let index = crate::SpecIndex::parse(&input.text).unwrap();
        assert_eq!(index.counts().requirements, 3);
        assert_eq!(index.counts().behaviors, 3);
        assert_eq!(index.counts().acceptance_cases, 3);
        let mut report = json!({"spec_index": crate::spec_index::index_to_json(&index)});
        input.annotate_report(&mut report);
        let requirements = &report["spec_index"]["requirements"];
        assert_eq!(requirements[0]["source_path"], "docs/current.md");
        assert_eq!(requirements[0]["source_markdown_line"], 4);
        assert_eq!(requirements[1]["source_path"], "docs/current.md");
        assert_eq!(requirements[1]["source_markdown_line"], 9);
        assert_eq!(requirements[2]["source_path"], "docs/new-domain.md");
        assert_eq!(requirements[2]["source_markdown_line"], 3);
        assert_eq!(report["spec_index"]["acceptance_cases"][2]["source_markdown_line"], 5);
    }

    #[test]
    fn duplicate_or_missing_documents_do_not_produce_a_partial_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path());
        let mut inventory: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        inventory["documents"][1] = Value::String("docs/current.md".into());
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(matches!(read_contract_input(&path), Err(error) if error.contains("duplicate contract document")));
        inventory["documents"][1] = Value::String("docs/missing.md".into());
        std::fs::write(&path, serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(matches!(read_contract_input(&path), Err(error) if error.contains("cannot locate contract")));
    }
}
