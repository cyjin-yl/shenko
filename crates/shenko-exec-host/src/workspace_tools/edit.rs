//! Pure, byte-preserving edit planning for the workspace reference tools.
//!
//! The caller owns paths, snapshot verification and writing. Both dialects
//! resolve every edit against the original source before constructing a result;
//! neither dialect guesses matches, normalizes whitespace or changes line endings.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Exactly one editor dialect is selected by the installed tool profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorMode {
    Replace,
    Hashline,
}

/// One replacement in original-source byte coordinates, with an exclusive end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextChange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub old_text: String,
    pub new_text: String,
}

/// A complete in-memory result; creating a plan performs no external effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditPlan {
    pub updated: String,
    /// Non-overlapping changes sorted by their original starting byte.
    pub changes: Vec<TextChange>,
}

/// Stable machine-readable reason plus a diagnostic for the rejected request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EditError {
    pub code: &'static str,
    pub message: String,
}

impl EditError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_request(error: serde_json::Error) -> Self {
        Self::new("invalid_request", error.to_string())
    }
}

/// Read projection for a physical line. Reconstruct its exact bytes as
/// `text + ending`; `ending` is `"\n"`, `"\r\n"` or empty at an unterminated EOF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchoredLine {
    pub line: usize,
    pub anchor: String,
    pub text: String,
    pub ending: String,
}

// A line hash is an addressing check, not a file identity or an authorization
// token. The caller must separately compare the full source snapshot digest.
const LINE_HASH_HEX_LEN: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRequest {
    edits: Vec<Replacement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    old_text: String,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashlineRequest {
    edits: Vec<LineReplacement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LineReplacement {
    start: String,
    end: String,
    new_text: String,
}

struct SourceLine<'a> {
    start_byte: usize,
    end_byte: usize,
    raw: &'a str,
}

/// Plan a checked replacement or hashline request of the form `{ "edits": [...] }`.
///
/// Replace entries contain `old_text` and `new_text`. Each nonempty `old_text`
/// must occur exactly once, including overlapping occurrences. Hashline entries
/// contain `start`, `end` and `new_text`; anchors come from [`render_anchors`].
/// Both endpoints are inclusive, and the replaced bytes include each selected
/// line's terminator. `new_text` is literal: callers must include the desired
/// line terminators, and an empty value deletes the selected bytes.
///
/// Empty batches, ambiguous/overlapping targets and no-op edits are rejected.
pub fn apply_edit(mode: EditorMode, source: &str, request: Value) -> Result<EditPlan, EditError> {
    let changes = match mode {
        EditorMode::Replace => {
            let request: ReplaceRequest =
                serde_json::from_value(request).map_err(EditError::invalid_request)?;
            replacement_changes(source, request)?
        }
        EditorMode::Hashline => {
            let request: HashlineRequest =
                serde_json::from_value(request).map_err(EditError::invalid_request)?;
            hashline_changes(source, request)?
        }
    };
    finish_plan(source, changes)
}

/// Render 1-based anchors for actual physical lines, without inventing an empty
/// line after a trailing newline. An empty file has no line anchors. Hashes cover
/// the exact line bytes, including CRLF/LF or the absence of a final terminator.
pub fn render_anchors(source: &str) -> Vec<AnchoredLine> {
    source_lines(source)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let (text, ending) = if let Some(text) = line.raw.strip_suffix("\r\n") {
                (text, "\r\n")
            } else if let Some(text) = line.raw.strip_suffix('\n') {
                (text, "\n")
            } else {
                (line.raw, "")
            };
            AnchoredLine {
                line: index + 1,
                anchor: anchor(index + 1, line.raw),
                text: text.into(),
                ending: ending.into(),
            }
        })
        .collect()
}

fn replacement_changes(
    source: &str,
    request: ReplaceRequest,
) -> Result<Vec<TextChange>, EditError> {
    let mut changes = Vec::with_capacity(request.edits.len());
    for (index, edit) in request.edits.into_iter().enumerate() {
        let Some(first_char) = edit.old_text.chars().next() else {
            return Err(EditError::new(
                "empty_old_text",
                format!("edits[{index}].old_text must not be empty"),
            ));
        };
        let start_byte = source.find(&edit.old_text).ok_or_else(|| {
            EditError::new(
                "no_match",
                format!("edits[{index}].old_text does not occur in the source"),
            )
        })?;
        // str::match_indices skips overlapping matches. Searching again after
        // one Unicode scalar also catches ambiguous targets such as aba in ababa.
        let first_char_len = first_char.len_utf8();
        if source[start_byte + first_char_len..].contains(&edit.old_text) {
            return Err(EditError::new(
                "multiple_matches",
                format!(
                    "edits[{index}].old_text occurs more than once; include distinguishing context"
                ),
            ));
        }
        changes.push(TextChange {
            start_byte,
            end_byte: start_byte + edit.old_text.len(),
            old_text: edit.old_text,
            new_text: edit.new_text,
        });
    }
    Ok(changes)
}

fn hashline_changes(source: &str, request: HashlineRequest) -> Result<Vec<TextChange>, EditError> {
    let lines = source_lines(source);
    let mut changes = Vec::with_capacity(request.edits.len());
    for (index, edit) in request.edits.into_iter().enumerate() {
        let start = resolve_anchor(&edit.start, &lines, index, "start")?;
        let end = resolve_anchor(&edit.end, &lines, index, "end")?;
        if start > end {
            return Err(EditError::new(
                "reversed_range",
                format!("edits[{index}].start is after end"),
            ));
        }
        let start_byte = lines[start].start_byte;
        let end_byte = lines[end].end_byte;
        changes.push(TextChange {
            start_byte,
            end_byte,
            old_text: source[start_byte..end_byte].into(),
            new_text: edit.new_text,
        });
    }
    Ok(changes)
}

