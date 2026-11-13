//! docs/requirements/traceability.md#spec-32, docs/requirements/design-provenance.md#spec-39 index parsing — the real requirement and acceptance-case
//! universe, read from the actual SPEC table rows.
//!
//! The coverage ledger in `docs/state/coverage.json` is *not* the authority
//! for which requirements or acceptance cases exist: it is a hand-maintained
//! view that can drift. This module parses the SPEC's own tables so the
//! denominator comes from current contract definitions rather than frozen counts;
//! a stale coverage ID cannot silently drop an obligation.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

/// One acceptance case row from a SPEC acceptance table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceCase {
    pub case_id: String,
    /// The scenario cell of the SPEC row.
    pub scenario: String,
    /// The pass-condition cell of the SPEC row.
    pub pass_condition: String,
    /// True when the SPEC row merges scenario and pass condition into one
    /// cell, so no separate pass condition can be read from the table.
    pub merged_row: bool,
    /// Current coordinate used only to diagnose duplicate definitions.
    pub(crate) spec_line: usize,
    /// The current document containing this semantic definition.
    pub source_path: String,
    /// The unique explicit anchor owning this semantic case ID.
    pub source_anchor: String,
    /// The verbatim SPEC row.
    pub source_row: String,
}

/// One requirement row from docs/requirements/traceability.md#spec-32.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub id: String,
    pub summary: String,
    /// `§`-references from the SPEC row, e.g. "docs/specs/architecture.md#spec-1, docs/specs/subject-runtime.md#spec-4, docs/specs/channels.md#spec-15–docs/specs/channel-adapters.md#spec-16".
    pub spec_locations: String,
    /// The `U`/`B` reference cell, e.g. "U01；B01/B22".
    pub user_source: String,
    /// Behavior IDs named by the requirement row.
    pub behaviors: Vec<String>,
    /// Current coordinate used only to diagnose duplicate definitions.
    pub(crate) spec_line: usize,
    pub source_path: String,
    pub source_anchor: String,
    pub source_row: String,
}

/// One behavior row from docs/requirements/design-provenance.md#spec-39.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Behavior {
    pub id: String,
    pub title: String,
    pub refs: String,
    pub contract: String,
    /// The verification cell, which also names acceptance cases.
    pub verification: String,
    /// Acceptance case IDs referenced by this behavior's verification cell.
    pub acceptance_cases: Vec<String>,
    /// Explicit IDs or ranges that cannot be resolved completely.
    pub unresolved_case_refs: Vec<String>,
    /// Current coordinate used only to diagnose duplicate definitions.
    pub(crate) spec_line: usize,
    pub source_path: String,
    pub source_anchor: String,
    pub source_row: String,
}

/// The parsed SPEC index.
#[derive(Debug, Clone, Default)]
pub struct SpecIndex {
    pub requirements: BTreeMap<String, Requirement>,
    pub behaviors: BTreeMap<String, Behavior>,
    pub acceptance_cases: BTreeMap<String, AcceptanceCase>,
}

/// Result of parsing: the index plus the counts the SPEC promises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCounts {
    pub requirements: usize,
    pub behaviors: usize,
    pub acceptance_cases: usize,
}

impl SpecIndex {
    /// Parse one current contract document with its explicit source path.
    ///
    /// Rows are recognised by their leading ID cell, so the parser follows
    /// the contract's own tables instead of a fixed list of IDs.
    pub fn parse(source_path: &str, spec_text: &str) -> Result<SpecIndex, IndexError> {
        Self::parse_documents([(source_path, spec_text)])
    }

