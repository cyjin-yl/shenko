//! Read the full current contract inventory; migration history is not a denominator.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Component, Path};

/// Load one explicit Markdown document or the repository's current-document
/// inventory. New obligations anywhere in a listed document are included; no
/// migration marker, old extent, checksum or archive determines that boundary.
/// Malformed inventories never fall back to a navigation page or old snapshot.
/// Each document's anchors, fences and table state are parsed independently.
pub fn read_contract_input(path: &Path) -> Result<crate::SpecIndex, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
        return crate::SpecIndex::parse(&path.display().to_string(), &content)
            .map_err(|error| format!("SPEC index error: {error:?}"));
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
    let mut sources = Vec::with_capacity(documents.len());
    let mut seen = BTreeSet::new();
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
        sources.push((relative.display().to_string(), text));
    }
    crate::SpecIndex::parse_documents(sources.iter().map(|(path, text)| (path.as_str(), text.as_str())))
        .map_err(|error| format!("SPEC index error: {error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(root: &Path) -> std::path::PathBuf {
        let docs = root.join("docs");
        std::fs::create_dir(&docs).unwrap();
        let document = r#"# Current

<a id="first"></a>
<!-- migrated-contract: SPEC:1 -->
| R001 | first | current | B01 |
| B01 first | R001 | contract | A01 |
| A01 | scenario | condition |
<!-- /migrated-contract: SPEC:1 -->

   ````markdown
<a id="first"></a>
```rust
| R901 | backtick example | example | B91 |
| B91 example | R901 | example | A91 |
| A91 | example | example |
```` trailing text cannot close the block
| R902 | still fenced | example | B92 |
~~~~
| R903 | other delimiter cannot close | example | B93 |
   `````   

~~~markdown
<a id="appended"></a>
| R904 | tilde example | example | B94 |
| B94 example | R904 | example | A94 |
| A94 | example | example |
```
| R905 | backticks cannot close tildes | example | B95 |
~~~ trailing text cannot close the block
| R906 | still tilde fenced | example | B96 |
~~~~

    ~~~ too much indentation to open a fence
`` too short to open a fence
<a id="appended"></a>
| R002 | added outside old block | current | B02 |
| B02 next | R002 | contract | A02 |
| A02 | new scenario | condition |

~~~~ unclosed fence belongs only to this document
"#;
        std::fs::write(docs.join("current.md"), document).unwrap();
        // This document has no historical mapping or migration markers.
        std::fs::write(docs.join("new-domain.md"), "# New domain\n\n<a id=\"new-domain\"></a>\n| R003 | new domain | current | B03 |\n| B03 new | R003 | contract | A03 |\n| A03 | new domain scenario | condition |\n").unwrap();
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
        let index = read_contract_input(&path).unwrap();
        assert_eq!(index.counts().requirements, 3);
        assert_eq!(index.counts().behaviors, 3);
        assert_eq!(index.counts().acceptance_cases, 3);
        assert_eq!(index.requirements["R001"].source_row, "| R001 | first | current | B01 |");
        assert_eq!(index.requirements["R002"].source_row, "| R002 | added outside old block | current | B02 |");
        assert_eq!(index.behaviors["B01"].source_row, "| B01 first | R001 | contract | A01 |");
        assert_eq!(index.acceptance_cases["A01"].source_row, "| A01 | scenario | condition |");
        let report = json!({"spec_index": crate::spec_index::index_to_json(&index)});
        let requirements = &report["spec_index"]["requirements"];
        assert_eq!(requirements[0]["source_path"], "docs/current.md");
        assert_eq!(requirements[0]["source_anchor"], "first");
        assert_eq!(requirements[1]["source_path"], "docs/current.md");
        assert_eq!(requirements[1]["source_anchor"], "appended");
        assert_eq!(requirements[2]["source_path"], "docs/new-domain.md");
        assert_eq!(requirements[2]["source_anchor"], "new-domain");
        assert_eq!(report["spec_index"]["acceptance_cases"][2]["source_anchor"], "new-domain");
        assert!(requirements[0].get("source_markdown_line").is_none());
        assert!(requirements[0].get("spec_line").is_none());
        let document = directory.path().join("docs/current.md");
        let text = std::fs::read_to_string(&document).unwrap();
        std::fs::write(&document, format!("Unrelated inserted prose.\n\n{text}")).unwrap();
        let moved_index = read_contract_input(&path).unwrap();
        let moved_report = json!({"spec_index": crate::spec_index::index_to_json(&moved_index)});
        assert_eq!(report, moved_report);
    }

    #[test]
    fn missing_repeated_and_cross_document_anchors_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path());
        let document = directory.path().join("docs/new-domain.md");
        std::fs::write(&document, "# Unowned\n\n| R003 | missing owner | current | B03 |\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("InvalidOwner"), "{error}");
        assert!(error.contains("docs/new-domain.md"), "{error}");
        assert!(error.contains("anchor: None"), "{error}");

        // An anchor after the row cannot retroactively own it, even when the
        // preceding document had an anchor with this same name.
        std::fs::write(&document, "| A03 | unowned case | condition |\n<a id=\"appended\"></a>\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("InvalidOwner"), "{error}");
        assert!(error.contains("A03"), "{error}");
        assert!(error.contains("anchor: None"), "{error}");

        std::fs::write(&document, "<a id=\"repeated\"></a>\n| B03 repeated | R003 | contract | A03 |\n<span name='repeated'></span>\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("InvalidOwner"), "{error}");
        assert!(error.contains("repeated"), "{error}");
        assert!(error.contains("definitions: 2"), "{error}");

        // Explicit names are unique within their own document, not across
        // unrelated source paths. Multiple anchors on a line are all counted.
        std::fs::write(&document, "<a id=\"first\"></a><span name='appended'></span>\n| A03 | local owner | condition |\n").unwrap();
        let index = read_contract_input(&path).unwrap();
        assert_eq!(index.acceptance_cases["A03"].source_path, "docs/new-domain.md");
        assert_eq!(index.acceptance_cases["A03"].source_anchor, "appended");

        std::fs::write(&document, "<a id=\"same\"></a><span name='same'></span>\n| A03 | ambiguous owner | condition |\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("InvalidOwner"), "{error}");
        assert!(error.contains("definitions: 2"), "{error}");
    }

    #[test]
    fn document_merge_rejects_duplicate_ids_and_resolves_cross_document_references() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path());
        let document = directory.path().join("docs/new-domain.md");
        std::fs::write(&document, "<a id=\"new-domain\"></a>\n| B03 new | R001 | contract | A01/A02 |\n").unwrap();
        let index = read_contract_input(&path).unwrap();
        assert_eq!(index.behaviors["B03"].acceptance_cases, ["A01", "A02"]);
        assert!(index.behaviors["B03"].unresolved_case_refs.is_empty());

        std::fs::write(&document, "<a id=\"new-domain\"></a>\n| R001 | duplicate requirement | current | B03 |\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("DuplicateDefinition"), "{error}");
        assert!(error.contains("requirement"), "{error}");
        assert!(error.contains("R001"), "{error}");

        std::fs::write(&document, "<a id=\"new-domain\"></a>\n| B01 duplicate | R003 | contract | A03 |\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("DuplicateDefinition"), "{error}");
        assert!(error.contains("behavior"), "{error}");
        assert!(error.contains("B01"), "{error}");

        std::fs::write(&document, "<a id=\"new-domain\"></a>\n| A01 | duplicate case | condition |\n").unwrap();
        let error = read_contract_input(&path).unwrap_err();
        assert!(error.contains("DuplicateDefinition"), "{error}");
        assert!(error.contains("acceptance case"), "{error}");
        assert!(error.contains("A01"), "{error}");
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
