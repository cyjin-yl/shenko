//! Persistent named kernel entries — the REPL kernel *plugin entries* of
//! SPEC §19.1 ("独立可替换的 kernel 插件") as durable installation state.
//!
//! A session's manifest already records the profile it was created with, but
//! that is session state: it cannot answer "which Python/Node/Bun kernels does
//! this installation actually have?" after a restart, and a caller had to know
//! the interpreter path on every `create`. An entry is the missing durable
//! binding: a name, the named §19.1 target it was built from, the real
//! interpreter path, and the digest of the exact bytes.
//!
//! What an entry is and is not:
//!
//! * It binds a **specific interpreter artifact by digest**. Resolving an
//!   entry re-reads and re-hashes the artifact; the session then re-hashes it
//!   again for every cell. A replaced or removed interpreter fails closed
//!   (`REPL_ENTRY_DIGEST_MISMATCH` / `REPL_KERNEL_UNAVAILABLE`) instead of
//!   silently executing a different runtime.
//! * It does **not** copy the interpreter. A native Node/Bun kernel needs its
//!   sibling `npm`/`bun` layout for dependency installation, and a WASI module
//!   is the operator's own artifact; an entry therefore records the canonical
//!   absolute path. Moving or deleting the artifact is a real, reported
//!   failure, not a hidden fallback.
//! * It never upgrades a runtime. `node-wasm`, `bun-wasm` and `pyodide` stay
//!   named-but-unavailable targets; a native host entry keeps saying so.
//!
//! Publication is atomic: the manifest is written and synced inside a
//! temporary directory, the directory is synced, and only then renamed into
//! place, so a crash never leaves a half-written entry at its real name.

use crate::kernel::{KernelProfile, ModuleSource};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Manifest file a kernel entry directory carries.
pub const ENTRY_MANIFEST: &str = "kernel.json";
/// Entry manifest schema. Bump on an incompatible change; an unknown schema is
/// refused rather than reinterpreted.
pub const ENTRY_SCHEMA: u32 = 1;
const MAX_ENTRY_NAME: usize = 64;

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// The durable identity of one REPL kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelEntry {
    pub schema_version: u32,
    /// Entry name (the directory name) the caller asks `create` for.
    pub entry: String,
    /// Named §19.1 target this entry was built from (`python-wasi`,
    /// `python-native-host`, `node-native-host`, `bun-native-host`,
    /// `quickjs-wasm`). Recorded so a reader never has to guess the route from
    /// the profile alone.
    pub target: String,
    /// The exact profile `create` will use. Identical in shape to the profile
    /// the session manifest persists.
    pub profile: KernelProfile,
    /// Canonical absolute path of the real interpreter artifact.
    pub module_path: PathBuf,
    /// BLAKE3 of the artifact bytes at install time.
    pub module_blake3: String,
    pub module_bytes: u64,
    /// Referenced CPython standard library for `python-wasi` (never copied).
    pub stdlib_path: Option<PathBuf>,
    /// Read-only tree digest of `stdlib_path`; a changed tree fails resolve.
    pub stdlib_blake3: Option<String>,
    pub installed_unix_ms: i64,
}

impl KernelEntry {
    /// Recovery level this entry actually delivers, from its real profile.
    pub fn recovery_level(&self) -> crate::contract::RecoveryLevel {
        self.profile.recovery_level()
    }

    /// Session model this entry actually delivers.
    pub fn session_model(&self) -> crate::contract::SessionModel {
        self.profile.session_model()
    }
}

/// Request to publish one entry.
#[derive(Debug, Clone)]
pub struct InstallEntry {
    /// Entry name; `[A-Za-z0-9][A-Za-z0-9._-]*`, at most 64 bytes.
    pub entry: String,
    /// Named §19.1 target (see `kernel::named_targets`).
    pub target: String,
    /// Real interpreter artifact (module `.wasm`, or a native executable).
    pub module: PathBuf,
    /// Optional `python-wasi` standard library directory.
    pub stdlib: Option<PathBuf>,
    /// Replace an existing entry of the same name. The old entry is retired
    /// only after the new one is durable.
    pub replace: bool,
}

