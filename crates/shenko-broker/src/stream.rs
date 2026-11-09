//! Provider streaming, public-text gating and bounded durable batches.
//!
//! Real streaming starts at the provider's own stream. This module parses the
//! provider wire, keeps the public content separate from internal control and
//! reasoning, and publishes ordered bounded batches.
//!
//! Two rules shape the projection:
//!
//! * Control syntax is gated incrementally. The subject's control actions are
//!   lines at column zero; a line that could become one is withheld until it
//!   is complete and only then classified. A partial control line and raw
//!   reasoning never reach the public stream or the durable journal.
//! * The terminal receipt is independent of the caller. A subject that
//!   disappears, is cancelled or never reads its channel does not change
//!   whether the terminal evidence is recorded.
//!
//! Batches are bounded by 100 ms or 4 KiB of serialized payload, which is the
//! same bound the durable journal enforces on one unit.

use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::request::{InferenceResponse, ProviderBinding, TerminationEvidence, TokenUsage};

/// What a streamed call produced.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamOutcome {
    /// The assembled response of the stream.
    pub response: InferenceResponse,
    /// Provider events observed on the wire.
    pub events: u64,
    /// Batches handed to the durable journal.
    pub batches: u64,
}

/// Flush a pending batch after this many milliseconds.
pub const STREAM_FLUSH_INTERVAL_MS: u64 = 100;
/// Maximum serialized size of one published batch.
pub const STREAM_MAX_BATCH_BYTES: usize = 4096;

/// Whether an event may ever be shown to a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamVisibility {
    /// Candidate public content. It still passes the control gate.
    Public,
    /// Internal control, reasoning or metadata. It is never published.
    Internal,
}

/// One event of a provider stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A chunk of model text.
    TextDelta { text: String },
    /// A tool call started.
    ToolUseStart { index: u32, id: String, name: String },
    /// A chunk of a tool call's arguments.
    ToolUseDelta { index: u32, partial_json: String },
    /// A tool call ended.
    ToolUseStop { index: u32 },
    /// Model reasoning. It is internal: it is never published and never
    /// written to the durable journal.
    ReasoningDelta { text: String },
    /// Token counters the provider reported.
    Usage { usage: TokenUsage },
    /// The provider's stop reason.
    MessageStop { stop_reason: String },
    /// A provider-side notice that does not change the outcome.
    Warning { code: String, detail: Option<String> },
}

impl StreamEvent {
    pub fn visibility(&self) -> StreamVisibility {
        match self {
            Self::TextDelta { .. } => StreamVisibility::Public,
            Self::ToolUseStart { .. }
            | Self::ToolUseDelta { .. }
            | Self::ToolUseStop { .. }
            | Self::ReasoningDelta { .. }
            | Self::Usage { .. }
            | Self::MessageStop { .. }
            | Self::Warning { .. } => StreamVisibility::Internal,
        }
    }

    /// Whether this event may be written to the durable journal. Reasoning is
    /// excluded: the journal keeps the public projection and the terminal
    /// evidence, not raw chain of thought.
    pub fn journalable(&self) -> bool {
        !matches!(self, Self::ReasoningDelta { .. })
    }
}

/// One ordered batch of stream events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamBatch {
    pub sequence: u64,
    pub request_id: EntityId,
    pub events: Vec<StreamEvent>,
    /// Serialized size of this batch, bounded by [`STREAM_MAX_BATCH_BYTES`].
    pub bytes: usize,
    pub first_at_ms: i64,
    pub last_at_ms: i64,
}

/// One public text chunk for the reader surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicTextChunk {
    pub request_id: EntityId,
    /// The message identity allocated when the request was armed.
    pub message_id: String,
    /// The part identity inside that message.
    pub part_id: String,
    /// The durable stream the chunk belongs to. SPEC §21.11: the envelope
    /// carries a stable stream identity through streaming, completion,
    /// failure, cancellation and reconnect.
    pub stream_id: EntityId,
    /// The attempt that owns the request. A new attempt is a new revision,
    /// never a rewrite of the previous one.
    pub attempt_id: EntityId,
    /// Contiguous chunk number starting at zero.
    pub chunk_seq: u64,
    pub text: String,
}

/// Why a stream write failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamSinkError {
    #[error("stream journal write failed: {0}")]
    Journal(String),
    #[error("public stream publish failed: {0}")]
    Publish(String),
}

