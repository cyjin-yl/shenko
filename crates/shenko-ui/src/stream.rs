//! Durable stream sink — docs/specs/web-interface.md#spec-21-11 流数据与显示状态, docs/specs/web-interface.md#spec-21-9.
//!
//! Real streaming starts at the provider stream (ArchBroker), is redacted to
//! public text by the subject (`public_text_prefix`), and lands here. Each
//! chunk is persisted with `Store::append_output_part` BEFORE it is broadcast
//! to browsers, so "durable" always precedes "visible" and a reconnect resumes
//! from a durable cursor instead of a guessed snapshot.
//!
//! Producer boundary: whoever owns the provider stream (bin/shenko wiring once
//! ArchBroker lands `InferenceStreamEvent`) calls [`StreamSink::publish`]. The
//! publish path stays non-blocking (bounded channel try_send) so a slow or
//! absent consumer never backpressures the subject.
//!
//! Honesty: when no producer is wired, no chunk path exists at all. The UI
//! then reports `streaming: false` and serves committed-only replay — never a
//! client-side typewriter animation and never whole-S1 split into fake chunks.

use std::sync::Arc;

use serde::Serialize;
use shenko_store::Store;
use shenko_store::output_parts::{OutputPart, OutputPartAdmission, StoredOutputPart};
use shenko_types::ids::EntityId;

/// One stream chunk as broadcast to browsers. `commit_ref` binds the chunk to
/// its ledger commit; `step` is the head step number at publish time so the
/// client can order stream traffic against committed steps without treating
/// either as the other's identity.
#[derive(Debug, Clone, Serialize)]
pub struct StreamChunkDto {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub role: &'static str,
    pub content: String,
    pub message_id: String,
    pub part_id: String,
    pub part_revision: String,
    pub chunk_seq: u64,
    pub terminal: bool,
    pub committed_only: bool,
    pub observed_at: String,
    pub step: u64,
    pub stream_id: String,
    pub request_id: String,
    pub attempt_id: String,
    pub commit_ref: String,
    /// Local replay position, separate from the immutable source commit.
    pub commit_seq: u64,
}

impl From<StoredOutputPart> for StreamChunkDto {
    /// Replay-path DTO: the stored part already carries every identity and
    /// the durable commit; `step` stays 0 because a pre-S1 part has no
    /// committed step number and the client must not invent one.
    fn from(stored: StoredOutputPart) -> Self {
        let part = stored.part;
        StreamChunkDto {
            schema_version: 2,
            kind: "stream_part",
            role: "subject",
            content: part.content,
            message_id: part.message_id.to_string(),
            part_id: part.part_id,
            part_revision: part.part_revision,
            chunk_seq: part.chunk_seq,
            terminal: part.terminal,
            committed_only: true,
            observed_at: chrono::Utc::now().to_rfc3339(),
            step: 0,
            stream_id: part.stream_id.to_string(),
            request_id: part.request_id.to_string(),
            attempt_id: part.attempt_id.to_string(),
            commit_ref: stored.commit.to_string(),
            commit_seq: stored.commit_seq,
        }
    }
}

pub struct StreamSink {
    store: Arc<Store>,
    subject_id: EntityId,
    tx: tokio::sync::broadcast::Sender<StreamChunkDto>,
}