/// Cheap, non-committal artifact state for a *listing*. Listing never claims
/// the binding is verified: re-hashing an interpreter (and a CPython standard
/// library) is what `resolve` does for a caller who is about to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    /// Present with the recorded length; the content digest is verified by
    /// `resolve` (and by the session for every cell).
    SizeMatches,
    /// The recorded artifact is not a readable file at its path.
    Missing,
    /// A file exists at the recorded path but its length already differs, so
    /// the binding cannot verify without reading it.
    SizeChanged { recorded: u64, actual: u64 },
}

/// One entry as a listing sees it. A corrupt entry is reported as corrupt with
/// its reason; it is never silently dropped and never listed as usable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryListing {
    Ok {
        entry: KernelEntry,
        artifact: ArtifactState,
    },
    Corrupt { entry: String, detail: String },
}

impl EntryListing {
    pub fn name(&self) -> &str {
        match self {
            EntryListing::Ok { entry, .. } => &entry.entry,
            EntryListing::Corrupt { entry, .. } => entry,
        }
    }
}

/// Validate an entry name: no path separators, no leading dot, no traversal.
pub fn validate_entry_name(entry: &str) -> Result<()> {
    if entry.is_empty() || entry.len() > MAX_ENTRY_NAME {
        bail!("REPL_ENTRY_INVALID: entry name must be 1..={MAX_ENTRY_NAME} bytes");
    }
    let mut chars = entry.chars();
    let first = chars.next().expect("non-empty name");
    if !first.is_ascii_alphanumeric() {
        bail!("REPL_ENTRY_INVALID: entry name {entry:?} must start with an ASCII alphanumeric");
    }
    if !entry
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("REPL_ENTRY_INVALID: entry name {entry:?} may only contain [A-Za-z0-9._-]");
    }
    if entry == "." || entry == ".." {
        bail!("REPL_ENTRY_INVALID: entry name {entry:?} is a directory traversal");
    }
    Ok(())
}

fn entry_dir(root: &Path, entry: &str) -> Result<PathBuf> {
    validate_entry_name(entry)?;
    Ok(root.join(entry))
}

/// Stream an artifact's identity: exact bytes and length, without
/// accumulating the interpreter module in memory.
fn artifact_identity(path: &Path) -> Result<(String, u64)> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("REPL_KERNEL_UNAVAILABLE: {} is not readable", path.display()))?;
    if !metadata.is_file() {
        bail!("REPL_ENTRY_INVALID: {} is not a regular file", path.display());
    }
    crate::journal::hash_reader(File::open(path)?)
}

