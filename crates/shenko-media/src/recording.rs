//! Recording audio capture with exact CAS closure — SPEC §20.
//!
//! [`MediaAudioRecorder`] accumulates the mono PCM16 little-endian frames a
//! session receives while a recording is `Recording`, wraps each segment in a
//! 44-byte canonical RIFF/WAVE header, and seals it in this ledger's CAS
//! under the `media-audio` namespace with codec `wav-pcm16` major 1. The
//! sealed [`ObjectRef`] becomes the segment-final transcript revision's
//! audio, so the subject reads the same bytes the provider heard — provider
//! handles never enter durable audio identity.
//!
//! One segment is bounded to 16 MiB PCM (~8.5 minutes at 16 kHz); when the
//! bound is hit the segment is sealed early and `truncated` is recorded in
//! the revision's `provider_facts` so no audio is silently lost.

use shenko_store::cas::ContentStore;
use shenko_types::error::ShenkoError;
use shenko_types::media::MediaStreamDescriptor;
use shenko_types::object::ObjectRef;

/// Bound on one sealed recording segment's PCM payload.
pub const SEGMENT_PCM_BOUND: usize = 16 * 1024 * 1024;

/// One sealed segment of recorded audio.
#[derive(Debug, Clone)]
pub struct RecordedSegment {
    pub object: ObjectRef,
    /// PCM payload byte length (excluding the WAVE header).
    pub pcm_bytes: usize,
    pub duration_ms: u64,
    /// The 16 MiB segment bound was hit; the segment was sealed early and the
    /// following audio starts a new segment.
    pub truncated: bool,
}

/// PCM16 capture for one live recording. Not `Sync`-shared: each session has
/// its own recorder and only the session's audio pump touches it.
pub struct MediaAudioRecorder {
    stream: MediaStreamDescriptor,
    sample_rate: u32,
    pcm: Vec<u8>,
    truncated: bool,
}

impl MediaAudioRecorder {
    pub fn new(stream: MediaStreamDescriptor, sample_rate: u32) -> Self {
        Self { stream, sample_rate, pcm: Vec::new(), truncated: false }
    }

    pub fn stream(&self) -> &MediaStreamDescriptor {
        &self.stream
    }

    pub fn is_empty(&self) -> bool {
        self.pcm.is_empty()
    }

    /// Accumulate one chunk of mono PCM16 little-endian audio.
    pub fn push(&mut self, chunk: &[u8]) {
        let room = SEGMENT_PCM_BOUND.saturating_sub(self.pcm.len());
        let take = room.min(chunk.len());
        self.pcm.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            self.truncated = true;
        }
    }

    /// Seal the accumulated PCM as one canonical WAVE segment and reset the
    /// capture. An empty recorder seals nothing.
    pub fn seal_segment(&mut self, cas: &ContentStore) -> Result<Option<RecordedSegment>, ShenkoError> {
        if self.pcm.is_empty() {
            return Ok(None);
        }
        let mut wav = Vec::with_capacity(44 + self.pcm.len());
        write_wav_header(&mut wav, self.sample_rate, self.pcm.len());
        wav.extend_from_slice(&self.pcm);
        let pcm_bytes = self.pcm.len();
        let truncated = self.truncated;
        let sample_rate = self.sample_rate;
        let sealed = cas.seal_object(&wav, "audio/wav", "media-audio", "wav-pcm16", 1)?;
        self.pcm.clear();
        self.truncated = false;
        Ok(Some(RecordedSegment {
            object: sealed.object,
            pcm_bytes,
            duration_ms: (pcm_bytes as u64).saturating_mul(1000)
                / (u64::from(sample_rate).saturating_mul(2)),
            truncated,
        }))
    }
}

/// Canonical 44-byte PCM16 mono RIFF/WAVE header; chunk sizes little-endian.
fn write_wav_header(out: &mut Vec<u8>, sample_rate: u32, data_bytes: usize) {
    let data_bytes_u32 = data_bytes as u32;
    let byte_rate = sample_rate.saturating_mul(2);
    let block_align = 2u16;
    let bits_per_sample = 16u16;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.wrapping_add(data_bytes_u32)).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format tag
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes_u32.to_le_bytes());
}