/// The durable journal of a provider stream.
///
/// The journal is opened when the request is armed, receives ordered batches
/// and a terminal unit, and is closed afterwards. A terminal unit exists even
/// when no batch was ever published.
pub trait StreamJournalSink: Send + Sync {
    fn open_stream(&self, request_id: EntityId, binding: &ProviderBinding) -> Result<(), StreamSinkError>;
    fn append_batch(&self, request_id: EntityId, batch: &StreamBatch) -> Result<(), StreamSinkError>;
    fn append_terminal(
        &self,
        request_id: EntityId,
        terminal: &TerminationEvidence,
    ) -> Result<(), StreamSinkError>;
    fn close_stream(&self, request_id: EntityId) -> Result<(), StreamSinkError>;
}

/// The public surface a reader consumes.
pub trait PublicStreamSink: Send + Sync {
    fn publish_text(&self, chunk: &PublicTextChunk) -> Result<(), StreamSinkError>;
    fn publish_terminal(
        &self,
        request_id: EntityId,
        terminal: &TerminationEvidence,
    ) -> Result<(), StreamSinkError>;
}

/// The identity a streamed request publishes under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamIdentity {
    /// The message identity allocated when the request was armed.
    pub message_id: String,
    /// The part identity inside that message.
    pub part_id: String,
    /// The durable stream the part belongs to.
    pub stream_id: EntityId,
    /// The attempt that owns the request.
    pub attempt_id: EntityId,
}

impl Default for StreamIdentity {
    fn default() -> Self {
        Self {
            message_id: String::new(),
            part_id: "text/0".into(),
            stream_id: EntityId::nil(),
            attempt_id: EntityId::nil(),
        }
    }
}

/// The incremental gate that keeps control syntax out of the public stream.
///
/// A control action is a line at column zero. A line that starts with `[` (or
/// with the `dispatch:` prefix) is withheld until the line is complete, then
/// classified with the same rule the subject's parser uses. Prose is
/// published as it arrives; a control line is dropped from the public stream
/// and remains in the original provider evidence for the subject to parse.
pub struct ControlGate {
    pending: String,
    in_fence: bool,
}

impl Default for ControlGate {
    fn default() -> Self {
        Self { pending: String::new(), in_fence: false }
    }
}

impl ControlGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a text delta and receive the publishable pieces.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.pending.push_str(delta);
        let mut published = Vec::new();
        while let Some(index) = self.pending.find('\n') {
            // The drained line keeps its own newline; it is the line break
            // that ended it.
            let line: String = self.pending.drain(..=index).collect();
            if let Some(text) = self.classify_line(&line) {
                published.push(text);
            }
        }
        if !self.pending.is_empty() && !self.may_be_control(&self.pending) {
            published.push(std::mem::take(&mut self.pending));
        }
        published
    }

    /// The remaining incomplete line at the end of a stream.
    pub fn finish(&mut self) -> Vec<String> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.pending);
        match self.classify_line(&line) {
            Some(text) => vec![text],
            None => Vec::new(),
        }
    }

    /// Whether the incomplete line could still become a control line.
    fn may_be_control(&self, pending: &str) -> bool {
        if self.in_fence {
            return false;
        }
        pending.starts_with('[') || "dispatch:".starts_with(pending)
    }

    /// Classify one complete line. `None` means the line is a control action
    /// and must not be published.
    fn classify_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if self.in_fence {
            // Inside a fenced block every line is literal prose.
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                self.in_fence = false;
            }
            return Some(line.to_string());
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            self.in_fence = true;
            return Some(line.to_string());
        }
        if is_control_line(trimmed) {
            return None;
        }
        Some(line.to_string())
    }
}

/// Whether a complete line is a control action under the subject's output
/// contract: column zero, not indented, and either a bracketed tag with an
/// ASCII alphanumeric header or the `dispatch:` prefix. A markdown reference
/// link is prose.
pub fn is_control_line(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    if line.starts_with("dispatch:") {
        return true;
    }
    let tag = line.strip_prefix('[').and_then(|value| value.split_once(']').map(|(header, _)| header));
    let markdown_reference = line
        .split_once(']')
        .is_some_and(|(_, suffix)| suffix.starts_with(['(', '[', ':']));
    if markdown_reference {
        return false;
    }
    match tag {
        Some(header) => header
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
            && header.bytes().all(|byte| byte.is_ascii_alphanumeric() || b":_-.".contains(&byte)),
        None => line
            .strip_prefix('[')
            .is_some_and(|value| value.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase())),
    }
}