    /// Parse documents independently, rejecting duplicate semantic definitions
    /// across the whole inventory before resolving references between documents.
    pub fn parse_documents<'a>(
        documents: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<SpecIndex, IndexError> {
        let mut index = SpecIndex::default();
        for (source_path, spec_text) in documents {
            let mut document = SpecIndex::default();
            document.add_document(source_path, spec_text)?;
            merge_definitions(
                &mut index.requirements, document.requirements,
                "requirement", source_path, |item| item.spec_line,
            )?;
            merge_definitions(
                &mut index.behaviors, document.behaviors,
                "behavior", source_path, |item| item.spec_line,
            )?;
            merge_definitions(
                &mut index.acceptance_cases, document.acceptance_cases,
                "acceptance case", source_path, |item| item.spec_line,
            )?;
        }
        for behavior in index.behaviors.values_mut() {
            (behavior.acceptance_cases, behavior.unresolved_case_refs) =
                acceptance_case_refs(&behavior.verification, &index.acceptance_cases);
        }
        Ok(index)
    }

    fn add_document(&mut self, source_path: &str, spec_text: &str) -> Result<(), IndexError> {
        // Count the whole document first: a later duplicate also makes the
        // anchor ambiguous for rows that appeared before it.
        let mut anchors = BTreeMap::new();
        for (_, line) in outside_fences(spec_text) {
            for anchor in explicit_anchors(line) {
                *anchors.entry(anchor).or_insert(0usize) += 1;
            }
        }
        let mut previous_table_row = None;
        let mut acceptance_table = None;
        let mut source_anchor = None;
        for (line_no, line) in outside_fences(spec_text) {
            for anchor in explicit_anchors(line) {
                source_anchor = Some(anchor);
            }
            let Some(cells) = split_table_row(line) else {
                previous_table_row = None;
                acceptance_table = None;
                continue;
            };
            if cells.is_empty() {
                continue;
            }
            if cells.iter().all(|cell| {
                !cell.is_empty() && cell.bytes().all(|byte| matches!(byte, b'-' | b':'))
            }) {
                acceptance_table = previous_table_row
                    .and_then(split_table_row)
                    .map(|header| is_acceptance_table(&header));
                continue;
            }
            previous_table_row = Some(line);
            let head = cells[0].trim();
            if head.starts_with('R') && is_requirement_id(head) {
                if cells.len() < 4 {
                    return Err(IndexError::MalformedRow {
                        source_path: source_path.to_string(),
                        line: line_no,
                        id: head.to_string(),
                    });
                }
                let source_anchor = owning_anchor(&anchors, source_anchor, source_path, line_no, head)?;
                let requirement = Requirement {
                    id: head.to_string(),
                    summary: cells[1].trim().to_string(),
                    spec_locations: cells[2].trim().to_string(),
                    user_source: cells[3].trim().to_string(),
                    behaviors: behavior_refs(&cells[3]),
                    spec_line: line_no,
                    source_path: source_path.to_string(),
                    source_anchor: source_anchor.to_string(),
                    source_row: line.to_string(),
                };
                match self.requirements.entry(requirement.id.clone()) {
                    Entry::Vacant(entry) => { entry.insert(requirement); }
                    Entry::Occupied(_) => return Err(IndexError::DuplicateDefinition {
                        kind: "requirement", id: requirement.id,
                        source_path: source_path.to_string(), line: line_no,
                    }),
                }
                continue;
            }
            if let Some((behavior_id, title)) = split_behavior_id(head) {
                if cells.len() < 4 {
                    return Err(IndexError::MalformedRow {
                        source_path: source_path.to_string(),
                        line: line_no,
                        id: behavior_id.clone(),
                    });
                }
                let source_anchor = owning_anchor(&anchors, source_anchor, source_path, line_no, &behavior_id)?;
                let verification = cells[3].trim().to_string();
                let behavior = Behavior {
                    id: behavior_id.clone(),
                    title,
                    refs: cells[1].trim().to_string(),
                    contract: cells[2].trim().to_string(),
                    acceptance_cases: Vec::new(),
                    unresolved_case_refs: Vec::new(),
                    verification,
                    spec_line: line_no,
                    source_path: source_path.to_string(),
                    source_anchor: source_anchor.to_string(),
                    source_row: line.to_string(),
                };
                match self.behaviors.entry(behavior.id.clone()) {
                    Entry::Vacant(entry) => { entry.insert(behavior); }
                    Entry::Occupied(_) => return Err(IndexError::DuplicateDefinition {
                        kind: "behavior", id: behavior.id,
                        source_path: source_path.to_string(), line: line_no,
                    }),
                }
                continue;
            }
            if acceptance_table != Some(false)
                && let Some((case_id, scenario, pass_condition, merged_row)) = parse_acceptance_case_row(&cells)
            {
                let source_anchor = owning_anchor(&anchors, source_anchor, source_path, line_no, &case_id)?;
                let case = AcceptanceCase {
                    case_id,
                    scenario,
                    pass_condition,
                    merged_row,
                    spec_line: line_no,
                    source_row: line.to_string(),
                    source_path: source_path.to_string(),
                    source_anchor: source_anchor.to_string(),
                };
                match self.acceptance_cases.entry(case.case_id.clone()) {
                    Entry::Vacant(entry) => { entry.insert(case); }
                    Entry::Occupied(_) => return Err(IndexError::DuplicateDefinition {
                        kind: "acceptance case", id: case.case_id,
                        source_path: source_path.to_string(), line: line_no,
                    }),
                }
            }
        }
        Ok(())
    }

