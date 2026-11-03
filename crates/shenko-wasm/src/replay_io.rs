//! Ordered host-owned input receipts. Replay never invokes a fresh supplier.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs::{File, OpenOptions}, io::{Read, Write}, path::Path, sync::{Arc, Mutex}};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayBinding {
    pub module_blake3: String,
    pub abi: String,
    pub environment_generation: String,
    pub source_blake3: String,
    pub invocation_blake3: String,
    pub cell: String,
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    sequence: u64,
    previous: String,
    operation: String,
    request_blake3: String,
    value_blake3: String,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoReceipt {
    pub binding_blake3: String,
    pub events: u64,
    pub chain_blake3: String,
}

struct State {
    module: String,
    abi: String,
    invocation: String,
    file: File,
    sequence: u64,
    chain: String,
    binding: String,
    finished: bool,
    failed: bool,
}

struct Inner {
    replay: bool,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct ReplayIo(Arc<Inner>);

fn write_frame(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.write_all(&(bytes.len() as u64).to_le_bytes())?;
    file.write_all(bytes)?;
    Ok(())
}
fn read_frame(file: &mut File, maximum: u64) -> Result<Vec<u8>> {
    let mut length = [0u8; 8];
    file.read_exact(&mut length).context("REPLAY_IO_MISSING: transcript frame")?;
    let length = u64::from_le_bytes(length);
    if length > maximum { bail!("REPLAY_IO_CORRUPT: frame exceeds expected bound"); }
    let mut value = vec![0; usize::try_from(length)?];
    file.read_exact(&mut value).context("REPLAY_IO_MISSING: truncated transcript")?;
    Ok(value)
}

impl ReplayIo {
    pub fn record(path: &Path, binding: &ReplayBinding) -> Result<Self> {
        let bytes = serde_json::to_vec(binding)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        write_frame(&mut file, &bytes)?;
        file.sync_all()?;
        File::open(path.parent().context("transcript has no parent")?)?.sync_all()?;
        Ok(Self::new(file, false, blake3::hash(&bytes).to_hex().to_string(), binding))
    }
    pub fn replay(path: &Path, binding: &ReplayBinding) -> Result<Self> {
        let mut file = File::open(path)?;
        let bytes = read_frame(&mut file, 64 * 1024)?;
        let recorded: ReplayBinding = serde_json::from_slice(&bytes)?;
        if &recorded != binding { bail!("REPLAY_IO_BINDING_MISMATCH"); }
        Ok(Self::new(file, true, blake3::hash(&bytes).to_hex().to_string(), binding))
    }
    fn new(file: File, replay: bool, binding: String, identity: &ReplayBinding) -> Self {
        Self(Arc::new(Inner { replay, state: Mutex::new(State { file, sequence: 0,
            module: identity.module_blake3.clone(), abi: identity.abi.clone(),
            invocation: identity.invocation_blake3.clone(),
            chain: binding.clone(), binding, finished: false, failed: false }) }))
    }
    pub fn is_replay(&self) -> bool {
        // A recording supplier can check its mode while input() holds the
        // ordering lock. Mode is immutable; querying it must not re-lock.
        self.0.replay || self.0.state.is_poisoned()
    }
    pub fn verify_execution(&self, module: &[u8], invocation: &str) -> Result<()> {
        let state = self.0.state.lock().map_err(|_| anyhow::anyhow!("REPLAY_IO_POISONED"))?;
        if state.module != blake3::hash(module).to_hex().as_str() || state.abi != abi_identity()
            || state.invocation != invocation {
            bail!("REPLAY_IO_BINDING_MISMATCH: executing module or host ABI");
        }
        Ok(())
    }
    /// `maximum` is the guest-requested maximum result size, not a guessed
    /// value. Supply errors poison recording; an incomplete receipt cannot
    /// later be sealed as a successful deterministic replay.
    pub fn input(&self, operation: &str, request: &[u8], maximum: u64,
        supply: impl FnOnce() -> Result<Vec<u8>>) -> Result<Vec<u8>> {
        let mut state = self.0.state.lock().map_err(|_| anyhow::anyhow!("REPLAY_IO_POISONED"))?;
        if state.finished || state.failed { bail!("REPLAY_IO_CLOSED"); }
        let result = (|| -> Result<Vec<u8>> {
            let request_blake3 = blake3::hash(request).to_hex().to_string();
            let (event, metadata, value) = if self.0.replay {
                let metadata = read_frame(&mut state.file, 64 * 1024)?;
                let event: Event = serde_json::from_slice(&metadata)?;
                if event.sequence != state.sequence || event.previous != state.chain
                    || event.operation != operation || event.request_blake3 != request_blake3 {
                    bail!("REPLAY_IO_ORDER_MISMATCH: {operation}");
                }
                let value = read_frame(&mut state.file, maximum)?;
                if value.len() as u64 != event.bytes
                    || blake3::hash(&value).to_hex().as_str() != event.value_blake3 {
                    bail!("REPLAY_IO_CORRUPT: input receipt");
                }
                (event, metadata, value)
            } else {
                let value = supply()?;
                if value.len() as u64 > maximum { bail!("REPLAY_IO_INVALID: supplier exceeds guest request"); }
                let event = Event { sequence: state.sequence, previous: state.chain.clone(),
                    operation: operation.into(), request_blake3,
                    value_blake3: blake3::hash(&value).to_hex().to_string(), bytes: value.len() as u64 };
                let metadata = serde_json::to_vec(&event)?;
                write_frame(&mut state.file, &metadata)?;
                write_frame(&mut state.file, &value)?;
                // Receipt precedes guest visibility, not merely process exit.
                state.file.sync_all()?;
                (event, metadata, value)
            };
            let mut hash = blake3::Hasher::new();
            hash.update(event.previous.as_bytes());
            hash.update(&metadata);
            state.chain = hash.finalize().to_hex().to_string();
            state.sequence += 1;
            Ok(value)
        })();
        if result.is_err() { state.failed = true; }
        result
    }
    pub fn finish(&self) -> Result<IoReceipt> {
        let mut state = self.0.state.lock().map_err(|_| anyhow::anyhow!("REPLAY_IO_POISONED"))?;
        if state.failed || state.finished { bail!("REPLAY_IO_INCOMPLETE"); }
        if self.0.replay {
            let mut extra = [0];
            if state.file.read(&mut extra)? != 0 { state.failed = true; bail!("REPLAY_IO_UNCONSUMED"); }
        } else { state.file.sync_all()?; }
        state.finished = true;
        Ok(IoReceipt { binding_blake3: state.binding.clone(), events: state.sequence,
            chain_blake3: state.chain.clone() })
    }
    pub fn reject_unrecorded(&self, operation: &str) -> Result<()> {
        if self.is_replay() {
            let mut state = self.0.state.lock().map_err(|_| anyhow::anyhow!("REPLAY_IO_POISONED"))?;
            state.failed = true;
            bail!("REPLAY_IO_UNOBSERVED: {operation}; no fresh input or effect is permitted");
        }
        Ok(())
    }
}

pub fn abi_identity() -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(include_bytes!("p1.rs"));
    hash.update(include_bytes!("runtime.rs"));
    hash.update(include_bytes!("../Cargo.toml"));
    format!("wasi-preview1:shenko-p1:{}", hash.finalize().to_hex())
}

pub(crate) fn invocation_digest(arguments: &[String], environment: &[(String, String)],
    mounts: &[crate::mount::MountSpec], home: &Path) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&(arguments, environment, mounts, home))?).to_hex().to_string())
}
