//! Immutable, portable admission contract for isolated process plugins.
//!
//! This describes bytes and exports, never installation state, credentials,
//! health, publication or an execution grant. Node resolves every object from
//! the selected UnitPackage closure; no reference names a host program path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionedRef};

pub const PROCESS_LOAD_SCHEMA: &str = "shenko.process-load";
/// The manifest major also versions the stdio JSONL protocol it selects.
pub const PROCESS_LOAD_MAJOR: u32 = 1;
pub const MAX_PROCESS_LOAD_BYTES: usize = 64 * 1024;
pub const MAX_PROCESS_FILES: usize = 64;
pub const MAX_PROCESS_EXPORTS: usize = 64;
pub const MAX_PROCESS_EFFECTS: usize = 64;
pub const MAX_PROCESS_PATH_BYTES: usize = 512;
pub const MAX_PROCESS_PATH_COMPONENT_BYTES: usize = 255;
/// The existing Node inline object namespace, not another object store.
pub const PROCESS_OBJECT_NAMESPACE: &str = "node-inline-v1";

#[derive(Debug, Error)]
pub enum ProcessLoadError {
    #[error("PROCESS_LOAD_SIZE: manifest exceeds the byte bound")]
    TooLarge,
    #[error("PROCESS_LOAD_JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("PROCESS_LOAD_VERSION: unsupported manifest schema or major")]
    UnsupportedVersion,
    #[error("PROCESS_LOAD_PLATFORM: executable does not target this native host")]
    UnsupportedPlatform,
    #[error("PROCESS_LOAD_INVALID: {0}")]
    Invalid(String),
}

/// Deserialize an object directly from its map. Going through Value would
/// erase duplicate keys; derived struct visitors also accept sequence input.
fn map_only<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    struct ObjectVisitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: DeserializeOwned> Visitor<'de> for ObjectVisitor<T> {
        type Value = T;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique, known fields")
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
}

// Keep these strict readers local. Existing ObjectRef/VersionedRef consumers
// retain their contracts; this protocol admits all of their nested fields
// directly, including SchemaRef and ArtifactRef, without a Value round trip.
fn schema_ref<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SchemaRef, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields { name: String, major: u32, digest: Digest }
    let value: Fields = map_only(deserializer)?;
    Ok(SchemaRef { name: value.name, major: value.major, digest: value.digest })
}

fn artifact_ref<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ArtifactRef, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields { digest: Digest, locator: String }
    let value: Fields = map_only(deserializer)?;
    Ok(ArtifactRef { digest: value.digest, locator: value.locator })
}

fn object_ref<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ObjectRef, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        digest: Digest,
        bytes: u64,
        media_type: String,
        store_namespace: String,
        locator: String,
        #[serde(deserialize_with = "schema_ref")]
        codec: SchemaRef,
    }
    let value: Fields = map_only(deserializer)?;
    Ok(ObjectRef { digest: value.digest, bytes: value.bytes, media_type: value.media_type,
        store_namespace: value.store_namespace, locator: value.locator, codec: value.codec })
}

fn versioned_ref<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VersionedRef, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        #[serde(deserialize_with = "artifact_ref")]
        artifact: ArtifactRef,
        symbol: String,
        #[serde(deserialize_with = "schema_ref")]
        schema: SchemaRef,
    }
    let value: Fields = map_only(deserializer)?;
    Ok(VersionedRef { artifact: value.artifact, symbol: value.symbol, schema: value.schema })
}

struct StrictObject(ObjectRef);
impl<'de> Deserialize<'de> for StrictObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        object_ref(deserializer).map(Self)
    }
}
fn object_refs<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<ObjectRef>, D::Error> {
    Vec::<StrictObject>::deserialize(deserializer)
        .map(|values| values.into_iter().map(|value| value.0).collect())
}
struct StrictVersioned(VersionedRef);
impl<'de> Deserialize<'de> for StrictVersioned {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        versioned_ref(deserializer).map(Self)
    }
}
fn optional_versioned_ref<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<VersionedRef>, D::Error> {
    Option::<StrictVersioned>::deserialize(deserializer).map(|value| value.map(|value| value.0))
}

