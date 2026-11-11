//! The durable journal of a subject's provider stream — docs/specs/durable-state.md#content-stream-journal, docs/specs/web-interface.md#spec-57-5.
//!
//! A streamed response must survive a crash or a UI reconnect: every unit the
//! transport delivered is journaled in the ledger, bound to the durable source
//! identity of the work (request) rather than to an in-memory callback. The
//! store already owns that journal (docs/specs/durable-state.md#content-stream-journal content streams); this adapter is
//! the bridge between the broker's stream contract and the store's own
//! invariants:
//!
//! * the producer's flush boundary IS the durable unit boundary, so a
//!   reconnecting consumer re-reads whole batches;
//! * an identical unit under the same (stream, seq) is a no-op and a
//!   different unit under the same key is a typed conflict — never an
//!   overwrite;
//! * rows arriving after a terminal unit or an explicit close are refused;
//! * reasoning never enters the journal: the journal keeps the public
//!   projection and the terminal evidence, not a chain of thought.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use shenko_broker::request::{ProviderBinding, TerminationEvidence};
use shenko_broker::stream::{StreamBatch, StreamJournalSink, StreamSinkError};
use shenko_store::{Store, provider_stream_id};
use shenko_types::ids::EntityId;

/// The content kind of a journaled public-text unit.
const PUBLIC_TEXT_KIND: &str = "provider_text";
/// The content kind of the journaled terminal unit.
const TERMINAL_KIND: &str = "provider_terminal";
/// The stream kind of a provider stream.
const STREAM_KIND: &str = "provider_stream";
/// The durable bound of one journaled text unit, matching the store's own
/// chunk bound (docs/specs/durable-state.md#content-stream-journal) and the broker's batch bound.
const UNIT_MAX_BYTES: usize = 4096;

/// A store-backed journal of provider streams.
pub struct SubjectStreamJournal {
    store: Arc<Store>,
    agent_id: EntityId,
    /// The next unit sequence per request. The store refuses a unit whose
    /// sequence was already used with different content, so the counter is
    /// the producer's own ordering, not a guess.
    next_seq: Mutex<HashMap<EntityId, i64>>,
}

impl SubjectStreamJournal {
    pub fn new(store: Arc<Store>, agent_id: EntityId) -> Self {
        Self {
            store,
            agent_id,
            next_seq: Mutex::new(HashMap::new()),
        }
    }

    fn take_seq(&self, request_id: EntityId) -> i64 {
        let mut guard = self.next_seq.lock();
        let counter = guard.entry(request_id).or_insert(0);
        let seq = *counter;
        *counter += 1;
        seq
    }
}

impl StreamJournalSink for SubjectStreamJournal {
    fn open_stream(
        &self,
        request_id: EntityId,
        binding: &ProviderBinding,
    ) -> Result<(), StreamSinkError> {
        let stream_id = provider_stream_id(request_id);
        let binding_text = serde_json::to_string(binding)
            .map_err(|error| StreamSinkError::Journal(format!("binding is not serializable: {error}")))?;
        self.store
            .open_content_stream(&stream_id, self.agent_id, STREAM_KIND, &binding_text)
            .map_err(|error| StreamSinkError::Journal(format!("{error:#}")))?;
        self.next_seq.lock().entry(request_id).or_insert(0);
        Ok(())
    }

    fn append_batch(
        &self,
        request_id: EntityId,
        batch: &StreamBatch,
    ) -> Result<(), StreamSinkError> {
        // Only the public text is journaled: reasoning and control lines are
        // internal, and the journal is not a transcript of the model.
        let text: String = batch
            .events
            .iter()
            .filter_map(|event| match event {
                shenko_broker::stream::StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if text.is_empty() {
            return Ok(());
        }
        let stream_id = provider_stream_id(request_id);
        for piece in split_units(&text) {
            let seq = self.take_seq(request_id);
            self.store
                .journal_content_batch(&stream_id, seq, PUBLIC_TEXT_KIND, &piece)
                .map_err(|error| StreamSinkError::Journal(format!("{error:#}")))?;
        }
        Ok(())
    }

    fn append_terminal(
        &self,
        request_id: EntityId,
        terminal: &TerminationEvidence,
    ) -> Result<(), StreamSinkError> {
        let stream_id = provider_stream_id(request_id);
        let seq = self.take_seq(request_id);
        let data = serde_json::to_string(terminal)
            .map_err(|error| StreamSinkError::Journal(format!("terminal is not serializable: {error}")))?;
        self.store
            .journal_content_terminal(&stream_id, seq, TERMINAL_KIND, &data)
            .map_err(|error| StreamSinkError::Journal(format!("{error:#}")))?;
        Ok(())
    }

    fn close_stream(&self, request_id: EntityId) -> Result<(), StreamSinkError> {
        let stream_id = provider_stream_id(request_id);
        self.store
            .close_content_stream(&stream_id)
            .map_err(|error| StreamSinkError::Journal(format!("{error:#}")))?;
        self.next_seq.lock().remove(&request_id);
        Ok(())
    }
}

/// Split one text into journalable units of at most [`UNIT_MAX_BYTES`].
///
/// The split happens on a character boundary, so a unit never contains half a
/// character; the sequence is what orders the units, not the split points.
fn split_units(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        let width = character.len_utf8();
        if current.len() + width > UNIT_MAX_BYTES && !current.is_empty() {
            units.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_are_bounded_and_ordered() {
        let text = "x".repeat(UNIT_MAX_BYTES * 2 + 17);
        let units = split_units(&text);
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|unit| unit.len() <= UNIT_MAX_BYTES));
        assert_eq!(units.concat(), text);
    }

    #[test]
    fn a_multibyte_character_is_never_split() {
        let text = "深".repeat(UNIT_MAX_BYTES);
        let units = split_units(&text);
        assert!(units.iter().all(|unit| unit.len() <= UNIT_MAX_BYTES));
        assert_eq!(units.concat(), text);
    }
}
