//! `shenko-document` — the optional, replaceable `document.parse` definition
//! v1 (issue #89). A subject delegates document parsing on demand; the
//! boundary processes documents in the background with per-page CAS
//! checkpoints, so cancellation and restart resume rather than redo. The
//! boundary never embeds OCR engine choice, Python, model weights or
//! "when to parse" business judgement into the kernel, and never substitutes
//! one provider's output for another's.
//!
//! SoMarkAI/OXR is the first named provider: an HTTP service reached through
//! the real upstream contract (multipart `POST /parse/sync`, or
//! `/parse/async` + `task_id` polling). Where OXR cannot run (no endpoint
//! bound/reachable, no model identity, no credential, no engine
//! attestation), the boundary reports an honest `ProviderUnavailable` — the
//! SPEC's rule that an absent capability is reported, not faked.

mod oxr_client;
mod provider;
mod types;

#[cfg(test)]
mod oxr_client_tests;
#[cfg(test)]
mod tests;

pub use oxr_client::{
    CredentialHandle, DEFAULT_POLL_INTERVAL, DEFAULT_POLL_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
    OxrClient, OxrClientConfig, OxrDocument, OxrError, OxrParsedPage, OxrTransport,
};
pub use provider::{
    DocumentProvider, OxrEngineBinding, OxrProvider, OxrProviderConfig, PageOutcome,
    ProviderAvailability, page_provenance, select_provider,
};
pub use types::{
    DOCUMENT_PARSE_DEFINITION_VERSION, PageBlock, PageCheckpoint, PageProvenance, PageRef,
    PageResult, PageSize, ParseRequest, ParseState, page_cache_key, parse_request_id,
};

use anyhow::Result;
use futures::StreamExt;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The page-level concurrency the boundary defaults to when the provider
/// declares none: the hosted OXR API is limited to QPS 4 per account.
pub const DEFAULT_PARSE_CONCURRENCY: usize = 4;
/// A hard ceiling on page-level concurrency regardless of configuration: a
/// larger bound has no meaning against a QPS-4 upstream and only multiplies
/// the load.
pub const MAX_PARSE_CONCURRENCY: usize = 16;

/// The outcome of running a parse request through a provider.
///
/// `checkpoints` are the per-page terminal facts the CALLER seals into the
/// Store/journal layer under their `cache_key` (this crate deliberately does
/// not touch the Store): a `result` is a finished page, a `failure` is a
/// terminal one, and an `unknown_outcome` is an unknown effect the caller
/// must reconcile before re-sending — it is not a licence to retry blindly.
/// The run is `Complete` only when every page is finished or terminally
/// failed; any `unknown_outcome` leaves it `Running`.
#[derive(Debug)]
pub struct ParseRun {
    /// Per-page terminal facts, in page order.
    pub checkpoints: Vec<PageCheckpoint>,
    /// The request's resulting state.
    pub state: ParseState,
}

/// The bound on how many pages are parsed concurrently.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
    /// `None` = the provider's own declared ceiling (`concurrency_limit()`,
    /// OXR: QPS 4/account). An explicit value is clamped into
    /// `1..=MAX_PARSE_CONCURRENCY`; zero becomes one page at a time.
    pub max_concurrency: Option<usize>,
}

/// Drive a parse request against a selected provider with bounded page
/// concurrency, producing one durable checkpoint per page. A page that
/// succeeds records a `result`; a page that definitively fails records its
/// `failure`; a page whose outcome is genuinely unknown records
/// `unknown_outcome` (retryable, never terminal). The run never fabricates
/// a block. A provider that is not Ready produces a single
/// `ProviderUnavailable` run with no partial page claims.
pub async fn run_parse(
    req: &ParseRequest,
    provider: &Arc<dyn DocumentProvider>,
    source_bytes: &[u8],
    options: ParseOptions,
) -> Result<ParseRun> {
    if let ProviderAvailability::Unavailable { reason: _ } = provider.availability().await {
        // Honest report: no page claims, no partial result.
        return Ok(ParseRun {
            checkpoints: vec![],
            state: ParseState::ProviderUnavailable,
        });
    }

    let pages: Vec<PageRef> = if req.pages.is_empty() {
        // Whole-document parse: the page count is provider-discovered; this
        // boundary does not guess it. A provider that cannot state the count
        // yields an honest PageCountUnknown with no claimed pages.
        match provider.page_count(source_bytes, req).await? {
            Some(count) => (0..count)
                .map(|page_index| PageRef { page_index })
                .collect(),
            None => {
                return Ok(ParseRun {
                    checkpoints: vec![],
                    state: ParseState::PageCountUnknown,
                });
            }
        }
    } else {
        req.pages.clone()
    };

    let limit = options
        .max_concurrency
        .unwrap_or_else(|| provider.concurrency_limit())
        .clamp(1, MAX_PARSE_CONCURRENCY);

    let mut outcomes: Vec<(PageRef, String, Result<PageOutcome>)> =
        futures::stream::iter(pages.iter().map(|page| {
            let page = *page;
            async move {
                let cache_key = page_cache_key(req, page);
                let outcome = provider.parse_page(source_bytes, page, req).await;
                (page, cache_key, outcome)
            }
        }))
        .buffer_unordered(limit)
        .collect()
        .await;

    // `buffer_unordered` yields in completion order; the document's order is
    // the checkpoint order.
    outcomes.sort_by_key(|(page, _, _)| page.page_index);
    let mut checkpoints = Vec::new();
    let mut has_unknown = false;
    for (_page, cache_key, outcome) in outcomes {
        match outcome {
            Ok(PageOutcome::Parsed(result)) => checkpoints.push(PageCheckpoint {
                cache_key,
                result: Some(*result),
                failure: None,
                unknown_outcome: None,
                committed_at: chrono::Utc::now(),
            }),
            Ok(PageOutcome::PageFailed(reason)) => checkpoints.push(PageCheckpoint {
                cache_key,
                result: None,
                failure: Some(reason),
                unknown_outcome: None,
                committed_at: chrono::Utc::now(),
            }),
            Ok(PageOutcome::Unknown(reason)) => {
                has_unknown = true;
                checkpoints.push(PageCheckpoint {
                    cache_key,
                    result: None,
                    failure: None,
                    unknown_outcome: Some(reason),
                    committed_at: chrono::Utc::now(),
                });
            }
            // A provider-level error aborts the run rather than being
            // labelled as a page fact.
            Err(error) => return Err(error),
        }
    }
    Ok(ParseRun {
        checkpoints,
        state: if has_unknown {
            ParseState::Running
        } else {
            ParseState::Complete
        },
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