// All protocol objects use the same map-only, duplicate-preserving reader.
macro_rules! protocol_object {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$field_meta:meta])* pub $field:ident: $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        pub struct $name { $($(#[$field_meta])* pub $field: $ty),* }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields { $($(#[$field_meta])* $field: $ty),* }
                let value: Fields = map_only(deserializer)?;
                Ok(Self { $($field: value.$field),* })
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessProtocol { StdioJsonl }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRuntime { Native }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole { Task, Provider, Channel }

protocol_object! {
    /// Exact executable target. UnitDescriptor applicability remains advisory;
    /// these immutable load requirements must match before process creation.
    pub struct ProcessPlatform {
        pub os: String,
        pub arch: String,
        pub runtime: ProcessRuntime,
    }
}
impl ProcessPlatform {
    pub fn native_host() -> Self {
        Self { os: std::env::consts::OS.into(), arch: std::env::consts::ARCH.into(), runtime: ProcessRuntime::Native }
    }
}

protocol_object! {
    /// One regular file to stage below a loader-owned immutable directory.
    /// Paths are portable relative destinations, never source locators. The
    /// executable is the only file which the loader may launch.
    pub struct ProcessFile {
        pub path: String,
        #[serde(deserialize_with = "object_ref")]
        pub object: ObjectRef,
    }
}

protocol_object! {
    /// A role's immutable owning-domain descriptor and explanatory/effect
    /// documents. The owning adapter must decode and admit those schemas
    /// before it registers any real handler. This manifest advertises no
    /// healthy/discoverable state and cannot supply authority or CallContext.
    pub struct ProcessExport {
        pub role: ProcessRole,
        /// Unique within the manifest, including across different roles.
        pub export_id: String,
        #[serde(deserialize_with = "object_ref")]
        pub descriptor: ObjectRef,
        #[serde(deserialize_with = "versioned_ref")]
        pub implementation: VersionedRef,
        /// Empty is explicit for an export declaring no effect contracts.
        #[serde(deserialize_with = "object_refs")]
        pub effects: Vec<ObjectRef>,
        #[serde(deserialize_with = "object_ref")]
        pub help: ObjectRef,
        #[serde(deserialize_with = "versioned_ref")]
        pub receipt_reader: VersionedRef,
        #[serde(default, deserialize_with = "optional_versioned_ref")]
        pub reconcile: Option<VersionedRef>,
    }
}

protocol_object! {
    /// The Code unit artifact itself is this manifest. Its digest therefore
    /// differs from the executable's digest and is retained independently.
    /// Version 1 selects isolated native-process stdio JSONL, never Rust ABI.
    pub struct ProcessLoad {
        pub schema: String,
        pub major: u32,
        pub protocol: ProcessProtocol,
        pub platform: ProcessPlatform,
        pub executable: ProcessFile,
        pub files: Vec<ProcessFile>,
        pub exports: Vec<ProcessExport>,
    }
}

fn invalid(message: &str) -> ProcessLoadError { ProcessLoadError::Invalid(message.into()) }

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn validate_schema(schema: &SchemaRef) -> Result<(), ProcessLoadError> {
    if !identifier(&schema.name) || schema.major == 0 || schema.digest.canonical_blake3_hex().is_none() {
        return Err(invalid("invalid versioned schema identity"));
    }
    Ok(())
}

fn validate_object(reference: &ObjectRef) -> Result<(), ProcessLoadError> {
    if reference.digest.canonical_blake3_hex().is_none()
        || reference.store_namespace != PROCESS_OBJECT_NAMESPACE
        || reference.locator != format!("sqlite:objects/{}", reference.digest)
        || reference.media_type.is_empty() || reference.media_type.len() > 256
        || reference.media_type.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("object must name canonical Node inline bytes, without a host locator"));
    }
    validate_schema(&reference.codec)
}

fn validate_symbol(reference: &VersionedRef, executable: &ObjectRef) -> Result<(), ProcessLoadError> {
    if reference.artifact.digest != executable.digest
        || reference.artifact.locator != executable.locator || !identifier(&reference.symbol)
    {
        return Err(invalid("implementation/reader/reconcile must name an executable symbol"));
    }
    validate_schema(&reference.schema)
}

fn portable_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PROCESS_PATH_BYTES { return false; }
    path.split('/').all(|part| {
        if part.is_empty() || part.len() > MAX_PROCESS_PATH_COMPONENT_BYTES
            || matches!(part, "." | "..") || part.ends_with('.')
            || !part.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        { return false; }
        // Windows device names alias ordinary paths even with an extension.
        let stem = part.split('.').next().unwrap_or("").to_ascii_lowercase();
        !matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            && !(stem.len() == 4 && (stem.starts_with("com") || stem.starts_with("lpt"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    })
}

impl ProcessLoad {
    /// Decode original bytes directly; rejects unknown/duplicate fields at
    /// every nested protocol/reference object, array-shaped objects, trailing
    /// documents and unsupported majors. No permissive Value intermediate.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProcessLoadError> {
        if bytes.len() > MAX_PROCESS_LOAD_BYTES { return Err(ProcessLoadError::TooLarge); }
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let load = Self::deserialize(&mut deserializer)?;
        deserializer.end()?;
        load.validate()?;
        Ok(load)
    }

    /// Every byte object referenced by the manifest. SchemaRef digests name
    /// owning-domain schemas; they are schema identities, not extra file
    /// downloads. The owning adapter admits its supported schema separately.
    pub fn objects(&self) -> impl Iterator<Item = &ObjectRef> {
        std::iter::once(&self.executable.object)
            .chain(self.files.iter().map(|file| &file.object))
            .chain(self.exports.iter().flat_map(|export| {
                std::iter::once(&export.descriptor).chain(std::iter::once(&export.help)).chain(export.effects.iter())
            }))
    }

    pub fn validate(&self) -> Result<(), ProcessLoadError> {
        if self.schema != PROCESS_LOAD_SCHEMA || self.major != PROCESS_LOAD_MAJOR {
            return Err(ProcessLoadError::UnsupportedVersion);
        }
        if !identifier(&self.platform.os) || !identifier(&self.platform.arch) {
            return Err(invalid("invalid exact platform identity"));
        }
        if self.files.len() > MAX_PROCESS_FILES || self.exports.is_empty() || self.exports.len() > MAX_PROCESS_EXPORTS {
            return Err(invalid("file/export count out of bounds"));
        }
        let mut paths = BTreeSet::new();
        for file in std::iter::once(&self.executable).chain(&self.files) {
            if !portable_path(&file.path) || !paths.insert(file.path.to_ascii_lowercase()) {
                return Err(invalid("invalid or duplicate portable file destination"));
            }
        }
        for path in &paths {
            for (index, _) in path.match_indices('/') {
                if paths.contains(&path[..index]) { return Err(invalid("file destination is an ancestor of another file")); }
            }
        }
        if self.executable.object.bytes == 0 { return Err(invalid("empty executable")); }
        let mut objects = BTreeMap::new();
        for object in self.objects() {
            validate_object(object)?;
            if let Some(previous) = objects.insert(&object.digest, object)
                && previous != object
            {
                return Err(invalid("one object digest has conflicting reference identities"));
            }
        }
        let mut ids = BTreeSet::new();
        let mut implementations = BTreeSet::new();
        for export in &self.exports {
            if !identifier(&export.export_id) || !ids.insert(&export.export_id)
                || !implementations.insert(&export.implementation.symbol)
            { return Err(invalid("invalid or duplicate export identity/implementation symbol")); }
            validate_symbol(&export.implementation, &self.executable.object)?;
            validate_symbol(&export.receipt_reader, &self.executable.object)?;
            if let Some(reconcile) = &export.reconcile { validate_symbol(reconcile, &self.executable.object)?; }
            if export.effects.len() > MAX_PROCESS_EFFECTS { return Err(invalid("effect count out of bounds")); }
            let mut effects = BTreeSet::new();
            for effect in &export.effects {
                if !effects.insert(&effect.digest) { return Err(invalid("duplicate effect contract identity")); }
            }
        }
        Ok(())
    }

    pub fn validate_native_host(&self) -> Result<(), ProcessLoadError> {
        self.validate()?;
        if self.platform != ProcessPlatform::native_host() { return Err(ProcessLoadError::UnsupportedPlatform); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reference(bytes: &[u8]) -> ObjectRef {
        let digest = Digest::blake3(bytes);
        ObjectRef { locator: format!("sqlite:objects/{digest}"), digest, bytes: bytes.len() as u64,
            media_type: "application/octet-stream".into(), store_namespace: PROCESS_OBJECT_NAMESPACE.into(),
            codec: SchemaRef { name: "raw".into(), major: 1, digest: Digest::blake3(b"raw") } }
    }
    fn manifest() -> ProcessLoad {
        let executable = reference(b"native program");
        let symbol = |name: &str| VersionedRef { artifact: ArtifactRef {
            digest: executable.digest.clone(), locator: executable.locator.clone() }, symbol: name.into(),
            schema: SchemaRef { name: "example.task".into(), major: 1, digest: Digest::blake3(b"example.task/v1") } };
        ProcessLoad { schema: PROCESS_LOAD_SCHEMA.into(), major: PROCESS_LOAD_MAJOR,
            protocol: ProcessProtocol::StdioJsonl, platform: ProcessPlatform::native_host(),
            executable: ProcessFile { path: "bin/plugin".into(), object: executable.clone() }, files: vec![],
            exports: vec![ProcessExport { role: ProcessRole::Task, export_id: "example.task".into(),
                descriptor: reference(b"task descriptor"), implementation: symbol("execute"), effects: vec![],
                help: reference(b"task help"), receipt_reader: symbol("read_receipt"), reconcile: None }] }
    }

    #[test]
    fn process_load_v1_roundtrip_preserves_hash_identities() {
        let load = manifest();
        let bytes = serde_json::to_vec(&load).unwrap();
        assert_eq!(ProcessLoad::decode(&bytes).unwrap(), load);
        assert_ne!(Digest::blake3(&bytes), load.executable.object.digest);
        load.validate_native_host().unwrap();
    }

    #[test]
    fn process_load_rejects_duplicate_unknown_sequence_and_trailing_objects() {
        let text = serde_json::to_string(&manifest()).unwrap();
        let digest = serde_json::to_string(&manifest().executable.object.digest).unwrap();
        let bad = [
            text.replacen("\"major\":1", "\"major\":1,\"major\":1", 1),
            text.replacen("\"path\":", "\"unknown\":0,\"path\":", 1),
            text.replacen(&format!("\"digest\":{digest}"), &format!("\"digest\":{digest},\"digest\":{digest}"), 1),
            text.replacen("\"codec\":{", "\"codec\":{\"unknown\":0,", 1),
            text.replacen("\"artifact\":{", "\"artifact\":{\"unknown\":0,", 1),
            text.replacen("\"schema\":{", "\"schema\":{\"unknown\":0,", 1),
            text.replacen("\"bytes\":", "\"locator\":\"x\",\"bytes\":", 1),
            format!("{text} {{}}"),
            "[]".into(),
        ];
        for bytes in bad { assert!(ProcessLoad::decode(bytes.as_bytes()).is_err(), "{bytes}"); }
        let mut document = serde_json::to_value(manifest()).unwrap();
        document["executable"]["object"] = json!([]);
        assert!(ProcessLoad::decode(&serde_json::to_vec(&document).unwrap()).is_err());
    }

    #[test]
    fn process_load_rejects_unsupported_major_protocol_runtime_and_host() {
        let mut document = serde_json::to_value(manifest()).unwrap();
        for (field, value) in [("major", json!(2)), ("protocol", json!("rust_abi")), ("schema", json!("other"))] {
            let mut changed = document.clone(); changed[field] = value;
            assert!(ProcessLoad::decode(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
        document["platform"]["runtime"] = json!("python");
        assert!(ProcessLoad::decode(&serde_json::to_vec(&document).unwrap()).is_err());
        let mut load = manifest(); load.platform.os = "unavailable-os".into();
        assert!(matches!(load.validate_native_host(), Err(ProcessLoadError::UnsupportedPlatform)));
    }

    #[test]
    fn process_load_rejects_paths_aliases_and_duplicate_exports() {
        let mut component = manifest();
        component.executable.path = format!("bin/{}", "a".repeat(255));
        component.validate().unwrap();
        component.executable.path.push('a');
        assert!(component.validate().is_err(), "a 256-byte component is not portable even below the total path bound");
        for path in ["/bin/plugin", "../plugin", "bin/../plugin", "bin//plugin", "C:/plugin", "bin\\plugin", "bin/CON.exe", "bin/plugin."] {
            let mut load = manifest(); load.executable.path = path.into();
            assert!(load.validate().is_err(), "{path}");
        }
        let mut load = manifest();
        load.files.push(ProcessFile { path: "BIN/PLUGIN".into(), object: reference(b"other") });
        assert!(load.validate().is_err());
        load.files[0].path = "bin".into();
        assert!(load.validate().is_err());
        load.files.clear(); load.exports.push(load.exports[0].clone());
        load.exports[1].role = ProcessRole::Channel;
        assert!(load.validate().is_err());
    }

    #[test]
    fn process_load_rejects_host_locators_reference_conflicts_and_unbound_symbols() {
        let mut load = manifest(); load.exports[0].help.locator = "/host/help".into();
        assert!(load.validate().is_err());
        let mut load = manifest();
        let mut alias = load.exports[0].help.clone(); alias.bytes += 1;
        load.exports[0].effects.push(alias);
        assert!(load.validate().is_err());
        let mut load = manifest();
        load.exports[0].receipt_reader.artifact.digest = Digest::blake3(b"unselected binary");
        assert!(load.validate().is_err());
    }
}