    /// Counts actually parsed from the SPEC.
    pub fn counts(&self) -> IndexCounts {
        IndexCounts {
            requirements: self.requirements.len(),
            behaviors: self.behaviors.len(),
            acceptance_cases: self.acceptance_cases.len(),
        }
    }

    /// Acceptance cases named by any behavior, in ID order.
    pub fn referenced_acceptance_cases(&self) -> BTreeSet<String> {
        let mut referenced = BTreeSet::new();
        for behavior in self.behaviors.values() {
            for case_id in &behavior.acceptance_cases {
                referenced.insert(case_id.clone());
            }
        }
        referenced
    }

    /// Requirements that reference a given acceptance case, through the
    /// behaviors that name it.
    pub fn requirements_for_case(&self, case_id: &str) -> Vec<String> {
        let mut owners = BTreeSet::new();
        for behavior in self.behaviors.values() {
            if !behavior.acceptance_cases.iter().any(|c| c == case_id) {
                continue;
            }
            for requirement in self.requirements.values() {
                if requirement.behaviors.iter().any(|b| b == &behavior.id) {
                    owners.insert(requirement.id.clone());
                }
            }
        }
        owners.into_iter().collect()
    }
}

/// Errors raised while parsing the SPEC index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    MalformedRow { source_path: String, line: usize, id: String },
    DuplicateDefinition { kind: &'static str, id: String, source_path: String, line: usize },
    InvalidOwner {
        source_path: String,
        line: usize,
        id: String,
        anchor: Option<String>,
        definitions: usize,
    },
}

fn merge_definitions<T>(
    target: &mut BTreeMap<String, T>,
    mut definitions: BTreeMap<String, T>,
    kind: &'static str,
    source_path: &str,
    line_of: impl Fn(&T) -> usize,
) -> Result<(), IndexError> {
    for (id, definition) in &definitions {
        if target.contains_key(id) {
            return Err(IndexError::DuplicateDefinition {
                kind,
                id: id.clone(),
                source_path: source_path.to_string(),
                line: line_of(definition),
            });
        }
    }
    target.append(&mut definitions);
    Ok(())
}

/// Match scripts/doc_refs.py's outside_fences: up to three whitespace
/// characters, backticks or tildes, and a closing run at least as long as the
/// opener with only whitespace after it. Every document starts outside a fence.
fn outside_fences(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut fence = None;
    text.lines().enumerate().filter_map(move |(offset, line)| {
        if let Some((delimiter, length, rest)) = fence_marker(line) {
            match fence {
                None => fence = Some((delimiter, length)),
                Some((opening_delimiter, minimum_length))
                    if delimiter == opening_delimiter
                        && length >= minimum_length
                        && rest.trim().is_empty() => fence = None,
                _ => {}
            }
            None
        } else if fence.is_none() {
            Some((offset + 1, line))
        } else {
            None
        }
    })
}

