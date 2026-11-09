//! `shenko-document` — the optional, replaceable `document.parse` definition
//! v1 (issue #89). A subject delegates document parsing on demand; the
//! boundary processes documents in the background with per-page CAS
//! checkpoints, so cancellation and restart resume rather than redo. The
//! boundary never embeds OCR engine choice, Python, model weights or
//! "when to parse" business judgement into the kernel, and never substitutes
//! one provider's output for another's.
//!
//! SoMarkAI/OXR is the first named provider. Where OXR cannot run (no
//! engine, no model weights, no GPU/CUDA-13 runtime), the boundary reports
//! an honest `ProviderUnavailable` — the SPEC's rule that an absent
//! capability is reported, not faked.

mod provider;
mod types;

#[cfg(test)]
mod tests;

pub use provider::{
    page_provenance, select_provider, DocumentProvider, OxrProvider, PageOutcome,
    ProviderAvailability,
};
pub use types::{
    page_cache_key, parse_request_id, PageBlock, PageCheckpoint, PageProvenance, PageRef,
    PageResult, ParseRequest, ParseState, DOCUMENT_PARSE_DEFINITION_VERSION,
};

use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The outcome of running a parse request through a provider.
#[derive(Debug)]
pub struct ParseRun {
    /// Per-page terminal facts, in page order.
    pub checkpoints: Vec<PageCheckpoint>,
    /// The request's resulting state.
    pub state: ParseState,
}

/// Drive a parse request against a selected provider, page by page,
/// producing a durable checkpoint per page. A page that succeeds records a
/// result; a page that definitively fails records its reason; the run never
/// fabricates a block. A provider that is not Ready produces a single
/// ProviderUnavailable run with no partial page claims.
pub fn run_parse(
    req: &ParseRequest,
    provider: &Arc<dyn DocumentProvider>,
    source_bytes: &[u8],
) -> Result<ParseRun> {
    let pages: Vec<PageRef> = if req.pages.is_empty() {
        // Whole-document parse: the page count is provider-discovered; this
        // boundary does not guess it. A Ready provider reports pages through
        // its parse result; an unprovisioned one reports Unavailable first.
        vec![]
    } else {
        req.pages.clone()
    };

    match provider.availability() {
        ProviderAvailability::Unavailable { reason: _ } => {
            // Honest report: no page claims, no partial result.
            return Ok(ParseRun {
                checkpoints: vec![],
                state: ParseState::ProviderUnavailable,
            });
        }
        ProviderAvailability::Ready => {}
    }

    let mut checkpoints = Vec::new();
    for page in &pages {
        let cache_key = page_cache_key(req, *page);
        match provider.parse_page(source_bytes, *page, req)? {
            PageOutcome::Parsed(result) => checkpoints.push(PageCheckpoint {
                cache_key,
                result: Some(*result),
                failure: None,
                committed_at: chrono::Utc::now(),
            }),
            PageOutcome::PageFailed(reason) => checkpoints.push(PageCheckpoint {
                cache_key,
                result: None,
                failure: Some(reason),
                committed_at: chrono::Utc::now(),
            }),
        }
    }
    Ok(ParseRun {
        checkpoints,
        state: ParseState::Complete,
    })
}

/// Index a completed run's checkpoints by their cache key for dedupe/resume.
/// A repeated request under the same closure re-uses checkpoint keys; an
/// unknown execution is reconciled, never re-sent blindly.
pub fn index_checkpoints(run: &ParseRun) -> BTreeMap<String, &PageCheckpoint> {
    run.checkpoints
        .iter()
        .map(|checkpoint| (checkpoint.cache_key.clone(), checkpoint))
        .collect()
}
