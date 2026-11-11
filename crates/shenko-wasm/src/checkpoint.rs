//! Persistent `/home` + `/memory` byte closure: capture, verify and restore
//! (docs/specs/wasm-home.md#spec-8-3, docs/specs/durable-state.md#spec-10-1–docs/specs/memory.md#spec-10-5, C05).
//!
//! This module implements the `FilesAndMetadata` recovery level: every regular
//! file's bytes are sealed into the installation's content-addressed store,
//! and the directory structure, permission bits and symbolic links are
//! recorded. A restore rewrites exactly that layer. It does not capture WASM
//! linear memory, stacks, interpreter object graphs, processes or sockets, and
//! a restore never replays or reverses an external effect: a message that was
//! sent stays sent, a remote write stays written. Restoring file bytes is not
//! a rollback of anything that already left the sandbox.
//!
//! Publication follows the durable order from docs/specs/memory.md#spec-10-5: every file object is
//! sealed and verified first, then the manifest object, then the append-only
//! journal line, and only then is the `head` pointer replaced — under an
//! expected-old-value check, so a concurrent publication is refused instead of
//! being overwritten. A closure that misses an object, names a path whose
//! ancestors are not directories of the same closure, or belongs to another
//! installation is refused *before* anything is mutated, with the cause
//! retained.
//!
//! All file access goes through admitted directory handles ([`OwnedDir`]), so a
//! path component that turns into a symbolic link cannot redirect a capture or
//! a restore outside the managed mount. A mount root that is absent is recorded
//! as absent — never as an empty directory — and a closure that itself recorded
//! an absent mount refuses to "restore" that absence.

use anyhow::{Context, Result, anyhow, bail, ensure};
use cap_std::fs::{Dir, Metadata};
#[cfg(unix)]
use cap_std::fs::{MetadataExt, Permissions, PermissionsExt};
use serde::{Deserialize, Serialize};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mount::OwnedDir;

/// Versioned closure format. A different format is refused, never guessed.
pub const CHECKPOINT_FORMAT: &str = "shenko-home-checkpoint/1";
/// In-flight restore intent format (present only while a restore mutates).
pub const RESTORE_INTENT_FORMAT: &str = "shenko-home-restore-intent/1";
/// Installation-local directory holding the published head, the journal and
/// the in-flight restore intent. This is a snapshot root record, not a second
/// source of truth for tasks, evidence or external effects.
pub const CHECKPOINT_DIR: &str = "home-checkpoints";
pub const HEAD_FILE: &str = "head";
pub const JOURNAL_FILE: &str = "journal.jsonl";
pub const INTENT_FILE: &str = "restore-intent.json";
/// Append-only record of completed restores. It is a root for reclamation:
/// the closure a restore replaced (its `safety`) stays readable instead of
/// becoming an unreferenced object as soon as the next capture moves `head`.
pub const RESTORE_RECEIPTS_FILE: &str = "restore-receipts.jsonl";
/// One restore receipt as persisted; the version is never guessed.
pub const RESTORE_RECEIPT_FORMAT: &str = "shenko-home-restore-receipt/1";
/// CAS labels for file objects and closure manifests.
pub const FILE_MEDIA_TYPE: &str = "application/octet-stream";
pub const CLOSURE_MEDIA_TYPE: &str = "application/vnd.shenko.home-checkpoint+json";
pub const STORE_NAMESPACE: &str = "seed-home-checkpoint";
pub const CODEC_NAME: &str = "shenko-home-checkpoint";
pub const CODEC_MAJOR: u32 = 1;

/// Depth and entry caps for the ordered walk. A tree that exceeds them is
/// refused rather than truncated: a partial tree must never be published as a
/// complete closure.
const MAX_TREE_DEPTH: usize = 64;
const MAX_TREE_ENTRIES: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointEntryKind {
    Directory,
    File,
    Symlink,
}

/// One captured entry. `digest`/`bytes` are the file bytes for `File`, the
/// empty digest for `Directory` and the exact link target bytes for `Symlink`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointEntry {
    pub kind: CheckpointEntryKind,
    /// Unix permission bits (`0o777`) observed on the host entry. `None` when
    /// the platform has none or the entry is a symbolic link; restore then
    /// leaves the mode as created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    pub bytes: u64,
    pub digest: Digest,
    /// Exact symbolic-link target as recorded, for `Symlink` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl CheckpointEntry {
    fn directory(mode: Option<u32>) -> Self {
        Self {
            kind: CheckpointEntryKind::Directory,
            mode,
            bytes: 0,
            digest: Digest::blake3(b""),
            target: None,
        }
    }

    fn object_ref(&self) -> ObjectRef {
        ObjectRef {
            digest: self.digest.clone(),
            bytes: self.bytes,
            media_type: FILE_MEDIA_TYPE.into(),
            store_namespace: STORE_NAMESPACE.into(),
            locator: String::new(),
            codec: SchemaRef {
                name: CODEC_NAME.into(),
                major: CODEC_MAJOR,
                digest: Digest::blake3(CODEC_NAME.as_bytes()),
            },
        }
    }
}

/// One mount's captured tree. `present` is false when the mount root itself
/// did not exist at capture time — recording absence instead of an empty
/// directory is what keeps a damaged home distinguishable from a fresh one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountTree {
    pub present: bool,
    pub entries: BTreeMap<String, CheckpointEntry>,
}