/// The state of one publisher.
struct PublisherState {
    sequence: u64,
    chunk_seq: u64,
    pending: Vec<StreamEvent>,
    pending_bytes: usize,
    first_at_ms: i64,
    last_at_ms: i64,
    gate: ControlGate,
    opened: bool,
    terminal_sent: bool,
}

/// Publishes ordered bounded batches to a durable journal and to the public
/// surface.
pub struct BoundedStreamPublisher {
    request_id: EntityId,
    identity: StreamIdentity,
    journal: Option<Arc<dyn StreamJournalSink>>,
    public: Option<Arc<dyn PublicStreamSink>>,
    state: Mutex<PublisherState>,
}

impl BoundedStreamPublisher {
    pub fn new(
        request_id: EntityId,
        identity: StreamIdentity,
        journal: Option<Arc<dyn StreamJournalSink>>,
        public: Option<Arc<dyn PublicStreamSink>>,
    ) -> Self {
        Self {
            request_id,
            identity,
            journal,
            public,
            state: Mutex::new(PublisherState {
                sequence: 0,
                chunk_seq: 0,
                pending: Vec::new(),
                pending_bytes: 0,
                first_at_ms: 0,
                last_at_ms: 0,
                gate: ControlGate::new(),
                opened: false,
                terminal_sent: false,
            }),
        }
    }

    /// Whether a durable journal is installed. Without one, a stream is not
    /// offered: content is never dropped silently.
    pub fn has_journal(&self) -> bool {
        self.journal.is_some()
    }

    /// How many public text chunks were handed to the reader surface.
    pub fn published_chunks(&self) -> u64 {
        self.state.lock().expect("stream publisher state poisoned").chunk_seq
    }

    /// Open the durable stream. It happens once, before the first event.
    pub fn open(&self, binding: &ProviderBinding, now_ms: i64) -> Result<(), StreamSinkError> {
        let mut state = self.state.lock().expect("stream publisher state poisoned");
        if state.opened {
            return Ok(());
        }
        if let Some(journal) = &self.journal {
            journal.open_stream(self.request_id, binding)?;
        }
        state.opened = true;
        state.first_at_ms = now_ms;
        state.last_at_ms = now_ms;
        Ok(())
    }

    /// Push one provider event.
    pub fn push(&self, event: StreamEvent, now_ms: i64) -> Result<(), StreamSinkError> {
        let mut state = self.state.lock().expect("stream publisher state poisoned");
        if state.terminal_sent {
            return Ok(());
        }
        if state.pending.is_empty() {
            state.first_at_ms = now_ms;
        }
        state.last_at_ms = now_ms;
        if let StreamEvent::TextDelta { text } = &event {
            let pieces = state.gate.push(text);
            let chunk_seq = state.chunk_seq;
            let mut published = Vec::with_capacity(pieces.len());
            for (offset, piece) in pieces.into_iter().enumerate() {
                published.push(PublicTextChunk {
                    request_id: self.request_id,
                    message_id: self.identity.message_id.clone(),
                    part_id: self.identity.part_id.clone(),
                    stream_id: self.identity.stream_id,
                    attempt_id: self.identity.attempt_id,
                    chunk_seq: chunk_seq + offset as u64,
                    text: piece,
                });
            }
            state.chunk_seq += published.len() as u64;
            if let Some(public) = &self.public {
                for chunk in &published {
                    public.publish_text(chunk)?;
                }
            }
        }
        if event.journalable() {
            let size = serde_json::to_string(&event).map(|json| json.len()).unwrap_or(0);
            state.pending_bytes += size;
            state.pending.push(event);
        }
        let elapsed = now_ms.saturating_sub(state.first_at_ms);
        if state.pending_bytes >= STREAM_MAX_BATCH_BYTES || elapsed >= STREAM_FLUSH_INTERVAL_MS as i64 {
            drop(state);
            self.flush(now_ms)?;
        }
        Ok(())
    }

