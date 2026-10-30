//! Durable history and window chain over the session store (feature `store`).
//!
//! `StoreHistory` reads committed `session_events`; `StoreWindowLog` keeps the
//! committed window chain in `context_windows`.
//!
//! Versioned raw-session payload:
//! `event_type` is one of system / user / assistant / assistant_cancelled /
//! tool_call / tool_result; `data` is JSON `{text, call_id?, tool?, args_ref?,
//! operation_id?, intent_id?, v?}`; payload status claims are not read;
//! `v` above [`PAYLOAD_VERSION`] keeps the raw data as Other. A call or result
//! without execution evidence is Unknown, never success; one without `call_id` cannot be
//! paired and is kept as [`ItemKind::Other`]; any other event type is kept as
//! Other too. Non-JSON `data` becomes the text. Nothing is dropped.
//! When `operation_id` is present the execution ledger is authoritative:
//! a referenced call's status comes from its OperationRecord. Subject intent ids join
//! the real memory receipt or task/operation/T2 records of this branch.
//! A referenced operation that is missing, unparsable or unreadable as a
//! record reads as Unknown, never as the event's own claim.
use std::sync::Arc;

use serde::Deserialize;
use shenko_store::{
    ContextWindowAppend, Store, StoredContextWindow, StoredDerivative, StoredSessionEvent,
};
use shenko_types::ids::EntityId;

use crate::history::{CallStatus, HistoryItem, HistorySource, ItemKind, ItemRef, StorageError};
use crate::policy::{Derivative, WindowBase};
use crate::window::{CommittedWindow, WindowAppend, WindowLog};

#[derive(Default, Deserialize)]
struct Payload {
    /// Payload schema version; absent means the unversioned v1 shape.
    v: Option<u32>,
    #[serde(default)]
    text: String,
    call_id: Option<String>,
    tool: Option<String>,
    args_ref: Option<String>,
    operation_id: Option<String>,
    intent_id: Option<String>,
}

/// Highest session event payload version this reader interprets.
pub const PAYLOAD_VERSION: u32 = 1;

/// Counter chosen by the caller; an estimate is not a measured tokenizer.
pub type TokenCounter = Arc<dyn Fn(&str) -> u32 + Send + Sync>;

pub struct StoreHistory {
    store: Arc<Store>,
    session_id: EntityId,
    count: TokenCounter,
}

impl StoreHistory {
    pub fn new(store: Arc<Store>, session_id: EntityId, count: TokenCounter) -> Self {
        Self { store, session_id, count }
    }

    fn item(&self, event: StoredSessionEvent) -> Result<HistoryItem, StorageError> {
        let seq = u64::try_from(event.seq)
            .map_err(|_| StorageError(format!("event {} has negative seq", event.event_id)))?;
        let payload = serde_json::from_str::<Payload>(&event.data)
            .unwrap_or_else(|_| Payload { text: event.data.clone(), ..Payload::default() });
        // A newer payload shape is not guessed at with this reader: keep the
        // raw data verbatim under a versioned Other kind.
        if let Some(v) = payload.v.filter(|v| *v > PAYLOAD_VERSION) {
            return Ok(HistoryItem {
                at: ItemRef { seq, event_id: event.event_id },
                kind: ItemKind::Other { event_type: format!("{}@v{v}", event.event_type), role: event.role },
                tokens: (self.count)(&event.data),
                text: event.data,
            });
        }
        let call_status = match payload.intent_id.as_deref() {
            Some(id) => match id.parse::<EntityId>() {
                Ok(id) => self.store.subject_call_evidence(self.session_id, id).map_err(storage)?
                    .map_or(CallStatus::Unknown, |facts| subject_status(id, &facts)),
                Err(_) => CallStatus::Unknown,
            },
            None => match payload.operation_id.as_deref() {
            None => CallStatus::Unknown,
            Some(id) => match id.parse::<EntityId>().ok().map(|id| self.store.get_operation(id)) {
                Some(Ok(Some(op))) => CallStatus::from_operation(
                    op.state,
                    op.outcome,
                    op.outcome_conflict,
                    op.effect_status,
                ),
                _ => CallStatus::Unknown,
            },
            },
        };
        let kind = match (event.event_type.as_str(), payload.call_id) {
            ("system", _) => ItemKind::System,
            ("user", _) => ItemKind::User,
            ("assistant", _) => ItemKind::Assistant { cancelled: false },
            ("assistant_cancelled", _) => ItemKind::Assistant { cancelled: true },
            ("tool_call", Some(call_id)) => ItemKind::ToolCall {
                call_id,
                tool: payload.tool.unwrap_or_default(),
                args_ref: payload.args_ref.unwrap_or_default(),
                status: call_status,
            },
            ("tool_result", Some(call_id)) => ItemKind::ToolResult { call_id, status: call_status },
            (other, _) => ItemKind::Other { event_type: other.to_owned(), role: event.role },
        };
        let text = if matches!(kind, ItemKind::Other { .. }) { event.data } else { payload.text };
        let tokens = (self.count)(&text);
        Ok(HistoryItem {
            at: ItemRef { seq, event_id: event.event_id },
            kind,
            text,
            tokens,
        })
    }
}

