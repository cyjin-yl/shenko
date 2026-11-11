//! Match-centred excerpts for recall results — docs/specs/context-policies.md#spec-12-3.
//!
//! A recall hit must show the actual match position in the original text,
//! not a blind prefix: the caller reads a bounded window centred on the
//! match, with explicit truncation flags and a `match_visible` marker. When
//! a match cannot be located literally (an FTS-normalized hit), the excerpt
//! keeps the prefix and says the match is not visible — never inventing a
//! span.
use serde::{Deserialize, Serialize};

/// How much of the original text a preview may carry. The same bound the
/// raw-history child uses; full text is always one explicit `read` away.
pub const MAX_PREVIEW_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchExcerpt {
    pub preview: String,
    /// Char offset of `preview` start in the original text (Unicode scalar
    /// offsets, never bytes or UTF-16 units).
    pub start_char: usize,
    pub truncated_before: bool,
    pub truncated_after: bool,
    /// The literal query text appears inside this preview.
    pub match_visible: bool,
}

/// Center a bounded excerpt on the first literal occurrence of `query`.
///
/// * CJK and mixed text match literally (`str::find`), because substring
///   semantics are what Chinese retrieval needs (docs/specs/context-policies.md#spec-12-3).
/// * ASCII queries also try a case-insensitive linear scan: the FTS index
///   can serve matches whose case differs from the stored text.
pub fn excerpt_for(text: &str, query: &str) -> MatchExcerpt {
    let query = query.trim();
    let located = if query.is_empty() {
        None
    } else {
        // Both probes answer in CHAR indices; byte offsets and char counts
        // are never mixed. `str::find` gives a byte offset, converted here.
        text.find(query)
            .map(|byte_offset| text[..byte_offset].chars().count())
            .or_else(|| ascii_case_insensitive_find(text, query))
    };
    let query_chars = query.chars().count();
    let left_context = MAX_PREVIEW_CHARS.saturating_sub(query_chars).min(100);
    let start_char = located.map_or(0, |offset| offset.saturating_sub(left_context));
    let mut chars = text.chars().skip(start_char);
    let preview: String = chars.by_ref().take(MAX_PREVIEW_CHARS).collect();
    let truncated_after = chars.next().is_some();
    let match_visible = located.is_some_and(|offset| {
        offset - start_char + query_chars <= preview.chars().count()
    });
    MatchExcerpt {
        preview,
        start_char,
        truncated_before: start_char > 0,
        truncated_after,
        match_visible,
    }
}

/// ASCII-only case-insensitive substring search; returns the char offset of
/// the first match. Linear scan over chars, no allocation.
fn ascii_case_insensitive_find(text: &str, query: &str) -> Option<usize> {
    let needle = query.as_bytes();
    if needle.is_empty() || !query.is_ascii() {
        return None;
    }
    let haystack: Vec<char> = text.chars().collect();
    let lower: Vec<u8> = needle.iter().map(u8::to_ascii_lowercase).collect();
    'outer: for start in 0..haystack.len() {
        if start + lower.len() > haystack.len() {
            break;
        }
        for (index, byte) in lower.iter().enumerate() {
            if !haystack[start + index].is_ascii()
                || !haystack[start + index].eq_ignore_ascii_case(&char::from(*byte))
            {
                continue 'outer;
            }
        }
        return Some(start);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centers_the_match_with_flags() {
        let text = format!("{}深刻{}", "前".repeat(500), "后".repeat(500));
        let excerpt = excerpt_for(&text, "深刻");
        assert!(excerpt.match_visible);
        assert!(excerpt.truncated_before);
        assert!(excerpt.truncated_after);
        assert!(excerpt.preview.contains("深刻"));
        assert!(excerpt.start_char > 0);
    }

    #[test]
    fn empty_query_reports_no_match() {
        let excerpt = excerpt_for("short text", "");
        assert!(!excerpt.match_visible);
        assert!(!excerpt.truncated_before);
        assert!(!excerpt.truncated_after);
        assert_eq!(excerpt.preview, "short text");
    }
}