impl MountTree {
    fn absent() -> Self {
        Self {
            present: false,
            entries: BTreeMap::new(),
        }
    }
}

/// A content-addressed closure over the persistent mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointManifest {
    pub format: String,
    pub installation_id: EntityId,
    /// Published closure id that was `head` when this capture started.
    pub parent: Option<Digest>,
    pub timestamp_unix_ms: u64,
    /// Why this closure was published; never used to make a recovery decision.
    pub reason: String,
    /// guest path (`/home`, `/memory`) -> captured tree.
    pub mounts: BTreeMap<String, MountTree>,
}

impl CheckpointManifest {
    pub fn entry_count(&self) -> usize {
        self.mounts
            .values()
            .map(|tree| tree.entries.len())
            .sum()
    }
}

/// Result of a capture. `published` is false when the captured view already
/// equalled the published head, so an unchanged home does not grow the journal.
#[derive(Debug, Clone)]
pub struct CaptureOutcome {
    pub manifest: CheckpointManifest,
    pub id: Digest,
    pub published: bool,
    pub objects_sealed: usize,
    pub bytes: u64,
}

/// Every closure and file object a checkpoint record set still needs.
///
/// This is what reclamation must keep: the published head, every journal
/// entry (append-only history stays restorable), a pending restore intent's
/// checkpoint *and* safety closure, and every completed restore receipt's
/// checkpoint, safety and resulting head. `objects` are the file bytes those
/// closures name; `closures` are the manifests and the COW commit objects
/// that reference them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointRoots {
    pub head: Option<Digest>,
    pub closures: BTreeSet<Digest>,
    pub objects: BTreeSet<Digest>,
    pub pending_restore: Option<RestoreIntent>,
    pub receipts: usize,
}

/// Summary returned by a complete closure verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedClosure {
    pub entries: usize,
    pub objects: usize,
    pub bytes: u64,
}

/// An interrupted restore, recorded durably before the first mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreIntent {
    pub format: String,
    /// Closure being restored.
    pub checkpoint: Digest,
    /// Closure captured immediately before mutating (the pre-restore state).
    pub safety: Digest,
    pub timestamp_unix_ms: u64,
}

/// What a completed restore did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreReceipt {
    pub format: String,
    /// Closure that was restored.
    pub checkpoint: Digest,
    /// Closure captured from the state this restore replaced, so those bytes
    /// stay available in CAS instead of being destroyed.
    pub safety: Digest,
    /// Published closure id of the restored view.
    pub head: Digest,
    pub closure: VerifiedClosure,
    pub written: usize,
    pub removed: usize,
    /// Entries whose type had to be replaced (file <-> directory <-> link).
    pub replaced: usize,
    /// When the receipt was recorded, so an operator can order receipts.
    pub recorded_at_unix_ms: u64,
}

/// One installation's checkpoint record over its declared persistent mounts.
pub struct HomeCheckpointStore<'a> {
    cas: &'a ContentStore,
    data_dir: &'a Path,
    installation_id: EntityId,
    mounts: Vec<(String, PathBuf)>,
}

impl<'a> HomeCheckpointStore<'a> {
    /// Bind to one installation's persistent mounts. Guest paths must be
    /// absolute and unique; the directories may be absent — that is exactly
    /// what a restore has to recreate.
    pub fn new(
        cas: &'a ContentStore,
        data_dir: &'a Path,
        installation_id: EntityId,
        mounts: Vec<(String, PathBuf)>,
    ) -> Result<Self> {
        ensure!(
            !mounts.is_empty(),
            "HOME_CHECKPOINT_INVALID: a checkpoint needs at least one persistent mount"
        );
        let mut guests = BTreeSet::new();
        for (guest, dir) in &mounts {
            validate_guest_path(guest)?;
            ensure!(
                guests.insert(guest.clone()),
                "HOME_CHECKPOINT_INVALID: duplicate guest mount {guest}"
            );
            ensure!(
                dir.is_absolute(),
                "HOME_CHECKPOINT_INVALID: persistent mount {guest} needs an absolute host directory, got {}",
                dir.display()
            );
        }
        Ok(Self {
            cas,
            data_dir,
            installation_id,
            mounts,
        })
    }

    /// Guest paths this store covers, in declaration order.
    pub fn guest_paths(&self) -> Vec<&str> {
        self.mounts.iter().map(|(guest, _)| guest.as_str()).collect()
    }

    /// Installation directory that holds the head, journal and intent.
    pub fn record_dir(&self) -> PathBuf {
        self.data_dir.join(CHECKPOINT_DIR)
    }