fn subject_status(id: EntityId, facts: &shenko_store::intents::SubjectCallEvidence) -> CallStatus {
    if matches!(facts.dispatch_state.as_str(), "unknown" | "blocked") {
        return CallStatus::Unknown;
    }
    if facts.kind == "memory" {
        return match (facts.dispatch_state.as_str(), facts.memory_state.as_deref()) {
            ("settled", Some("committed")) => CallStatus::Executed { ok: true },
            (_, Some("accepted")) => CallStatus::Pending,
            _ => CallStatus::Unknown,
        };
    }
    let Some(operation) = &facts.operation else {
        return if facts.task_state.is_some() { CallStatus::Pending } else { CallStatus::Unknown };
    };
    if operation.task_id != id {
        return CallStatus::Unknown;
    }
    let status = CallStatus::from_operation(operation.state, operation.outcome,
        operation.outcome_conflict, operation.effect_status);
    if !status.is_resolved() {
        return status;
    }
    // A finished operation is not yet a finished native task: T2 also waits
    // for owned children. A queue handoff alone is never a successful call.
    let terminal = facts.task_state.as_deref() == Some("terminal");
    let completed = match facts.kind.as_str() {
        "delegation" => terminal && facts.task_result.as_ref().is_some_and(|result|
            result.task_id == id && result.operation_id == operation.operation_id
                && Some(result.task_revision.0) == facts.task_revision),
        "publication" => terminal && facts.dispatch_state == "settled",
        _ => false,
    };
    if completed { status } else { CallStatus::Pending }
}

fn storage(error: impl std::fmt::Display) -> StorageError {
    StorageError(error.to_string())
}

impl HistorySource for StoreHistory {
    fn read_range(&self, from: u64, to: u64) -> Result<Vec<HistoryItem>, StorageError> {
        let to = i64::try_from(to).unwrap_or(i64::MAX);
        let from = i64::try_from(from).map_err(storage)?;
        self.store
            .session_events_range(self.session_id, from, to)
            .map_err(storage)?
            .into_iter()
            .map(|event| self.item(event))
            .collect()
    }

    fn read_event(&self, event_id: EntityId) -> Result<Option<HistoryItem>, StorageError> {
        self.store
            .session_event(event_id)
            .map_err(storage)?
            .filter(|event| event.session_id == self.session_id)
            .map(|event| self.item(event))
            .transpose()
    }

    fn leaf_seq(&self) -> Result<Option<u64>, StorageError> {
        self.store
            .session_leaf_seq(self.session_id)
            .map_err(storage)?
            .map(|seq| u64::try_from(seq).map_err(storage))
            .transpose()
    }
}

/// WindowLog over the store's `context_windows` chain for one scope. The
/// body is the committed window as JSON without derivative bytes; frames are
/// stored beside it in the same transaction and verified on read.
pub struct StoreWindowLog {
    store: Arc<Store>,
    scope_id: EntityId,
}

/// Stored derivative identity without the payload. `bytes` is the durable
/// byte length the store recorded; `blake3` is the digest the commit-time
/// writer verified, not a value recomputed by this reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivativeMeta {
    pub media_type: String,
    pub blake3: String,
    pub bytes: u64,
}

/// A committed window's durable facts without derivative payloads. Every
/// non-payload field of [`CommittedWindow`] is preserved verbatim:
/// `window.derivatives` is empty by construction and `derivatives` carries
/// the per-frame identity the store recorded.
#[derive(Debug, Clone)]
pub struct CommittedWindowMeta {
    /// Decoded committed body; `derivatives` is always empty here.
    pub window: CommittedWindow,
    pub derivatives: Vec<DerivativeMeta>,
}

/// One bounded page of committed window metadata, newest generation first.
#[derive(Debug, Clone)]
pub struct WindowMetaPage {
    /// Windows committed under the scope, regardless of this page's bounds.
    pub total: u64,
    /// More committed windows exist below this page's oldest generation.
    pub has_older: bool,
    pub windows: Vec<CommittedWindowMeta>,
}

