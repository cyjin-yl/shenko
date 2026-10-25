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