    /// Publish the pending batch, respecting the serialized size bound.
    pub fn flush(&self, now_ms: i64) -> Result<(), StreamSinkError> {
        let mut state = self.state.lock().expect("stream publisher state poisoned");
        if state.pending.is_empty() {
            return Ok(());
        }
        let events = std::mem::take(&mut state.pending);
        state.pending_bytes = 0;
        let first_at = state.first_at_ms;
        let last_at = now_ms.max(state.last_at_ms);
        let mut sequence = state.sequence;
        let mut current: Vec<StreamEvent> = Vec::new();
        let mut batches: Vec<StreamBatch> = Vec::new();
        for event in events {
            for piece in split_event(event) {
                current.push(piece);
                let probe = make_batch(sequence, self.request_id, &current, first_at, last_at);
                let size = serde_json::to_string(&probe).map(|json| json.len()).unwrap_or(usize::MAX);
                if size > STREAM_MAX_BATCH_BYTES && current.len() > 1 {
                    let popped = current.pop().expect("batch checked non-empty");
                    batches.push(make_batch(sequence, self.request_id, &current, first_at, last_at));
                    sequence += 1;
                    current = vec![popped];
                }
            }
        }
        if !current.is_empty() {
            batches.push(make_batch(sequence, self.request_id, &current, first_at, last_at));
            sequence += 1;
        }
        state.sequence = sequence;
        state.first_at_ms = now_ms;
        drop(state);
        if let Some(journal) = &self.journal {
            for batch in &batches {
                journal.append_batch(self.request_id, batch)?;
            }
        }
        Ok(())
    }

    /// Publish the terminal evidence. It is independent of the caller: a
    /// dropped receiver does not cancel this write.
    pub fn finish(
        &self,
        terminal: &TerminationEvidence,
        now_ms: i64,
    ) -> Result<(), StreamSinkError> {
        self.flush(now_ms)?;
        let mut state = self.state.lock().expect("stream publisher state poisoned");
        if state.terminal_sent {
            return Ok(());
        }
        state.terminal_sent = true;
        drop(state);
        if let Some(journal) = &self.journal {
            journal.append_terminal(self.request_id, terminal)?;
            journal.close_stream(self.request_id)?;
        }
        if let Some(public) = &self.public {
            public.publish_terminal(self.request_id, terminal)?;
        }
        Ok(())
    }
}

/// Build one batch envelope.
fn make_batch(
    sequence: u64,
    request_id: EntityId,
    events: &[StreamEvent],
    first_at_ms: i64,
    last_at_ms: i64,
) -> StreamBatch {
    StreamBatch {
        sequence,
        request_id,
        events: events.to_vec(),
        bytes: 0,
        first_at_ms,
        last_at_ms,
    }
}

/// Split one oversized event into pieces that fit the batch bound. A text
/// delta is split on a character boundary so no UTF-8 sequence is broken.
fn split_event(event: StreamEvent) -> Vec<StreamEvent> {
    let size = serde_json::to_string(&event).map(|json| json.len()).unwrap_or(0);
    if size <= STREAM_MAX_BATCH_BYTES {
        return vec![event];
    }
    match event {
        StreamEvent::TextDelta { text } => {
            let mut pieces = Vec::new();
            let mut rest = text;
            while rest.len() > 1024 {
                let middle = rest.len() / 2;
                let boundary = rest
                    .char_indices()
                    .map(|(index, _)| index)
                    .take_while(|index| *index <= middle)
                    .last()
                    .unwrap_or(0);
                let (head, tail) = rest.split_at(boundary);
                pieces.push(StreamEvent::TextDelta { text: head.to_string() });
                rest = tail.to_string();
            }
            pieces.push(StreamEvent::TextDelta { text: rest });
            pieces
        }
        other => vec![other],
    }
}

/// One decoded server-sent-events frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// The incremental decoder of a `text/event-stream` body.
#[derive(Debug, Default)]
pub struct SseDecoder {
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line. A frame is complete when a blank line arrives. The
    /// callers split the body on `\n` and keep the terminator, so a line may
    /// end in `\n` or `\r\n`; the line ending is stripped before the
    /// boundary test. A separator that kept its `\n` is still blank.
    pub fn feed_line(&mut self, line: &str) -> Option<SseFrame> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if self.data.is_empty() && self.event.is_none() {
                return None;
            }
            let frame = SseFrame {
                event: self.event.take(),
                data: self.data.join("\n"),
            };
            self.data.clear();
            return Some(frame);
        }
        if let Some(value) = line.strip_prefix("event:") {
            self.event = Some(value.trim().to_string());
            return None;
        }
        if let Some(value) = line.strip_prefix("data:") {
            self.data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
        None
    }
}