impl StoreWindowLog {
    pub fn new(store: Arc<Store>, scope_id: EntityId) -> Self {
        Self { store, scope_id }
    }

    fn encode(window: &CommittedWindow) -> Result<StoredContextWindow, StorageError> {
        let body = CommittedWindow { derivatives: Vec::new(), ..window.clone() };
        Ok(StoredContextWindow {
            window_id: window.id,
            generation: i64::try_from(window.generation).map_err(storage)?,
            body: serde_json::to_string(&body).map_err(storage)?,
            derivatives: window
                .derivatives
                .iter()
                .map(|d| StoredDerivative {
                    media_type: d.media_type.clone(),
                    blake3: d.blake3.clone(),
                    bytes: d.bytes.clone(),
                })
                .collect(),
        })
    }

    fn decode(stored: StoredContextWindow) -> Result<CommittedWindow, StorageError> {
        let mut window: CommittedWindow = serde_json::from_str(&stored.body).map_err(storage)?;
        if window.id != stored.window_id {
            return Err(storage(format!("window row {} holds body of {}", stored.window_id, window.id)));
        }
        window.derivatives = stored
            .derivatives
            .into_iter()
            .map(|d| Derivative { media_type: d.media_type, blake3: d.blake3, bytes: d.bytes })
            .collect();
        Ok(window)
    }

    /// Bounded page of committed window metadata, newest generation first:
    /// `before` excludes windows with `generation >= before`; at most `limit`
    /// windows are returned. Unlike [`WindowLog::all`], this never reads
    /// derivative payloads — the page carries their recorded identity and
    /// byte length, while bodies still decode through the same committed
    /// format. Read-only metadata cannot mint windows, so an empty chain
    /// yields an empty page rather than the lazily persisted genesis.
    pub fn meta_page(&self, before: Option<u64>, limit: u64) -> Result<WindowMetaPage, StorageError> {
        let before = before.map(|g| i64::try_from(g).map_err(storage)).transpose()?;
        let limit = i64::try_from(limit).map_err(storage)?;
        let page = self
            .store
            .context_window_meta_page(self.scope_id, before, limit)
            .map_err(storage)?;
        let mut windows = Vec::with_capacity(page.windows.len());
        for stored in page.windows {
            let window: CommittedWindow =
                serde_json::from_str(&stored.body).map_err(storage)?;
            if window.id != stored.window_id {
                return Err(storage(format!(
                    "window row {} holds body of {}",
                    stored.window_id, window.id
                )));
            }
            windows.push(CommittedWindowMeta {
                window,
                derivatives: stored
                    .derivatives
                    .into_iter()
                    .map(|d| DerivativeMeta {
                        media_type: d.media_type,
                        blake3: d.blake3,
                        bytes: d.bytes,
                    })
                    .collect(),
            });
        }
        Ok(WindowMetaPage {
            total: u64::try_from(page.total).map_err(storage)?,
            has_older: page.has_older,
            windows,
        })
    }
}

impl WindowLog for StoreWindowLog {
    fn head(&self) -> Result<CommittedWindow, StorageError> {
        if let Some(head) = self.store.context_window_head(self.scope_id).map_err(storage)? {
            return Self::decode(head);
        }
        // Lazily persist genesis. Losing the race to another writer is fine:
        // whoever landed first is the genesis everyone then reads.
        let genesis = CommittedWindow::genesis();
        match self
            .store
            .append_context_window(self.scope_id, None, None, &Self::encode(&genesis)?)
            .map_err(storage)?
        {
            ContextWindowAppend::Appended | ContextWindowAppend::HeadChanged => {}
            ContextWindowAppend::ModelGenerationChanged { .. } => {
                return Err(storage("genesis append unexpectedly checked model generation"));
            }
        }
        self.store
            .context_window_head(self.scope_id)
            .map_err(storage)?
            .map(Self::decode)
            .transpose()?
            .ok_or_else(|| storage("genesis window missing after append"))
    }

    fn all(&self) -> Result<Vec<CommittedWindow>, StorageError> {
        self.store
            .context_windows(self.scope_id)
            .map_err(storage)?
            .into_iter()
            .map(Self::decode)
            .collect()
    }