impl StreamSink {
    pub fn new(store: Arc<Store>, subject_id: EntityId, capacity: usize) -> Arc<Self> {
        let (tx, _) = tokio::sync::broadcast::channel(capacity.max(16));
        Arc::new(Self {
            store,
            subject_id,
            tx,
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StreamChunkDto> {
        self.tx.subscribe()
    }

    /// Persist one ordered public-text chunk, then broadcast it. Replays do
    /// not rebroadcast (clients already applied the original or will receive
    /// it through the durable cursor). Conflicts never overwrite.
    pub fn publish(&self, part: &OutputPart) -> anyhow::Result<Option<shenko_types::commit::CommitRef>> {
        let step = self
            .store
            .load_subject(self.subject_id)
            .ok()
            .map(|(_, step)| step.step_no)
            .unwrap_or(0);
        match self.store.append_output_part(self.subject_id, part)? {
            OutputPartAdmission::Appended(commit) => {
                let _ = self.tx.send(self.dto(part, step, &commit));
                Ok(Some(commit))
            }
            OutputPartAdmission::Replayed(commit) => Ok(Some(commit)),
            OutputPartAdmission::Conflict => {
                Err(anyhow::anyhow!("output part conflict: same chunk_seq with different content"))
            }
        }
    }

    fn dto(
        &self,
        part: &OutputPart,
        step: u64,
        commit: &shenko_types::commit::CommitRef,
    ) -> StreamChunkDto {
        StreamChunkDto {
            schema_version: 2,
            kind: "stream_part",
            role: "subject",
            content: part.content.clone(),
            message_id: part.message_id.to_string(),
            part_id: part.part_id.clone(),
            part_revision: part.part_revision.clone(),
            chunk_seq: part.chunk_seq,
            terminal: part.terminal,
            // Durable before visible: this chunk is a committed ledger fact.
            committed_only: true,
            observed_at: chrono::Utc::now().to_rfc3339(),
            step,
            stream_id: part.stream_id.to_string(),
            request_id: part.request_id.to_string(),
            attempt_id: part.attempt_id.to_string(),
            commit_ref: commit.to_string(),
            commit_seq: commit.seq.0,
        }
    }

    /// Durable resume for a reconnecting client. `after_commit_seq` is the
    /// last commit the client durably applied (0 = from the beginning).
    /// Holes in chunk_seq are returned as-is; the client buffers and waits
    /// for the missing seq instead of patching text (docs/specs/web-interface.md#spec-21-11).
    pub fn parts_after(
        &self,
        after_commit_seq: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredOutputPart>> {
        Ok(self
            .store
            .output_parts_after(self.subject_id, after_commit_seq, limit)?)
    }
}

/// The reader surface of a provider stream — docs/specs/web-interface.md#spec-21-9, docs/specs/web-interface.md#spec-57-5.
///
/// The broker publishes the public text of a stream here; every chunk is
/// written to the store BEFORE it is broadcast, so "durable" always precedes
/// "visible" and a reconnecting client resumes from a durable cursor instead
/// of a guessed snapshot.
///
/// The part identity is the one the committed projection also uses:
/// `message_id` is the step id and `part_id` is `text/0`. A client that
/// watched the stream therefore absorbs the committed message as the S1
/// binding instead of appending the same text twice (docs/specs/web-interface.md#spec-21-8, docs/specs/web-interface.md#spec-21-9).
pub struct UiPublicStreamSink {
    sink: Arc<StreamSink>,
}

impl UiPublicStreamSink {
    pub fn new(sink: Arc<StreamSink>) -> Self {
        Self { sink }
    }
}

impl shenko_broker::stream::PublicStreamSink for UiPublicStreamSink {
    fn publish_text(
        &self,
        chunk: &shenko_broker::stream::PublicTextChunk,
    ) -> Result<(), shenko_broker::stream::StreamSinkError> {
        let message_id: shenko_types::ids::EntityId = chunk.message_id.parse().map_err(|error| {
            shenko_broker::stream::StreamSinkError::Publish(format!(
                "stream chunk names no usable message identity: {error}"
            ))
        })?;
        let part = shenko_store::output_parts::OutputPart {
            stream_id: chunk.stream_id,
            request_id: chunk.request_id,
            attempt_id: chunk.attempt_id,
            message_id,
            part_id: chunk.part_id.clone(),
            part_revision: "r1".to_string(),
            chunk_seq: chunk.chunk_seq,
            content: chunk.text.clone(),
            kind: shenko_store::output_parts::OutputPartKind::PublicText,
            // The committed decision carries the terminal truth; a streamed
            // chunk is not the end of the story.
            terminal: false,
        };
        self.sink
            .publish(&part)
            .map_err(|error| shenko_broker::stream::StreamSinkError::Publish(format!("{error:#}")))?;
        Ok(())
    }

    fn publish_terminal(
        &self,
        _request_id: shenko_types::ids::EntityId,
        _terminal: &shenko_broker::request::TerminationEvidence,
    ) -> Result<(), shenko_broker::stream::StreamSinkError> {
        // The terminal evidence belongs to the durable journal; the reader
        // surface learns that the step was committed from the committed lane,
        // which is the only place a terminal claim is true.
        Ok(())
    }
}