fn fence_marker(mut line: &str) -> Option<(u8, usize, &str)> {
    for _ in 0..3 {
        if let Some(character) = line.chars().next().filter(|c| c.is_whitespace()) {
            line = &line[character.len_utf8()..];
        } else {
            break;
        }
    }
    let delimiter = *line.as_bytes().first()?;
    if !matches!(delimiter, b'`' | b'~') {
        return None;
    }
    let length = line.bytes().take_while(|byte| *byte == delimiter).count();
    (length >= 3).then_some((delimiter, length, &line[length..]))
}

/// Current contracts use explicit HTML anchors; a generated heading slug or a
/// source offset cannot silently stand in for a missing authored identity.
fn explicit_anchors(mut line: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        while let Some(start) = line.find('<') {
            let candidate = &line[start..];
            line = &candidate[1..];
            let Some(rest) = candidate.strip_prefix("<a").or_else(|| candidate.strip_prefix("<span")) else {
                continue;
            };
            let trimmed = rest.trim_start();
            if trimmed.len() == rest.len() {
                continue;
            }
            let Some(value) = trimmed.strip_prefix("id=").or_else(|| trimmed.strip_prefix("name=")) else {
                continue;
            };
            if !value.starts_with(['\'', '"']) {
                continue;
            }
            let value = &value[1..];
            let Some(end) = value.find(['\'', '"']) else {
                continue;
            };
            if end == 0 {
                continue;
            }
            line = &value[end + 1..];
            return Some(&value[..end]);
        }
        None
    })
}

fn owning_anchor<'a>(
    anchors: &BTreeMap<&str, usize>,
    anchor: Option<&'a str>,
    source_path: &str,
    line: usize,
    id: &str,
) -> Result<&'a str, IndexError> {
    let definitions = anchor.and_then(|name| anchors.get(name)).copied().unwrap_or(0);
    if let Some(anchor) = anchor
        && definitions == 1
    {
        return Ok(anchor);
    }
    Err(IndexError::InvalidOwner {
        source_path: source_path.to_string(),
        line,
        id: id.to_string(),
        anchor: anchor.map(str::to_string),
        definitions,
    })
}

/// Split a markdown table row into trimmed cells, or `None` when the line is
/// not a table row.
fn split_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return None;
    }
    let inner = trimmed.strip_prefix('|')?;
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    Some(inner.split('|').map(|c| c.trim().to_string()).collect())
}

/// Column roles distinguish acceptance definitions from decision/reference
/// tables. Unknown case families in these tables are retained, not guessed.
fn is_acceptance_table(header: &[String]) -> bool {
    match header.first().map(String::as_str) {
        Some("Case") => true,
        Some("ID" | "编号") => header.last().is_some_and(|cell| {
            matches!(
                cell.as_str(),
                "通过条件"
                    | "可判定结果"
                    | "必须观察到的行为"
                    | "场景与通过条件"
                    | "成立条件"
                    | "必须可观察的结果"
                    | "必须观察到的结果"
                    | "必须出现的真实证据"
            )
        }),
        _ => false,
    }
}

fn is_requirement_id(cell: &str) -> bool {
    let bytes = cell.as_bytes();
    bytes.len() == 4 && bytes[0] == b'R' && bytes[1..].iter().all(|b| b.is_ascii_digit())
}

fn is_behavior_id(cell: &str) -> bool {
    let bytes = cell.as_bytes();
    bytes.len() == 3 && bytes[0] == b'B' && bytes[1..].iter().all(|b| b.is_ascii_digit())
}