    fn append(
        &self,
        expected: EntityId,
        expected_model_generation: u64,
        window: &CommittedWindow,
    ) -> Result<WindowAppend, StorageError> {
        let expected_model_generation = i64::try_from(expected_model_generation).map_err(storage)?;
        match self
            .store
            .append_context_window(
                self.scope_id,
                Some(expected),
                Some(expected_model_generation),
                &Self::encode(window)?,
            )
            .map_err(storage)?
        {
            ContextWindowAppend::Appended => Ok(WindowAppend::Appended),
            ContextWindowAppend::HeadChanged => Ok(WindowAppend::SourceChanged),
            ContextWindowAppend::ModelGenerationChanged { current } => {
                Ok(WindowAppend::ModelGenerationChanged {
                    current: u64::try_from(current).map_err(storage)?,
                })
            }
        }
    }

    fn model_generation(&self) -> Result<u64, StorageError> {
        let generation = self.store.context_model_generation(self.scope_id).map_err(storage)?;
        u64::try_from(generation).map_err(storage)
    }

    fn model_profile(&self) -> Result<Option<(u64, String)>, StorageError> {
        self.store.context_profile(self.scope_id).map_err(storage)?
            .map(|(generation, profile)| Ok((u64::try_from(generation).map_err(storage)?, profile)))
            .transpose()
    }

    fn rebind_model(&self, generation: u64, profile_digest: &str) -> Result<(), StorageError> {
        let generation = i64::try_from(generation).map_err(storage)?;
        let generation_u64 = u64::try_from(generation).map_err(storage)?;
        let current = self.head()?;
        if self.model_profile()?.is_some_and(|(current, profile)|
            current == generation_u64 && profile.as_str() == profile_digest) {
            return Ok(());
        }
        let mut root = CommittedWindow::genesis();
        root.base = Some(WindowBase {
            source_window: current.id, leaf_seq: 0,
            model_generation: current.model_generation, policy_epoch: 0,
        });
        root.generation = current.generation + 1;
        root.model_generation = u64::try_from(generation).map_err(storage)?;
        match self.store.rebind_context_model_generation(self.scope_id, current.id,
            i64::try_from(current.model_generation).map_err(storage)?, generation, profile_digest,
            &Self::encode(&root)?).map_err(storage)? {
            ContextWindowAppend::Appended => Ok(()),
            ContextWindowAppend::HeadChanged => Err(storage("context head changed during model rebind")),
            ContextWindowAppend::ModelGenerationChanged { current } => Err(storage(format!("context model generation changed to {current} during rebind"))),
        }
    }
}

#[cfg(test)]
mod meta_page_tests {
    use super::*;
    use crate::WindowEntry;

    #[test]
    fn meta_page_bounds_cost_and_preserves_identity_without_payloads() {
        let dir = std::env::temp_dir().join(format!("shenko-meta-page-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let log = StoreWindowLog::new(store, EntityId::new());

        let genesis = log.head().unwrap();
        let mut window = CommittedWindow::genesis();
        window.id = EntityId::new();
        window.generation = genesis.generation + 1;
        window.base = Some(WindowBase {
            source_window: genesis.id,
            leaf_seq: 7,
            model_generation: genesis.model_generation,
            policy_epoch: 0,
        });
        window.policy_id = Some("snapcompact".into());
        window.entries.push(WindowEntry::Image { from: 1, to: 9, derivative: 0, tokens: 7 });
        window.derivatives.push(Derivative::new("image/png", vec![1, 2, 3]));
        window.call_states.insert("call-1".into(), vec![(3, CallStatus::Unknown)]);
        window.cost.window_tokens = 7;
        assert_eq!(log.append(genesis.id, 0, &window).unwrap(), WindowAppend::Appended);

        // First page: only the newest window, with a truthful older cursor.
        let page = log.meta_page(None, 1).unwrap();
        assert_eq!(page.total, 2);
        assert!(page.has_older);
        let [meta] = page.windows.as_slice() else { panic!("one window per page") };
        assert_eq!(meta.window.id, window.id);
        assert_eq!(meta.window.policy_id.as_deref(), Some("snapcompact"));
        assert_eq!(meta.window.call_states, window.call_states);
        assert_eq!(meta.window.cost.window_tokens, 7);
        // The committed body never carries payloads: identity without bytes.
        assert!(meta.window.derivatives.is_empty());
        let [frame] = meta.derivatives.as_slice() else { panic!("one derivative") };
        assert_eq!(frame.blake3, window.derivatives[0].blake3);
        assert_eq!(frame.bytes, 3);

        // Cursor below the page's oldest generation reaches only genesis.
        let rest = log.meta_page(Some(meta.window.generation), 8).unwrap();
        assert_eq!(rest.total, 2);
        assert!(!rest.has_older);
        let [oldest] = rest.windows.as_slice() else { panic!("one older window") };
        assert_eq!(oldest.window.id, genesis.id);
        assert!(oldest.derivatives.is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }
}