/// Publish one kernel entry. The interpreter artifact must already exist; this
/// function never downloads or builds one.
pub fn install(root: &Path, request: &InstallEntry) -> Result<KernelEntry> {
    validate_entry_name(&request.entry)?;
    fs::create_dir_all(root)?;
    let root = root.canonicalize().context("canonicalize kernels root")?;
    // Refuse a name collision before reading an interpreter: hashing a large
    // module is real work and must not happen for a request that cannot win.
    let destination = entry_dir(&root, &request.entry)?;
    let existing = destination.exists();
    if existing && !request.replace {
        bail!(
            "REPL_ENTRY_EXISTS: kernel entry {:?} already exists at {}; pass replace to publish a new binding",
            request.entry,
            destination.display()
        );
    }
    let module_path = request.module.canonicalize()
        .with_context(|| format!("REPL_KERNEL_UNAVAILABLE: {} is not readable", request.module.display()))?;
    let (module_blake3, module_bytes) = artifact_identity(&module_path)?;
    let (stdlib_path, stdlib_blake3) = match &request.stdlib {
        Some(stdlib) => {
            let stdlib = stdlib.canonicalize()
                .with_context(|| format!("REPL_KERNEL_UNAVAILABLE: stdlib {} is not readable", stdlib.display()))?;
            if !stdlib.is_dir() {
                bail!("REPL_ENTRY_INVALID: stdlib {} is not a directory", stdlib.display());
            }
            let digest = crate::install::tree_digest(&stdlib)?;
            (Some(stdlib), Some(digest))
        }
        None => (None, None),
    };
    // The profile is built by the same adapter mapping `create` uses, so an
    // entry can never describe a kernel the runtime would build differently.
    let profile = crate::adapters::profile_for_target(
        &request.target,
        request.entry.clone(),
        module_path.clone(),
        stdlib_path.clone(),
    )?;
    // Bind only a native artifact the kernel could actually execute; a WASI
    // module is read by the runtime and needs no execute bit.
    crate::kernel::validate_module_mode(&profile, &module_path)?;
    let entry = KernelEntry {
        schema_version: ENTRY_SCHEMA,
        entry: request.entry.clone(),
        target: request.target.clone(),
        profile,
        module_path,
        module_blake3,
        module_bytes,
        stdlib_path,
        stdlib_blake3,
        installed_unix_ms: unix_ms(),
    };
    // Everything below happens in a temporary directory: a crash before the
    // rename leaves no entry at the real name.
    let staging = root.join(format!(".entry-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging)?;
    let manifest_bytes = serde_json::to_vec_pretty(&entry)?;
    let manifest_path = staging.join(ENTRY_MANIFEST);
    {
        let mut file = File::create(&manifest_path)?;
        use std::io::Write as _;
        file.write_all(&manifest_bytes)?;
        file.sync_all()?;
    }
    File::open(&staging)?.sync_all()?;
    let retired = if existing {
        let retired = root.join(format!(".retired-{}", uuid::Uuid::new_v4()));
        fs::rename(&destination, &retired)?;
        Some(retired)
    } else {
        None
    };
    if let Err(error) = fs::rename(&staging, &destination) {
        // The old entry is still intact under its retirement name: put it
        // back rather than leaving the installation with no entry at all.
        if let Some(retired) = &retired {
            let _ = fs::rename(retired, &destination);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(error).context("publish kernel entry");
    }
    File::open(&root)?.sync_all()?;
    if let Some(retired) = retired {
        // Retired only after the new binding is durable; a leftover retirement
        // directory is inert (listing skips dot-prefixed names).
        let _ = fs::remove_dir_all(retired);
    }
    Ok(entry)
}

fn read_manifest(dir: &Path) -> Result<KernelEntry> {
    let path = dir.join(ENTRY_MANIFEST);
    let bytes = fs::read(&path)
        .with_context(|| format!("REPL_ENTRY_CORRUPT: {} is unreadable", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("REPL_ENTRY_CORRUPT: {} is not JSON", path.display()))?;
    let schema = value.get("schema_version").and_then(serde_json::Value::as_u64);
    if schema != Some(ENTRY_SCHEMA as u64) {
        bail!(
            "REPL_ENTRY_CORRUPT: {} declares schema {:?}, this build reads {}",
            path.display(),
            schema,
            ENTRY_SCHEMA
        );
    }
    let entry: KernelEntry = serde_json::from_value(value)
        .with_context(|| format!("REPL_ENTRY_CORRUPT: {} is not a kernel entry", path.display()))?;
    let directory = dir.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if entry.entry != directory {
        bail!(
            "REPL_ENTRY_CORRUPT: {} names entry {:?} but lives in {:?}",
            path.display(),
            entry.entry,
            directory
        );
    }
    Ok(entry)
}

/// The record must describe itself consistently before any artifact is read:
/// the profile's kernel identity is the entry name and its module is the
/// artifact recorded beside it. A handcrafted manifest that says otherwise is
/// corrupt, not a second binding. This reads the manifest only, so a listing
/// can apply it without hashing an interpreter.
fn validate_record(entry: &KernelEntry) -> Result<()> {
    if entry.profile.kernel_id != entry.entry {
        bail!(
            "REPL_ENTRY_CORRUPT: kernel entry {:?} records profile kernel_id {:?}",
            entry.entry,
            entry.profile.kernel_id
        );
    }
    if profile_module(&entry.profile) != entry.module_path {
        bail!(
            "REPL_ENTRY_CORRUPT: kernel entry {:?} profile module is not the recorded artifact",
            entry.entry
        );
    }
    if matches!(
        (&entry.stdlib_path, &entry.stdlib_blake3),
        (Some(_), None) | (None, Some(_))
    ) {
        bail!(
            "REPL_ENTRY_CORRUPT: kernel entry {:?} has an incomplete stdlib binding",
            entry.entry
        );
    }
    crate::kernel::validate_profile(&entry.profile)?;
    Ok(())
}

/// Re-verify an entry against the installation: the artifact bytes and the
/// stdlib tree must still be the ones the entry was published with.
pub fn verify(entry: &KernelEntry) -> Result<()> {
    validate_record(entry)?;
    let (digest, bytes) = artifact_identity(&entry.module_path)
        .with_context(|| format!("REPL_ENTRY_UNBOUND: kernel entry {:?}", entry.entry))?;
    if digest != entry.module_blake3 || bytes != entry.module_bytes {
        bail!(
            "REPL_ENTRY_DIGEST_MISMATCH: kernel entry {:?} bound {} ({} bytes) at {}, artifact is {} ({} bytes)",
            entry.entry,
            entry.module_blake3,
            entry.module_bytes,
            entry.module_path.display(),
            digest,
            bytes
        );
    }
    if let (Some(path), Some(digest)) = (&entry.stdlib_path, &entry.stdlib_blake3) {
        let actual = crate::install::tree_digest(path)
            .with_context(|| format!("REPL_ENTRY_UNBOUND: stdlib {}", path.display()))?;
        if &actual != digest {
            bail!(
                "REPL_ENTRY_DIGEST_MISMATCH: kernel entry {:?} stdlib {} changed",
                entry.entry,
                path.display()
            );
        }
    }
    Ok(())
}

/// The cheap pre-flight a listing reports: is the recorded artifact still
/// there with the length it was published with? Says nothing about content.
fn artifact_state(entry: &KernelEntry) -> ArtifactState {
    match fs::metadata(&entry.module_path) {
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() == entry.module_bytes {
                ArtifactState::SizeMatches
            } else {
                ArtifactState::SizeChanged {
                    recorded: entry.module_bytes,
                    actual: metadata.len(),
                }
            }
        }
        _ => ArtifactState::Missing,
    }
}

/// Resolve one entry to the profile `create` must use, verifying the binding.
pub fn resolve(root: &Path, entry: &str) -> Result<(KernelProfile, KernelEntry)> {
    let dir = entry_dir(root, entry)?;
    if !dir.join(ENTRY_MANIFEST).is_file() {
        bail!(
            "REPL_ENTRY_UNAVAILABLE: no kernel entry {:?} at {}",
            entry,
            dir.display()
        );
    }
    let record = read_manifest(&dir)?;
    verify(&record)?;
    Ok((record.profile.clone(), record))
}

/// Every entry this installation has, sorted by name. Corrupt entries are
/// reported with their reason instead of being hidden or repaired: every
/// non-hidden child directory that cannot be admitted (including one with no
/// manifest at all) is visible as corrupt, never silently skipped.
pub fn list(root: &Path) -> Result<Vec<EntryListing>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut listings = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }
        let listing = match read_manifest(&entry.path()).and_then(|record| {
            validate_record(&record)?;
            Ok(record)
        }) {
            Ok(record) => EntryListing::Ok {
                artifact: artifact_state(&record),
                entry: record,
            },
            Err(error) => EntryListing::Corrupt {
                entry: name.clone(),
                detail: format!("{error:#}"),
            },
        };
        listings.push(listing);
    }
    listings.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(listings)
}