fn split_behavior_id(head: &str) -> Option<(String, String)> {
    let end = head.find(char::is_whitespace).unwrap_or(head.len());
    let id = &head[..end];
    is_behavior_id(id).then(|| (id.to_string(), head[end..].trim().to_string()))
}

/// An acceptance case row is `| <ID> | <scenario> | <pass condition> |`.
///
/// The SPEC uses three shapes and the parser accepts all of them:
///   * two cells — the scenario and its pass condition share one cell
///     (V1-01…V1-15, H01…H10, K01…K09);
///   * three cells — scenario and pass condition are separate; the ID cell
///     may carry a title (`QA01 指标隔离`);
///   * four cells — a fault-matrix row with an extra expectation cell
///     (DX01…DX18).
fn parse_acceptance_case_row(cells: &[String]) -> Option<(String, String, String, bool)> {
    let head = cells.first()?.trim();
    let (case_id, _title) = split_case_id_with_title(head);
    if case_id.is_empty() {
        return None;
    }
    if cells.len() < 2 {
        return None;
    }
    let (scenario, pass_condition, merged_row) = match cells.len() {
        2 => (cells[1].clone(), String::new(), true),
        3 => (cells[1].clone(), cells[2].clone(), false),
        _ => (cells[1].clone(), cells[cells.len() - 1].clone(), false),
    };
    Some((case_id, scenario, pass_condition, merged_row))
}

/// Split `QA01 指标隔离` into ("QA01", Some(title)) and `A01` into
/// ("A01", None).
fn split_case_id_with_title(head: &str) -> (String, Option<String>) {
    let end = head.find(char::is_whitespace).unwrap_or(head.len());
    let id = head[..end].to_string();
    let title = head[end..].trim();
    if !is_case_id(&id) {
        return (String::new(), None);
    }
    if title.is_empty() {
        (id, None)
    } else {
        (id, Some(title.to_string()))
    }
}

/// An acceptance case ID is a letter prefix plus digits, e.g. `A01`,
/// `UI-S01`, `V1-01`, `P41-01`.
fn is_case_id(id: &str) -> bool {
    let prefix = id.trim_end_matches(|c: char| c.is_ascii_digit());
    // R/B/U belong to the requirement/behavior/source tables, not the
    // acceptance universe. A bare version/phase such as V1 or S0 is not
    // a zero-padded acceptance identifier either.
    if matches!(prefix, "R" | "B" | "U") || id.len() - prefix.len() < 2 {
        return false;
    }
    if !prefix
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        return false;
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
    {
        return false;
    }
    true
}

/// Behavior IDs named in a requirement row's reference cell (`U01；B01/B22`).
fn behavior_refs(cell: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    let bytes = cell.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'B' && index + 2 < bytes.len() {
            let digits = &bytes[index + 1..index + 3];
            if digits.iter().all(u8::is_ascii_digit) {
                let after = bytes.get(index + 3).copied();
                if !after.is_some_and(|c| c.is_ascii_digit()) {
                    found.insert(cell[index..index + 3].to_string());
                    index += 3;
                    continue;
                }
            }
        }
        index += 1;
    }
    found.into_iter().collect()
}