/// Why a provider stream frame could not be interpreted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamParseError {
    #[error("provider stream frame is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("provider stream reported an error: {0}")]
    ProviderError(String),
}

/// Interpret one Anthropic Messages stream frame.
pub fn parse_anthropic_frame(frame: &SseFrame) -> Result<Vec<StreamEvent>, StreamParseError> {
    let name = frame.event.as_deref().unwrap_or_default();
    if name == "ping" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(&frame.data)
        .map_err(|error| StreamParseError::InvalidJson(error.to_string()))?;
    match name {
        "message_start" => {
            let usage = value
                .pointer("/message/usage")
                .map(|usage| TokenUsage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    cache_read_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                    cache_write_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()),
                })
                .unwrap_or(TokenUsage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                });
            Ok(vec![StreamEvent::Usage { usage }])
        }
        "content_block_start" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let block = value.get("content_block");
            let kind = block.and_then(|block| block.get("type")).and_then(|v| v.as_str());
            match kind {
                Some("tool_use") => Ok(vec![StreamEvent::ToolUseStart {
                    index,
                    id: block
                        .and_then(|block| block.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .and_then(|block| block.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }]),
                _ => Ok(Vec::new()),
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let delta = value.get("delta");
            let kind = delta.and_then(|delta| delta.get("type")).and_then(|v| v.as_str());
            match kind {
                Some("text_delta") => Ok(vec![StreamEvent::TextDelta {
                    text: delta
                        .and_then(|delta| delta.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }]),
                Some("input_json_delta") => Ok(vec![StreamEvent::ToolUseDelta {
                    index,
                    partial_json: delta
                        .and_then(|delta| delta.get("partial_json"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }]),
                Some("thinking_delta") => Ok(vec![StreamEvent::ReasoningDelta {
                    text: delta
                        .and_then(|delta| delta.get("thinking"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }]),
                _ => Ok(Vec::new()),
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            Ok(vec![StreamEvent::ToolUseStop { index }])
        }
        "message_delta" => {
            let stop_reason = value
                .pointer("/delta/stop_reason")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let mut events = Vec::new();
            if let Some(usage) = value.get("usage") {
                events.push(StreamEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                        cache_read_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
                        cache_write_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()),
                    },
                });
            }
            events.push(StreamEvent::MessageStop { stop_reason });
            Ok(events)
        }
        "message_stop" => Ok(vec![StreamEvent::MessageStop { stop_reason: String::new() }]),
        "error" => {
            let detail = value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("provider stream error")
                .to_string();
            Err(StreamParseError::ProviderError(detail))
        }
        _ => Ok(Vec::new()),
    }
}

/// Interpret one OpenAI-compatible stream frame.
pub fn parse_openai_frame(frame: &SseFrame) -> Result<Vec<StreamEvent>, StreamParseError> {
    if frame.data.trim() == "[DONE]" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_str(&frame.data)
        .map_err(|error| StreamParseError::InvalidJson(error.to_string()))?;
    let mut events = Vec::new();
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        events.push(StreamEvent::Usage {
            usage: TokenUsage {
                input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
                output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
                cache_read_tokens: usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(|v| v.as_u64()),
                cache_write_tokens: None,
            },
        });
    }
    let Some(choice) = value
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())
    else {
        return Ok(events);
    };
    if let Some(text) = choice.pointer("/delta/content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta { text: text.to_string() });
        }
    }
    if let Some(calls) = choice.pointer("/delta/tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            let index = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let id = call.get("id").and_then(|v| v.as_str());
            let name = call.pointer("/function/name").and_then(|v| v.as_str());
            if id.is_some() || name.is_some() {
                events.push(StreamEvent::ToolUseStart {
                    index,
                    id: id.unwrap_or_default().to_string(),
                    name: name.unwrap_or_default().to_string(),
                });
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                if !arguments.is_empty() {
                    events.push(StreamEvent::ToolUseDelta {
                        index,
                        partial_json: arguments.to_string(),
                    });
                }
            }
        }
    }
    if let Some(finish) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        events.push(StreamEvent::MessageStop { stop_reason: finish.to_string() });
    }
    Ok(events)
}