    /// Published closure id, if any.
    pub fn head(&self) -> Result<Option<Digest>> {
        let path = self.record_dir().join(HEAD_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow!(
                    "HOME_CHECKPOINT_INCOMPLETE: read {}: {error}",
                    path.display()
                ));
            }
        };
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let digest = Digest(text.to_string());
        ensure!(
            digest.canonical_blake3_hex().is_some(),
            "HOME_CHECKPOINT_INCOMPLETE: corrupt head {}: {text}",
            path.display()
        );
        Ok(Some(digest))
    }

    /// Read one published closure manifest and check that it belongs to this
    /// installation and mount set. It does not verify the object closure;
    /// [`Self::verify`] does that before any mutation.
    pub fn manifest(&self, id: &Digest) -> Result<CheckpointManifest> {
        let bytes = self
            .cas
            .read_object(id)
            .map_err(|error| anyhow!("HOME_CHECKPOINT_INCOMPLETE: read closure {id}: {error}"))?;
        let manifest: CheckpointManifest = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow!("HOME_CHECKPOINT_INCOMPLETE: decode closure {id}: {error}")
        })?;
        self.admit_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Published closures, oldest first. A journal line that cannot be read is
    /// reported, never skipped: an unreadable journal is not empty history.
    pub fn journal(&self) -> Result<Vec<CheckpointManifest>> {
        let path = self.record_dir().join(JOURNAL_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow!(
                    "HOME_CHECKPOINT_INCOMPLETE: read {}: {error}",
                    path.display()
                ));
            }
        };
        let mut out = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let manifest: CheckpointManifest = serde_json::from_slice(line).map_err(|error| {
                anyhow!(
                    "HOME_CHECKPOINT_INCOMPLETE: journal {} line {}: {error}",
                    path.display(),
                    index + 1
                )
            })?;
            self.admit_manifest(&manifest)?;
            out.push(manifest);
        }
        Ok(out)
    }

    /// The latest published closure manifest, if any.
    pub fn latest(&self) -> Result<Option<CheckpointManifest>> {
        match self.head()? {
            Some(id) => Ok(Some(self.manifest(&id)?)),
            None => Ok(None),
        }
    }

    /// A restore intent left behind by an interrupted restore, if any.
    pub fn pending_restore(&self) -> Result<Option<RestoreIntent>> {
        let path = self.record_dir().join(INTENT_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow!(
                    "HOME_CHECKPOINT_INCOMPLETE: read restore intent {}: {error}",
                    path.display()
                ));
            }
        };
        let intent: RestoreIntent = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow!(
                "HOME_CHECKPOINT_INCOMPLETE: decode restore intent {}: {error}",
                path.display()
            )
        })?;
        ensure!(
            intent.format == RESTORE_INTENT_FORMAT,
            "HOME_CHECKPOINT_INCOMPLETE: unsupported restore intent format {:?}",
            intent.format
        );
        Ok(Some(intent))
    }

    /// Completed restores, oldest first. A receipt line that cannot be read is
    /// reported instead of skipped: a receipt names closures that must stay
    /// reclaimable, so an unreadable line is not "no receipt".
    pub fn restore_receipts(&self) -> Result<Vec<RestoreReceipt>> {
        let path = self.record_dir().join(RESTORE_RECEIPTS_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow!(
                    "HOME_CHECKPOINT_INCOMPLETE: read restore receipts {}: {error}",
                    path.display()
                ));
            }
        };
        let mut receipts = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let receipt: RestoreReceipt = serde_json::from_slice(line).map_err(|error| {
                anyhow!(
                    "HOME_CHECKPOINT_INCOMPLETE: restore receipts {} line {}: {error}",
                    path.display(),
                    index + 1
                )
            })?;
            ensure!(
                receipt.format == RESTORE_RECEIPT_FORMAT,
                "HOME_CHECKPOINT_INCOMPLETE: unsupported restore receipt format {:?}",
                receipt.format
            );
            receipts.push(receipt);
        }
        Ok(receipts)
    }

    /// Every published closure this record set can still restore from, plus
    /// every file object those closures reference. This is root *discovery*
    /// for reclamation, not admission: it reads the durable records and
    /// reports what they name. A record that cannot be read or decoded is an
    /// error here, so a caller can refuse to reclaim instead of guessing that
    /// the unreadable part referenced nothing.
    pub fn roots(&self) -> Result<CheckpointRoots> {
        let mut roots = CheckpointRoots::default();
        let head = self.head()?;
        let mut manifests: Vec<(Digest, CheckpointManifest)> = Vec::new();
        for manifest in self.journal()? {
            let bytes = serde_json::to_vec(&manifest).expect("checkpoint manifest serialize");
            let id = Digest::blake3(&bytes);
            ensure!(
                self.cas.object_exists(&id),
                "HOME_CHECKPOINT_INCOMPLETE: journal names closure {id} that is not in this installation's CAS"
            );
            manifests.push((id, manifest));
        }
        if let Some(head) = &head {
            if !manifests.iter().any(|(id, _)| id == head) {
                // `head` may name a closure whose journal line was lost. Its
                // bytes are still a root, so read it through the CAS.
                manifests.push((head.clone(), self.manifest(head)?));
            }
        }
        for (id, manifest) in manifests {
            roots.closures.insert(id);
            for tree in manifest.mounts.values() {
                for entry in tree.entries.values() {
                    if entry.kind == CheckpointEntryKind::File {
                        roots.objects.insert(entry.digest.clone());
                    }
                }
            }
        }
        roots.head = head;
        roots.pending_restore = self.pending_restore()?;
        if let Some(intent) = &roots.pending_restore {
            roots.closures.insert(intent.checkpoint.clone());
            roots.closures.insert(intent.safety.clone());
            // The closures an interrupted restore names are roots even when
            // the CAS copy is the only surviving record of them.
            for id in [&intent.checkpoint, &intent.safety] {
                let manifest = self.manifest(id)?;
                for tree in manifest.mounts.values() {
                    for entry in tree.entries.values() {
                        if entry.kind == CheckpointEntryKind::File {
                            roots.objects.insert(entry.digest.clone());
                        }
                    }
                }
            }
        }
        for receipt in self.restore_receipts()? {
            for id in [&receipt.checkpoint, &receipt.safety, &receipt.head] {
                roots.closures.insert(id.clone());
                // A receipt whose closure is gone is reported by the caller as
                // a missing root, not silently treated as empty.
                let manifest = self.manifest(id)?;
                for tree in manifest.mounts.values() {
                    for entry in tree.entries.values() {
                        if entry.kind == CheckpointEntryKind::File {
                            roots.objects.insert(entry.digest.clone());
                        }
                    }
                }
            }
            roots.receipts += 1;
        }
        Ok(roots)
    }

    /// Capture both persistent mounts, seal every file's bytes, and publish the
    /// closure unless the captured view equals the published head.
    pub fn capture(&self, reason: &str) -> Result<CaptureOutcome> {
        let parent = self.head()?;
        let mut mounts = BTreeMap::new();
        let mut stats = CaptureStats::default();
        for (guest, dir) in &self.mounts {
            let admitted = match OwnedDir::admit_absolute(dir) {
                Ok(admitted) => admitted,
                Err(error) => {
                    // An absent root is recorded as absent. Any other failure
                    // (a link, a permission problem, a swapped ancestor) is a
                    // real error: it must not be published as "no entries".
                    if std::fs::symlink_metadata(dir)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    {
                        mounts.insert(guest.clone(), MountTree::absent());
                        continue;
                    }
                    return Err(error).with_context(|| {
                        format!(
                            "HOME_CHECKPOINT_INCOMPLETE: persistent mount {guest} cannot be entered at {}",
                            dir.display()
                        )
                    });
                }
            };
            let mut entries = BTreeMap::new();
            capture_dir(self.cas, admitted.dir(), "", 0, &mut entries, &mut stats)
                .with_context(|| format!("capture {guest} at {}", dir.display()))?;
            mounts.insert(
                guest.clone(),
                MountTree {
                    present: true,
                    entries,
                },
            );
        }
        let manifest = CheckpointManifest {
            format: CHECKPOINT_FORMAT.into(),
            installation_id: self.installation_id,
            parent: parent.clone(),
            timestamp_unix_ms: now_unix_ms(),
            reason: reason.to_string(),
            mounts,
        };

        if let Some(head) = &parent {
            let previous = self.manifest(head)?;
            if previous.mounts == manifest.mounts {
                return Ok(CaptureOutcome {
                    manifest: previous,
                    id: head.clone(),
                    published: false,
                    objects_sealed: stats.objects,
                    bytes: stats.bytes,
                });
            }
        }

        let bytes = serde_json::to_vec(&manifest).expect("checkpoint manifest serialize");
        let sealed = self
            .cas
            .seal_object(
                &bytes,
                CLOSURE_MEDIA_TYPE,
                STORE_NAMESPACE,
                CODEC_NAME,
                CODEC_MAJOR,
            )
            .map_err(|error| anyhow!("HOME_CHECKPOINT_INCOMPLETE: seal closure: {error}"))?;
        let id = sealed.object.digest.clone();

        // Journal first (append-only, durable), then the head pointer under the
        // expected-old-value check.
        let journal = self.record_dir().join(JOURNAL_FILE);
        let mut line = bytes;
        line.push(b'\n');
        append_durable(&journal, &line)
            .with_context(|| format!("publish checkpoint journal {}", journal.display()))?;
        self.publish_head(&id, parent.as_ref())?;

        Ok(CaptureOutcome {
            manifest,
            id,
            published: true,
            objects_sealed: stats.objects,
            bytes: stats.bytes,
        })
    }

    fn publish_head(&self, id: &Digest, expected_old: Option<&Digest>) -> Result<()> {
        let path = self.record_dir().join(HEAD_FILE);
        let current = self.head()?;
        ensure!(
            current.as_ref() == expected_old,
            "HOME_CHECKPOINT_CONFLICT: head changed from {:?} to {:?} while publishing {id}; refusing to overwrite a concurrent publication",
            expected_old,
            current
        );
        write_atomic(&path, format!("{id}\n").as_bytes())
            .with_context(|| format!("publish checkpoint head {}", path.display()))
    }

    /// Verify the whole closure before anything is mutated: format, ownership,
    /// path shape, recorded ancestors, and every file object's digest and size
    /// in this installation's CAS.
    pub fn verify(&self, manifest: &CheckpointManifest) -> Result<VerifiedClosure> {
        self.admit_manifest(manifest)?;
        let mut objects = 0usize;
        let mut bytes = 0u64;
        for (guest, tree) in &manifest.mounts {
            if !tree.present {
                ensure!(
                    tree.entries.is_empty(),
                    "HOME_CHECKPOINT_INCOMPLETE: closure {guest} records an absent mount with entries"
                );
                continue;
            }
            for (rel, entry) in &tree.entries {
                validate_rel(rel).with_context(|| format!("closure {guest} path {rel:?}"))?;
                // Every path's ancestors must be directories of this same
                // closure: a restore must never invent a directory the closure
                // did not record, and a removal pass must never delete a
                // recorded subtree.
                let mut ancestors: Vec<&str> = rel.split('/').collect();
                ancestors.pop();
                while !ancestors.is_empty() {
                    let ancestor = ancestors.join("/");
                    ensure!(
                        tree.entries.get(&ancestor).is_some_and(|entry| {
                            entry.kind == CheckpointEntryKind::Directory
                        }),
                        "HOME_CHECKPOINT_INCOMPLETE: closure {guest} path {rel:?} has no recorded directory {ancestor:?}"
                    );
                    ancestors.pop();
                }
                match entry.kind {
                    CheckpointEntryKind::Directory => {
                        ensure!(
                            entry.target.is_none() && entry.bytes == 0,
                            "HOME_CHECKPOINT_INCOMPLETE: closure {guest} directory {rel:?} carries file data"
                        );
                    }
                    CheckpointEntryKind::File => {
                        ensure!(
                            entry.target.is_none(),
                            "HOME_CHECKPOINT_INCOMPLETE: closure {guest} file {rel:?} carries a link target"
                        );
                        let path = self
                            .cas
                            .verified_object_path(&entry.object_ref())
                            .map_err(|error| {
                                anyhow!(
                                    "HOME_CHECKPOINT_INCOMPLETE: closure {guest} file {rel:?} object {} is missing or corrupt: {error}",
                                    entry.digest
                                )
                            })?;
                        ensure!(
                            path.is_file(),
                            "HOME_CHECKPOINT_INCOMPLETE: closure {guest} file {rel:?} object is not a file"
                        );
                        objects += 1;
                        bytes += entry.bytes;
                    }
                    CheckpointEntryKind::Symlink => {
                        ensure!(
                            entry.target.is_some() && entry.bytes == 0,
                            "HOME_CHECKPOINT_INCOMPLETE: closure {guest} link {rel:?} has no recorded target"
                        );
                    }
                }
            }
        }
        Ok(VerifiedClosure {
            entries: manifest.entry_count(),
            objects,
            bytes,
        })
    }

    fn admit_manifest(&self, manifest: &CheckpointManifest) -> Result<()> {
        ensure!(
            manifest.format == CHECKPOINT_FORMAT,
            "HOME_CHECKPOINT_INCOMPLETE: unsupported closure format {:?}",
            manifest.format
        );
        ensure!(
            manifest.installation_id == self.installation_id,
            "DATA_PATH_CONFLICT: closure belongs to installation {}, not {}",
            manifest.installation_id,
            self.installation_id
        );
        let declared: BTreeSet<&str> = manifest.mounts.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = self.mounts.iter().map(|(guest, _)| guest.as_str()).collect();
        ensure!(
            declared == expected,
            "DATA_PATH_CONFLICT: closure covers {:?} but this installation declares {:?}",
            declared,
            expected
        );
        ensure!(
            manifest.entry_count() <= MAX_TREE_ENTRIES,
            "HOME_CHECKPOINT_INCOMPLETE: closure exceeds {MAX_TREE_ENTRIES} entries"
        );
        Ok(())
    }

    /// Restore a verified closure into the persistent mounts.
    ///
    /// The current state is captured and published first, so the bytes this
    /// restore replaces stay in CAS instead of being destroyed. A durable intent
    /// is written before the first mutation and removed only after the restored
    /// view is captured again: an interrupted restore stays visible and can be
    /// resumed, and it is never reported as completed.
    ///
    /// A restore assumes no guest is executing against these mounts: it applies
    /// a full-view replacement, not a merge with concurrent writers. Bootstrap
    /// only restores before the subject starts. Writer exclusion and revocation
    /// of in-flight guest I/O are a separate mechanism and are not claimed here.
    pub fn restore(&self, manifest: &CheckpointManifest, reason: &str) -> Result<RestoreReceipt> {
        let closure = self.verify(manifest)?;
        let target_id = checkpoint_object_id(self.cas, manifest)?;
        for (guest, tree) in &manifest.mounts {
            ensure!(
                tree.present,
                "HOME_CHECKPOINT_INCOMPLETE: closure recorded {guest} as absent; restoring that absence would delete the persistent mount, so it is refused"
            );
        }

        let safety = self.capture(&format!("pre-restore: {reason}"))?;
        let intent = RestoreIntent {
            format: RESTORE_INTENT_FORMAT.into(),
            checkpoint: target_id.clone(),
            safety: safety.id.clone(),
            timestamp_unix_ms: now_unix_ms(),
        };
        let intent_path = self.record_dir().join(INTENT_FILE);
        write_atomic(
            &intent_path,
            &serde_json::to_vec_pretty(&intent).expect("restore intent serialize"),
        )
        .with_context(|| format!("record restore intent {}", intent_path.display()))?;

        let mut applied = ApplyStats::default();
        if let Err(error) = self.apply(manifest, &mut applied) {
            return Err(error.context(format!(
                "HOME_CHECKPOINT_INCOMPLETE: restore of {target_id} stopped after writing {} and removing {} entries; intent {} retains checkpoint {target_id} and safety {}",
                applied.written,
                applied.removed,
                intent_path.display(),
                safety.id
            )));
        }

        let restored = self.capture(&format!("restored: {reason}"))?;
        let receipt = RestoreReceipt {
            format: RESTORE_RECEIPT_FORMAT.into(),
            checkpoint: target_id,
            safety: safety.id,
            head: restored.id,
            closure,
            written: applied.written,
            removed: applied.removed,
            replaced: applied.replaced,
            recorded_at_unix_ms: now_unix_ms(),
        };
        // The receipt is durable before the intent is dropped: the closures it
        // names (restored and safety) are roots for reclamation, so losing the
        // receipt would turn recoverable bytes into orphans. A receipt write
        // that fails keeps the intent in place: the restore is then reported
        // as still unfinished rather than as a clean completion.
        self.append_receipt(&receipt)
            .context("record completed restore receipt")?;
        std::fs::remove_file(&intent_path).with_context(|| {
            format!(
                "remove completed restore intent {}",
                intent_path.display()
            )
        })?;
        closure_sync_dir(&self.record_dir())?;
        Ok(receipt)
    }

    fn append_receipt(&self, receipt: &RestoreReceipt) -> Result<()> {
        let mut line = serde_json::to_vec(receipt).expect("restore receipt serialize");
        line.push(b'\n');
        append_durable(&self.record_dir().join(RESTORE_RECEIPTS_FILE), &line)
    }

    fn apply(&self, manifest: &CheckpointManifest, applied: &mut ApplyStats) -> Result<()> {
        for (guest, dir) in &self.mounts {
            let tree = manifest
                .mounts
                .get(guest)
                .ok_or_else(|| anyhow!("HOME_CHECKPOINT_INCOMPLETE: closure has no {guest} tree"))?;
            ensure!(
                tree.present,
                "HOME_CHECKPOINT_INCOMPLETE: closure recorded {guest} as absent"
            );
            // The mount root is materialized as a real directory; every entry
            // below it comes from the closure.
            closure_root(dir).with_context(|| {
                format!(
                    "HOME_CHECKPOINT_INCOMPLETE: restore root {guest} at {}",
                    dir.display()
                )
            })?;
            let admitted = OwnedDir::admit_absolute(dir).with_context(|| {
                format!(
                    "HOME_CHECKPOINT_INCOMPLETE: admit restore root {guest} at {}",
                    dir.display()
                )
            })?;
            let root = admitted.dir();

            // Directories first: a `/`-separated path always sorts after its own
            // ancestors, so one ordered pass creates every level.
            for (rel, entry) in &tree.entries {
                if entry.kind != CheckpointEntryKind::Directory {
                    continue;
                }
                applied.replaced += usize::from(closure_directory(root, rel)?);
                applied.written += 1;
            }
            for (rel, entry) in &tree.entries {
                match entry.kind {
                    CheckpointEntryKind::File => {
                        let object = self
                            .cas
                            .verified_object_path(&entry.object_ref())
                            .map_err(|error| {
                                anyhow!(
                                    "closure file {guest} {rel:?} object {} is missing or corrupt: {error}",
                                    entry.digest
                                )
                            })?;
                        applied.replaced += usize::from(closure_file(root, rel, &object, entry.bytes, entry.mode)?);
                        applied.written += 1;
                    }
                    CheckpointEntryKind::Symlink => {
                        let target = entry
                            .target
                            .as_deref()
                            .ok_or_else(|| anyhow!("closure {guest} link {rel:?} has no target"))?;
                        applied.replaced += usize::from(closure_symlink(root, rel, target)?);
                        applied.written += 1;
                    }
                    CheckpointEntryKind::Directory => {}
                }
            }
            applied.removed += apply_removals(root, "", &tree.entries, 0)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct ApplyStats {
    written: usize,
    removed: usize,
    replaced: usize,
}

#[derive(Debug, Default)]
struct CaptureStats {
    objects: usize,
    bytes: u64,
}

fn capture_dir(
    cas: &ContentStore,
    dir: &Dir,
    prefix: &str,
    depth: usize,
    entries: &mut BTreeMap<String, CheckpointEntry>,
    stats: &mut CaptureStats,
) -> Result<()> {
    if depth > MAX_TREE_DEPTH {
        bail!(
            "HOME_CHECKPOINT_UNSUPPORTED: tree deeper than {MAX_TREE_DEPTH} levels below {prefix:?}"
        );
    }
    let read = dir.entries().context("list directory")?;
    for entry in read {
        let entry = entry.context("read directory entry")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            anyhow!("HOME_CHECKPOINT_UNSUPPORTED: non-UTF-8 entry name under {prefix:?}")
        })?;
        let rel = rel_join(prefix, name);
        validate_rel(&rel)?;
        if entries.len() >= MAX_TREE_ENTRIES {
            bail!(
                "HOME_CHECKPOINT_UNSUPPORTED: more than {MAX_TREE_ENTRIES} entries below the mount"
            );
        }
        let metadata = entry.metadata().with_context(|| format!("inspect {rel:?}"))?;
        let file_type = entry.file_type().with_context(|| format!("type of {rel:?}"))?;
        if file_type.is_symlink() {
            let target = dir
                .read_link(name)
                .with_context(|| format!("read link {rel:?}"))?;
            let target = target.to_str().ok_or_else(|| {
                anyhow!("HOME_CHECKPOINT_UNSUPPORTED: non-UTF-8 link target at {rel:?}")
            })?;
            entries.insert(
                rel,
                CheckpointEntry {
                    kind: CheckpointEntryKind::Symlink,
                    mode: None,
                    bytes: 0,
                    digest: Digest::blake3(target.as_bytes()),
                    target: Some(target.to_string()),
                },
            );
        } else if file_type.is_dir() {
            entries.insert(rel.clone(), CheckpointEntry::directory(entry_mode(&metadata)));
            let child = dir.open_dir(name).with_context(|| format!("open {rel:?}"))?;
            capture_dir(cas, &child, &rel, depth + 1, entries, stats)?;
        } else if file_type.is_file() {
            // The file is read through the admitted handle. A file whose
            // identity changes underneath the capture makes the view unstable,
            // and that is refused instead of published.
            let before = file_identity(&metadata);
            let mut file = dir.open(name).with_context(|| format!("open {rel:?}"))?;
            let sealed = cas
                .seal_reader(
                    &mut file,
                    FILE_MEDIA_TYPE,
                    STORE_NAMESPACE,
                    CODEC_NAME,
                    CODEC_MAJOR,
                )
                .map_err(|error| anyhow!("seal {rel:?}: {error}"))?;
            let after = dir
                .symlink_metadata(name)
                .with_context(|| format!("re-inspect {rel:?}"))?;
            ensure!(
                file_identity(&after) == before,
                "HOME_CHECKPOINT_UNSTABLE: {rel:?} changed while it was captured; capture a quiescent home"
            );
            stats.objects += 1;
            stats.bytes += sealed.object.bytes;
            entries.insert(
                rel,
                CheckpointEntry {
                    kind: CheckpointEntryKind::File,
                    mode: entry_mode(&metadata),
                    bytes: sealed.object.bytes,
                    digest: sealed.object.digest,
                    target: None,
                },
            );
        } else {
            bail!(
                "HOME_CHECKPOINT_UNSUPPORTED: {rel:?} is a special file; \"/home\" and \"/memory\" checkpoints cover regular files, directories and symbolic links only"
            );
        }
    }
    Ok(())
}