/// The entry names an installation has, as a name → digest map (used by
/// callers that only need the binding identity).
pub fn digest_map(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for listing in list(root)? {
        if let EntryListing::Ok { entry, .. } = listing {
            map.insert(entry.entry, entry.module_blake3);
        }
    }
    Ok(map)
}

/// Whether the profile's module really is the artifact this entry recorded.
/// Sessions persist the profile, so this is the bridge that lets a reopened
/// session still be checked against its entry.
pub fn profile_module(profile: &KernelProfile) -> &Path {
    match &profile.module {
        ModuleSource::ModulePath(path) => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_refuse_traversal() {
        for name in ["", ".", "..", "a/b", "/abs", ".hidden", "sp ace"] {
            assert!(validate_entry_name(name).is_err(), "{name:?} must be refused");
        }
        for name in ["python-3.13", "node_20", "bun", "a"] {
            assert!(validate_entry_name(name).is_ok(), "{name:?} must be admitted");
        }
    }

    #[test]
    fn corrupt_manifest_schema_is_refused() {
        let root = std::env::temp_dir().join(format!("shenko-entry-{}", uuid::Uuid::new_v4()));
        let dir = root.join("python");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(ENTRY_MANIFEST), br#"{"schema_version":99}"#).unwrap();
        assert!(read_manifest(&dir).is_err());
        // The listing reports the corrupt entry instead of hiding it.
        let listings = list(&root).unwrap();
        assert_eq!(listings.len(), 1);
        assert!(matches!(listings[0], EntryListing::Corrupt { .. }));
        fs::remove_dir_all(&root).unwrap();
    }

    fn fresh_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("shenko-entry-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A real artifact file: content matters (the binding is a digest), not
    /// the format — these tests never execute an interpreter.
    fn artifact(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn request(entry: &str, target: &str, module: PathBuf) -> InstallEntry {
        InstallEntry {
            entry: entry.into(),
            target: target.into(),
            module,
            stdlib: None,
            replace: false,
        }
    }

    /// Publishing binds the exact artifact bytes: a same-length replacement is
    /// still a different interpreter and must fail resolve, while the cheap
    /// listing only reports that nothing changed in size.
    #[test]
    fn resolve_rehashes_the_artifact_and_refuses_a_replacement() {
        let root = fresh_root("resolve");
        let module = artifact(&root, "python.wat", b"(module)");
        let installed = install(&root, &request("python", "python-wasi", module.clone())).unwrap();
        assert_eq!(installed.entry, "python");
        assert_eq!(installed.target, "python-wasi");
        assert_eq!(
            installed.recovery_level(),
            crate::contract::RecoveryLevel::R1ObjectCheckpoint
        );
        assert_eq!(
            installed.session_model(),
            crate::contract::SessionModel::LiveProcess
        );
        let (profile, record) = resolve(&root, "python").unwrap();
        assert_eq!(profile.kernel_id, "python");
        assert_eq!(profile_module(&profile), module.as_path());
        assert_eq!(record.module_blake3, installed.module_blake3);
        // Same length, different bytes.
        fs::write(&module, b"(modulE)").unwrap();
        let error = format!("{:#}", resolve(&root, "python").unwrap_err());
        assert!(error.contains("REPL_ENTRY_DIGEST_MISMATCH"), "{error}");
        let listings = list(&root).unwrap();
        match &listings[0] {
            EntryListing::Ok { artifact, entry } => {
                assert_eq!(*artifact, ArtifactState::SizeMatches, "listing is not a digest claim");
                assert_eq!(entry.entry, "python");
            }
            other => panic!("expected a bound entry, got {other:?}"),
        }
        // A removed artifact is reported as missing, not as a usable binding.
        fs::remove_file(&module).unwrap();
        match &list(&root).unwrap()[0] {
            EntryListing::Ok { artifact, .. } => assert_eq!(*artifact, ArtifactState::Missing),
            other => panic!("expected a bound entry, got {other:?}"),
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// The standard library is part of the binding: the directory is
    /// referenced (never copied) and a changed file invalidates the entry.
    #[test]
    fn stdlib_tree_is_part_of_the_binding() {
        let root = fresh_root("stdlib");
        let module = artifact(&root, "python.wat", b"(module)");
        let stdlib = root.join("stdlib");
        fs::create_dir_all(stdlib.join("encodings")).unwrap();
        fs::write(stdlib.join("os.py"), b"# os\n").unwrap();
        fs::write(stdlib.join("encodings/__init__.py"), b"# enc\n").unwrap();
        let mut install_request = request("python", "python-wasi", module);
        install_request.stdlib = Some(stdlib.join("..").join("stdlib"));
        let installed = install(&root, &install_request).unwrap();
        assert_eq!(installed.stdlib_path.as_deref(), Some(stdlib.as_path()));
        assert!(installed.stdlib_blake3.is_some());
        resolve(&root, "python").unwrap();
        fs::write(stdlib.join("encodings/__init__.py"), b"# changed\n").unwrap();
        let error = format!("{:#}", resolve(&root, "python").unwrap_err());
        assert!(error.contains("REPL_ENTRY_DIGEST_MISMATCH"), "{error}");
        fs::remove_dir_all(&root).unwrap();
    }

    /// Named-but-unavailable routes are refused instead of being satisfied by
    /// another runtime, and a stdlib on a route without one is refused too.
    #[test]
    fn unavailable_targets_and_foreign_stdlib_are_refused() {
        let root = fresh_root("targets");
        // Realistic layout: artifacts and the stdlib live in the operator's
        // admitted kernels tree, bindings live in their own root.
        let bindings = root.join("bindings");
        fs::create_dir_all(&bindings).unwrap();
        let module = artifact(&root, "node.wat", b"(module)");
        for target in ["node-wasm", "bun-wasm", "pyodide", "deno"] {
            let error = format!(
                "{:#}",
                install(&bindings, &request("node", target, module.clone())).unwrap_err()
            );
            assert!(error.contains("REPL_KERNEL_UNAVAILABLE"), "{target}: {error}");
        }
        let stdlib = root.join("stdlib");
        fs::create_dir_all(&stdlib).unwrap();
        fs::write(stdlib.join("os.py"), b"# os\n").unwrap();
        let quickjs = artifact(&root, "qjs.wat", b"(module)");
        let mut install_request = request("qjs", "quickjs-wasm", quickjs);
        install_request.stdlib = Some(stdlib);
        let error = format!("{:#}", install(&bindings, &install_request).unwrap_err());
        assert!(error.contains("REPL_KERNEL_INVALID"), "{error}");
        // Nothing was published by any refusal: the bindings root is empty.
        assert!(list(&bindings).unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Replacing a binding is explicit and atomic: without `replace` the
    /// existing entry is untouched, with it the new digest is the only one.
    #[test]
    fn replace_retires_the_previous_binding_only_after_publication() {
        let root = fresh_root("replace");
        let first = artifact(&root, "python-a.wat", b"(module)");
        let second = artifact(&root, "python-b.wat", b"(module (func))");
        install(&root, &request("python", "python-wasi", first.clone())).unwrap();
        let original = resolve(&root, "python").unwrap().1;
        let error = format!(
            "{:#}",
            install(&root, &request("python", "python-wasi", second.clone())).unwrap_err()
        );
        assert!(error.contains("REPL_ENTRY_EXISTS"), "{error}");
        assert_eq!(resolve(&root, "python").unwrap().1.module_blake3, original.module_blake3);
        let replaced = install(
            &root,
            &InstallEntry {
                entry: "python".into(),
                target: "python-wasi".into(),
                module: second.clone(),
                stdlib: None,
                replace: true,
            },
        )
        .unwrap();
        assert_eq!(replaced.module_path, second);
        assert_ne!(replaced.module_blake3, original.module_blake3);
        let listings = list(&root).unwrap();
        assert_eq!(listings.len(), 1, "a replaced binding leaves one entry: {listings:?}");
        // Retirement directories are dot-prefixed and never listed.
        let stray = fs::read_dir(&root).unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".entry-"))
            .count();
        assert_eq!(stray, 0, "staging directories were retired");
        fs::remove_dir_all(&root).unwrap();
    }

    /// A handcrafted manifest cannot point the binding at another artifact or
    /// rename the kernel: both are integrity failures, not second bindings.
    #[test]
    fn handcrafted_manifest_cannot_redirect_the_binding() {
        let root = fresh_root("tamper");
        let module = artifact(&root, "python.wat", b"(module)");
        install(&root, &request("python", "python-wasi", module)).unwrap();
        let manifest = root.join("python").join(ENTRY_MANIFEST);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value["profile"]["kernel_id"] = serde_json::json!("other");
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = format!("{:#}", resolve(&root, "python").unwrap_err());
        assert!(error.contains("REPL_ENTRY_CORRUPT"), "{error}");
        match &list(&root).unwrap()[0] {
            EntryListing::Corrupt { detail, .. } =>
                assert!(detail.contains("REPL_ENTRY_CORRUPT"), "{detail}"),
            other => panic!("a tampered binding must not list as usable: {other:?}"),
        }
        fs::remove_dir_all(&root).unwrap();
    }

    /// A native binding must point at an artifact the kernel can actually
    /// exec; a WASI module is read by the runtime and needs no execute bit.
    /// The refusal publishes nothing.
    #[test]
    fn native_binding_requires_an_executable_artifact() {
        use std::os::unix::fs::PermissionsExt;
        let root = fresh_root("native-mode");
        // Artifacts and bindings live in separate roots, as in an installation.
        let bindings = root.join("bindings");
        fs::create_dir_all(&bindings).unwrap();
        let module = artifact(&root, "node", b"not an interpreter");
        fs::set_permissions(&module, fs::Permissions::from_mode(0o644)).unwrap();
        let error = format!(
            "{:#}",
            install(&bindings, &request("node", "node-native-host", module.clone())).unwrap_err()
        );
        assert!(error.contains("REPL_KERNEL_UNAVAILABLE"), "{error}");
        assert!(error.contains("executable"), "{error}");
        assert!(list(&bindings).unwrap().is_empty(), "a refused binding publishes nothing");
        fs::set_permissions(&module, fs::Permissions::from_mode(0o755)).unwrap();
        install(&bindings, &request("node", "node-native-host", module)).unwrap();
        assert_eq!(list(&bindings).unwrap().len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }
}