/// Acceptance case IDs named in a behavior's verification cell.
///
/// The cell mixes section references (`docs/specs/verification.md#spec-29, docs/requirements/design-provenance.md#spec-39–docs/governance/development-contract.md#spec-40, docs/specs/architecture.md#spec-41`), explicit case IDs
/// (`A24`), ID lists (`A01/A02/A03/A31/A36/A37`) and ranges (`E01–E10`,
/// `UI-S01–10/UI01–04`, `V1-01–V1-15`, `AW01–09`). Ranges are expanded
/// against the universe of case IDs the SPEC actually defines, so a range
/// never invents an ID that does not exist.
fn acceptance_case_refs(
    cell: &str,
    universe: &BTreeMap<String, AcceptanceCase>,
) -> (Vec<String>, Vec<String>) {
    let mut found = BTreeSet::new();
    let mut unresolved = BTreeSet::new();
    let mut cursor = 0;
    while cursor < cell.len() {
        let rest = &cell[cursor..];
        let Some(offset) = rest.find(|c: char| c.is_ascii_uppercase()) else {
            break;
        };
        let start = cursor + offset;
        let token_end = start + id_token_len(&cell[start..]);
        let first = &cell[start..token_end];
        cursor = token_end;
        if !is_case_id(first) {
            continue;
        }
        let after = cell[token_end..].trim_start();
        if after.starts_with(['–', '—']) {
            let upper = after[3..].trim_start();
            let upper_len = id_token_len(upper);
            cursor = cell.len() - upper.len() + upper_len;
            let token = &cell[start..cursor];
            if let Some((prefix, low, high)) = parse_range(first, &upper[..upper_len]) {
                let mut count = 0u64;
                for id in universe.keys() {
                    if let Some((family, number)) = strip_trailing_digits(id) {
                        if family == prefix && (low..=high).contains(&number) {
                            found.insert(id.clone());
                            count += 1;
                        }
                    }
                }
                if high.checked_sub(low).and_then(|n| n.checked_add(1)) != Some(count) {
                    unresolved.insert(token.to_string());
                }
            } else {
                unresolved.insert(token.to_string());
            }
        } else if universe.contains_key(first) {
            found.insert(first.to_string());
        } else {
            unresolved.insert(first.to_string());
        }
    }
    (
        found.into_iter().collect(),
        unresolved.into_iter().collect(),
    )
}

fn id_token_len(text: &str) -> usize {
    text.bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'-')
        .count()
}

/// `A01` → `("A", 1)`, `E10` → `("E", 10)`.
fn strip_trailing_digits(token: &str) -> Option<(&str, u64)> {
    let position = token.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (prefix, digits) = token.split_at(position);
    if prefix.is_empty() || digits.is_empty() {
        return None;
    }
    let number = digits.parse().ok()?;
    Some((prefix, number))
}

/// Parse `A03–A05` or `AW01–09` style ranges into prefix and bounds.
fn parse_range<'a>(first: &'a str, upper: &str) -> Option<(&'a str, u64, u64)> {
    let (prefix, low) = strip_trailing_digits(first)?;
    let high = if upper.bytes().all(|c| c.is_ascii_digit()) {
        upper.parse().ok()?
    } else {
        let (end_prefix, high) = strip_trailing_digits(upper)?;
        if prefix != end_prefix {
            return None;
        }
        high
    };
    (low <= high).then_some((prefix, low, high))
}

/// Build a JSON view of the parsed index, for machine-readable reports.
pub fn index_to_json(index: &SpecIndex) -> Value {
    let requirements: Vec<Value> = index
        .requirements
        .values()
        .map(|requirement| {
            serde_json::json!({
                "requirement_id": requirement.id,
                "summary": requirement.summary,
                "spec_locations": requirement.spec_locations,
                "user_source": requirement.user_source,
                "behaviors": requirement.behaviors,
                "source_path": requirement.source_path,
                "source_anchor": requirement.source_anchor,
            })
        })
        .collect();
    let cases: Vec<Value> = index
        .acceptance_cases
        .values()
        .map(|case| {
            serde_json::json!({
                "case_id": case.case_id,
                "scenario": case.scenario,
                "pass_condition": case.pass_condition,
                "source_path": case.source_path,
                "source_anchor": case.source_anchor,
            })
        })
        .collect();
    serde_json::json!({
        "requirements": requirements,
        "acceptance_cases": cases,
        "behaviors": index.behaviors.values().map(|behavior| serde_json::json!({
            "behavior_id": behavior.id,
            "acceptance_cases": behavior.acceptance_cases,
            "unresolved_case_refs": behavior.unresolved_case_refs,
            "source_path": behavior.source_path,
            "source_anchor": behavior.source_anchor,
        })).collect::<Vec<_>>(),
    })
}