/// `(len, mtime_ns, inode)` identity used to detect a file captured while it
/// was being modified or replaced.
fn file_identity(metadata: &Metadata) -> (u64, Option<i128>, u64) {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128);
    #[cfg(unix)]
    {
        (metadata.len(), modified, metadata.ino())
    }
    #[cfg(not(unix))]
    {
        (metadata.len(), modified, 0)
    }
}

fn entry_mode(metadata: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        Some(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

pub(crate) fn closure_mode(dir: &Dir, rel: &str, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(mode) = mode {
            dir.set_permissions(rel, Permissions::from_mode(mode))
                .with_context(|| format!("set mode of {rel:?}"))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, rel, mode);
    }
    Ok(())
}

/// Ensure `rel` is a real directory, replacing a file or symbolic link that
/// occupies the path. Never follows the existing entry. Returns whether an
/// existing non-directory was replaced.
pub(crate) fn closure_directory(dir: &Dir, rel: &str) -> Result<bool> {
    match dir.symlink_metadata(rel) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(_) => {
            dir.remove_file(rel)
                .with_context(|| format!("replace non-directory {rel:?}"))?;
            dir.create_dir_all(rel)
                .with_context(|| format!("create directory {rel:?}"))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            dir.create_dir_all(rel)
                .with_context(|| format!("create directory {rel:?}"))?;
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| format!("inspect {rel:?}")),
    }
}