fn source_lines(source: &str) -> Vec<SourceLine<'_>> {
    let mut start_byte = 0;
    source
        .split_inclusive('\n')
        .map(|raw| {
            let end_byte = start_byte + raw.len();
            let line = SourceLine {
                start_byte,
                end_byte,
                raw,
            };
            start_byte = end_byte;
            line
        })
        .collect()
}

fn line_hash(raw: &str) -> String {
    blake3::hash(raw.as_bytes()).to_hex()[..LINE_HASH_HEX_LEN].to_owned()
}

fn anchor(number: usize, raw: &str) -> String {
    format!("{number}:{}", line_hash(raw))
}

fn resolve_anchor(
    value: &str,
    lines: &[SourceLine<'_>],
    edit_index: usize,
    field: &str,
) -> Result<usize, EditError> {
    let invalid = || {
        EditError::new(
            "invalid_anchor",
            format!(
                "edits[{edit_index}].{field} must be a 1-based line number and {LINE_HASH_HEX_LEN}-hex hash, N:HASH"
            ),
        )
    };
    let (number, hash) = value.split_once(':').ok_or_else(&invalid)?;
    if number.is_empty()
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || hash.len() != LINE_HASH_HEX_LEN
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid());
    }
    let number: usize = number.parse().map_err(|_| invalid())?;
    if number == 0 || number > lines.len() {
        return Err(EditError::new(
            "anchor_out_of_range",
            format!(
                "edits[{edit_index}].{field} names line {number}; the source has {} lines",
                lines.len()
            ),
        ));
    }
    let line = &lines[number - 1];
    if !hash.eq_ignore_ascii_case(&line_hash(line.raw)) {
        return Err(EditError::new(
            "anchor_mismatch",
            format!(
                "edits[{edit_index}].{field} does not match the source; current anchor is {}",
                anchor(number, line.raw)
            ),
        ));
    }
    Ok(number - 1)
}

fn finish_plan(source: &str, mut changes: Vec<TextChange>) -> Result<EditPlan, EditError> {
    if changes.is_empty() {
        return Err(EditError::new(
            "invalid_request",
            "edits must contain at least one replacement",
        ));
    }
    changes.sort_by_key(|change| change.start_byte);
    for pair in changes.windows(2) {
        if pair[0].end_byte > pair[1].start_byte {
            return Err(EditError::new(
                "overlapping_edits",
                "edits target overlapping original-source ranges",
            ));
        }
    }
    let mut updated = String::with_capacity(source.len());
    let mut cursor = 0;
    for change in &changes {
        if change.old_text == change.new_text {
            return Err(EditError::new(
                "no_change",
                format!(
                    "replacement at bytes {}..{} is unchanged",
                    change.start_byte, change.end_byte
                ),
            ));
        }
        updated.push_str(&source[cursor..change.start_byte]);
        updated.push_str(&change.new_text);
        cursor = change.end_byte;
    }
    updated.push_str(&source[cursor..]);
    if updated == source {
        return Err(EditError::new(
            "no_change",
            "the combined replacements leave the source unchanged",
        ));
    }
    Ok(EditPlan { updated, changes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replacement_batch_preserves_original_coordinates_and_untouched_bytes() {
        let source = "const α = 1;\r\nlet beta = 2;\r\n尾部";
        let plan = apply_edit(
            EditorMode::Replace,
            source,
            json!({"edits": [
                {"old_text": "let beta = 2", "new_text": "let beta = 70"},
                {"old_text": "const α = 1", "new_text": "const α = 9"}
            ]}),
        )
        .unwrap();
        assert_eq!(plan.updated, "const α = 9;\r\nlet beta = 70;\r\n尾部");
        assert_eq!(plan.changes[0].start_byte, 0);
        assert_eq!(plan.changes[1].start_byte, source.find("let beta").unwrap());
        assert_eq!(
            &source[plan.changes[1].start_byte..plan.changes[1].end_byte],
            "let beta = 2"
        );
    }

    #[test]
    fn ambiguous_overlapping_occurrences_reject_the_entire_replace_batch() {
        let error = apply_edit(
            EditorMode::Replace,
            "header\nababa\n",
            json!({"edits": [
                {"old_text": "header", "new_text": "changed"},
                {"old_text": "aba", "new_text": "resolved"}
            ]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "multiple_matches");
    }

    #[test]
    fn hashline_addresses_physical_lines_and_preserves_literal_terminators() {
        let source = "前\r\n\r\n末";
        let lines = render_anchors(source);
        assert_eq!(lines.len(), 3);
        assert_eq!((&*lines[0].text, &*lines[0].ending), ("前", "\r\n"));
        assert_eq!((&*lines[2].text, &*lines[2].ending), ("末", ""));
        let plan = apply_edit(
            EditorMode::Hashline,
            source,
            json!({"edits": [
                {"start": lines[0].anchor, "end": lines[1].anchor, "new_text": "新\r\n"}
            ]}),
        )
        .unwrap();
        assert_eq!(plan.updated, "新\r\n末");
        assert_eq!(plan.changes[0].old_text, "前\r\n\r\n");
        assert!(render_anchors("").is_empty());
        assert_eq!(render_anchors("\n").len(), 1);
        let error = apply_edit(
            EditorMode::Hashline,
            &plan.updated,
            json!({"edits": [
                {"start": lines[2].anchor, "end": lines[2].anchor, "new_text": "end"}
            ]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "anchor_out_of_range");
    }
}