/// Write one file from the verified CAS object. The bytes are staged next to
/// the destination and renamed over it, so a reader never sees a half-written
/// file, and the staged length must match the closure before publication.
/// Returns whether an existing directory had to be replaced.
pub(crate) fn closure_file(
    dir: &Dir,
    rel: &str,
    object: &Path,
    bytes: u64,
    mode: Option<u32>,
) -> Result<bool> {
    let mut replaced = false;
    if let Ok(metadata) = dir.symlink_metadata(rel) {
        if metadata.is_dir() {
            dir.remove_dir_all(rel)
                .with_context(|| format!("remove directory occupying file path {rel:?}"))?;
            replaced = true;
        }
    }
    let name = Path::new(rel)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("closure path {rel:?} has no file name"))?;
    let staging = format!(".{name}.shenko-restore-{}", EntityId::new());
    let staging = match Path::new(rel).parent().filter(|parent| !parent.as_os_str().is_empty()) {
        Some(parent) => parent
            .join(&staging)
            .to_str()
            .ok_or_else(|| anyhow!("staging path for {rel:?} is not UTF-8"))?
            .to_string(),
        None => staging,
    };
    {
        let mut source =
            std::fs::File::open(object).with_context(|| format!("open sealed object for {rel:?}"))?;
        let mut destination = dir.create(&staging).with_context(|| format!("stage {rel:?}"))?;
        std::io::copy(&mut source, &mut destination)
            .with_context(|| format!("write staged {rel:?}"))?;
        destination
            .sync_all()
            .with_context(|| format!("sync staged {rel:?}"))?;
    }
    let staged = dir
        .symlink_metadata(&staging)
        .with_context(|| format!("inspect staged {rel:?}"))?;
    ensure!(
        staged.len() == bytes,
        "HOME_CHECKPOINT_INCOMPLETE: staged {rel:?} has {} bytes, the closure records {bytes}",
        staged.len()
    );
    dir.rename(&staging, dir, rel)
        .with_context(|| format!("publish staged {rel:?}"))?;
    closure_mode(dir, rel, mode)?;
    Ok(replaced)
}

/// Create one symbolic link from the closure. Returns whether an existing entry
/// had to be replaced.
pub(crate) fn closure_symlink(dir: &Dir, rel: &str, target: &str) -> Result<bool> {
    let replaced = match dir.symlink_metadata(rel) {
        Ok(metadata) if metadata.is_dir() => {
            dir.remove_dir_all(rel)
                .with_context(|| format!("remove directory occupying link path {rel:?}"))?;
            true
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            dir.remove_file(rel)
                .with_context(|| format!("remove previous link {rel:?}"))?;
            false
        }
        Ok(_) => {
            dir.remove_file(rel)
                .with_context(|| format!("remove entry occupying link path {rel:?}"))?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("inspect {rel:?}")),
    };
    // `symlink_contents` records the target exactly as captured; a recorded
    // absolute target is legitimate guest state and is not a host path
    // resolution (guest resolution stays confined to the preopen).
    dir.symlink_contents(target, rel)
        .with_context(|| format!("create link {rel:?} -> {target}"))?;
    Ok(replaced)
}

/// Remove every entry the restored closure does not contain. A directory that
/// is absent from the closure cannot contain a recorded path (verification
/// requires recorded ancestors), so its whole subtree is extra and is removed
/// as a unit.
fn apply_removals(
    dir: &Dir,
    prefix: &str,
    tree: &BTreeMap<String, CheckpointEntry>,
    depth: usize,
) -> Result<usize> {
    if depth > MAX_TREE_DEPTH {
        bail!(
            "HOME_CHECKPOINT_INCOMPLETE: tree deeper than {MAX_TREE_DEPTH} levels below {prefix:?}"
        );
    }
    let mut names = Vec::new();
    for entry in dir.entries().context("list restore root")? {
        let entry = entry.context("read restore entry")?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            anyhow!("HOME_CHECKPOINT_UNSUPPORTED: non-UTF-8 entry name under {prefix:?}")
        })?;
        names.push(name.to_string());
    }
    let mut removed = 0usize;
    for name in names {
        let rel = rel_join(prefix, &name);
        match tree.get(&rel) {
            Some(entry) if entry.kind == CheckpointEntryKind::Directory => {
                let child = dir.open_dir(&name).with_context(|| format!("open {rel:?}"))?;
                removed += apply_removals(&child, &rel, tree, depth + 1)?;
            }
            Some(_) => {}
            None => {
                match dir.symlink_metadata(&name) {
                    Ok(metadata) if metadata.is_dir() => {
                        dir.remove_dir_all(&name)
                            .with_context(|| format!("remove extra directory {rel:?}"))?;
                    }
                    Ok(_) => {
                        dir.remove_file(&name)
                            .with_context(|| format!("remove extra entry {rel:?}"))?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error).with_context(|| format!("inspect {rel:?}")),
                }
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// The CAS id of a manifest sealed earlier: the canonical serialization is
/// deterministic, so the id is the digest of the same bytes.
fn checkpoint_object_id(cas: &ContentStore, manifest: &CheckpointManifest) -> Result<Digest> {
    let bytes = serde_json::to_vec(manifest).expect("checkpoint manifest serialize");
    let digest = Digest::blake3(&bytes);
    ensure!(
        cas.object_exists(&digest),
        "HOME_CHECKPOINT_INCOMPLETE: closure {digest} is not published in this installation's CAS"
    );
    Ok(digest)
}

pub(crate) fn closure_root(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "DATA_PATH_CONFLICT: restore root {} is a symbolic link; one installation cannot alias another's state",
            dir.display()
        ),
        Ok(_) => bail!(
            "DATA_PATH_CONFLICT: restore root {} exists and is not a directory",
            dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create restore root {}", dir.display()))
        }
        Err(error) => Err(error).with_context(|| format!("inspect restore root {}", dir.display())),
    }
}

fn validate_guest_path(guest: &str) -> Result<()> {
    if !guest.starts_with('/') || guest.contains('\0') {
        bail!("HOME_CHECKPOINT_INVALID: guest mount {guest:?} must be an absolute path");
    }
    if guest.len() > 1 && guest.ends_with('/') {
        bail!("HOME_CHECKPOINT_INVALID: guest mount {guest:?} is not canonical");
    }
    Ok(())
}

/// A closure path is `/`-separated, relative and canonical: no empty, `.` or
/// `..` component and no NUL.
pub(crate) fn validate_rel(rel: &str) -> Result<()> {
    ensure!(
        !rel.is_empty() && !rel.starts_with('/') && !rel.contains('\0'),
        "HOME_CHECKPOINT_INCOMPLETE: closure path {rel:?} is not a relative path"
    );
    for component in rel.split('/') {
        ensure!(
            !component.is_empty() && component != "." && component != "..",
            "HOME_CHECKPOINT_INCOMPLETE: closure path {rel:?} is not canonical"
        );
    }
    Ok(())
}

fn rel_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("checkpoint record has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create checkpoint directory {}", parent.display()))?;
    let temporary = parent.join(format!(".checkpoint-{}.tmp", EntityId::new()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
        .with_context(|| format!("publish {} -> {}", temporary.display(), path.display()))?;
    closure_sync_dir(parent)
}

fn append_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("checkpoint journal has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    closure_sync_dir(parent)
}

pub(crate) fn closure_sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        bail!("HOME_CHECKPOINT_UNSUPPORTED: durable publication requires directory synchronization");
    }
}
