//! The Finder-like file view: every file system the subject can reach.
//!
//! Roots are facts, never guesses. The subject's WASM home and its other
//! declared mounts come from the validated startup closure. A mount's network
//! protocol (sshfs, NFS, SMB, a hypervisor shared folder) comes from the
//! host's own mount table for the mount's source. Other nodes come from this
//! node's sync-peer records, wired in by the runtime.
//!
//! Browsing walks from a directory capability admitted the way the guest's
//! preopens are (`OwnedDir`: per-component, no symlink following), so the
//! view can never reach outside a declared root through a link or a `..`.
//! The owner's view is read-only; writing is the subject's effect.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use shenko_wasm::mount::{MountMode, MountSpec, OwnedDir};

/// What the runtime knows about another node this one syncs with.
#[derive(Debug, Clone, Serialize)]
pub struct FilePeer {
    pub node_id: String,
    pub ssh_target: String,
    pub workspaces: Vec<PathBuf>,
    pub remote_data_dir: PathBuf,
    /// When this node last confirmed the peer, if ever.
    pub confirmed_at: Option<String>,
    /// The last failure reaching it, if the most recent attempt failed.
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// The subject's own WASM home, `/home`.
    Home,
    /// Another mount the home manifest declares (`/memory`, workspaces…).
    Mount,
    /// A file system another node reaches.
    Node,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRoot {
    pub id: String,
    pub label: String,
    pub kind: RootKind,
    /// `wasm_home`, `local`, `sshfs`, `nfs`, `smb`, `shared_folder`, `ssh`, or
    /// the host's own type name when it is none of these.
    pub protocol: String,
    /// The path the subject sees, for mounts; the remote path for nodes.
    pub guest_path: String,
    pub mode: Option<String>,
    pub writable: bool,
    /// Whether the owner can browse it from this page now.
    pub browsable: bool,
    /// Why it cannot be browsed, or what the view shows of it.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    /// Lossless native component token; display names are never path authority.
    pub id: String,
    pub name: String,
    /// `dir`, `file`, `symlink` or `other`.
    pub kind: &'static str,
    pub bytes: u64,
    pub modified: Option<String>,
}

/// Immutable read-only capabilities captured by the normal startup, after
/// installation/home validation. No request reopens a manifest or host root.
pub struct FileMounts {
    mounts: Vec<FileMount>,
}

struct FileMount {
    spec: MountSpec,
    root: OwnedDir,
    protocol: String,
}

impl FileMounts {
    /// Call only with the same validated closure supplied to wasi.exec. Pin
    /// sources before publishing the UI; an admission failure aborts startup.
    /// COW roots expose only their base, without taking another writer claim.
    pub fn from_validated_startup(mounts: &[MountSpec]) -> anyhow::Result<Self> {
        let mut admitted = Vec::with_capacity(mounts.len());
        for spec in mounts {
            let root = OwnedDir::admit_absolute(&spec.source)?;
            let protocol = if spec.guest_path == "/home" {
                "wasm_home".to_owned()
            } else {
                host_fs_type(&spec.source).map_or_else(|| "local".to_owned(), |fs| protocol_for(&fs).to_owned())
            };
            admitted.push(FileMount { spec: spec.clone(), root, protocol });
        }
        admitted.sort_by(|a, b| (a.spec.guest_path != "/home", &a.spec.guest_path)
            .cmp(&(b.spec.guest_path != "/home", &b.spec.guest_path)));
        Ok(Self { mounts: admitted })
    }
}

/// The file-system type the host reports for the mount holding `path`, read
/// from `/proc/self/mountinfo` (longest matching mount point wins).
fn host_fs_type(path: &Path) -> Option<String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let (Some(point), Some(sep)) = (fields.get(4), fields.iter().position(|field| *field == "-")) else { continue };
        let point = unescape_mount_path(point);
        let Some(fs_type) = fields.get(sep + 1) else { continue };
        if path.starts_with(&point) && best.as_ref().is_none_or(|(len, _)| point.len() >= *len) {
            best = Some((point.len(), (*fs_type).to_owned()));
        }
    }
    best.map(|(_, fs_type)| fs_type)
}

fn unescape_mount_path(raw: &str) -> String {
    raw.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}

/// Name the protocol behind a host file-system type.
pub fn protocol_for(fs_type: &str) -> &str {
    match fs_type {
        "fuse.sshfs" | "sshfs" => "sshfs",
        "nfs" | "nfs4" => "nfs",
        "cifs" | "smb3" | "smbfs" => "smb",
        "9p" | "virtiofs" | "vboxsf" | "vmhgfs" | "fuse.vmhgfs-fuse" | "prl_fs" | "fuse.osxfs" | "fakeowner" => "shared_folder",
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "apfs" | "tmpfs" | "overlay" | "f2fs" | "bcachefs" => "local",
        other => other,
    }
}

fn mode_name(mode: &MountMode) -> &'static str {
    match mode {
        MountMode::ReadOnly => "read_only",
        MountMode::ReadWrite => "read_write",
        MountMode::CopyOnWrite => "copy_on_write",
    }
}

/// Every root this page can show, in sidebar order: home, other mounts, nodes.
pub fn roots(authority: &FileMounts, peers: &[FilePeer]) -> Vec<FileRoot> {
    let mut roots = Vec::new();
    for admitted in &authority.mounts {
        let mount = &admitted.spec;
        let home = mount.guest_path == "/home";
        roots.push(FileRoot {
            id: format!("mount:{}", mount.guest_path),
            label: mount.guest_path.trim_start_matches('/').to_owned(),
            kind: if home { RootKind::Home } else { RootKind::Mount },
            protocol: admitted.protocol.clone(),
            guest_path: mount.guest_path.clone(),
            mode: Some(mode_name(&mount.mode).to_owned()),
            writable: mount.writable,
            browsable: true,
            note: (mount.mode == MountMode::CopyOnWrite)
                .then(|| "COW 挂载：此处显示只读基底，不合成本分支的覆盖层".to_owned()),
        });
    }
    for peer in peers {
        for path in std::iter::once(peer.remote_data_dir.join("home")).chain(peer.workspaces.iter().cloned()) {
            roots.push(FileRoot {
                id: format!("node:{}:{}", peer.node_id, path.display()),
                label: format!("{} · {}", peer.ssh_target, path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()),
                kind: RootKind::Node,
                protocol: "ssh".to_owned(),
                guest_path: path.display().to_string(),
                mode: None,
                writable: false,
                browsable: false,
                note: Some(match (&peer.failure, &peer.confirmed_at) {
                    (Some(failure), _) => format!("最近一次连接失败：{failure}"),
                    (None, Some(at)) => format!("{at} 确认可达；从此页浏览其他节点的文件尚未接通"),
                    (None, None) => "尚未确认可达；从此页浏览其他节点的文件尚未接通".to_owned(),
                }),
            });
        }
    }
    roots
}

/// At most 256 entries are inspected and returned, plus one lookahead name.
/// This is a bounded, explicitly partial view, not a directory snapshot/page.
pub const MAX_LIST_ENTRIES: usize = 256;
pub const MAX_LIST_BYTES: usize = 256 * 1024;
const MAX_COMPONENT_BYTES: usize = 4096;
const MAX_PATH_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize)]
pub struct FileListing {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

/// Hex is an opaque native component identity, separate from its display.
fn native_id(name: &OsStr) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut id = String::with_capacity(name.as_bytes().len() * 2);
        for byte in name.as_bytes() {
            id.push(HEX[(byte >> 4) as usize] as char);
            id.push(HEX[(byte & 15) as usize] as char);
        }
        id
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut id = String::with_capacity(name.len() * 4);
        for unit in name.encode_wide() {
            for shift in [12, 8, 4, 0] {
                id.push(HEX[((unit >> shift) & 15) as usize] as char);
            }
        }
        id
    }
}

fn native_name(id: &str) -> Result<OsString, &'static str> {
    if id.is_empty() || id.len() > MAX_COMPONENT_BYTES * 2
        || !id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return Err("文件名称标识无效");
    }
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        if !id.len().is_multiple_of(2) { return Err("文件名称标识无效"); }
        let bytes: Result<Vec<_>, _> = (0..id.len()).step_by(2)
            .map(|index| u8::from_str_radix(&id[index..index + 2], 16)).collect();
        let bytes = bytes.map_err(|_| "文件名称标识无效")?;
        if bytes.contains(&0) { return Err("文件名称标识无效"); }
        OsString::from_vec(bytes)
    };
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        if id.len() % 4 != 0 { return Err("文件名称标识无效"); }
        let units: Result<Vec<_>, _> = (0..id.len()).step_by(4)
            .map(|index| u16::from_str_radix(&id[index..index + 4], 16)).collect();
        let units = units.map_err(|_| "文件名称标识无效")?;
        if units.contains(&0) { return Err("文件名称标识无效"); }
        OsString::from_wide(&units)
    };
    let mut components = Path::new(&name).components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part == name.as_os_str())
        || components.next().is_some() {
        return Err("路径只能是根之下的普通名称");
    }
    Ok(name)
}

/// A path is a sequence of native component tokens, never display labels.
fn relative(path: &str) -> Result<PathBuf, &'static str> {
    if path.len() > MAX_PATH_BYTES { return Err("文件路径过长"); }
    let mut out = PathBuf::new();
    if path.is_empty() { return Ok(out); }
    for component in path.split('/') {
        out.push(native_name(component)?);
    }
    Ok(out)
}

/// A bounded prefix in native enumeration order, displayed directories first.
/// No cursor is promised: re-reading a changing directory is not pagination.
pub fn list(authority: &FileMounts, root_id: &str, path: &str) -> Result<FileListing, String> {
    let guest = root_id.strip_prefix("mount:").ok_or("只能浏览本节点的挂载")?;
    let mount = authority.mounts.iter()
        .find(|mount| mount.spec.guest_path == guest)
        .ok_or("没有这个挂载")?;
    let rel = relative(path)?;
    let dir = OwnedDir::admit_relative(&mount.root, &rel).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    let mut truncated = false;
    // Reserve framing overhead so the serialized successful response stays
    // below MAX_LIST_BYTES even when JSON must escape every display byte.
    let mut bytes = 1024;
    for entry in dir.dir().entries().map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entries.len() == MAX_LIST_ENTRIES { truncated = true; break; }
        let native = entry.file_name();
        if native.as_encoded_bytes().len() > MAX_COMPONENT_BYTES { truncated = true; break; }
        let id = native_id(&native);
        if id.len() > MAX_COMPONENT_BYTES * 2 { truncated = true; break; }
        let name = native.to_string_lossy().into_owned();
        let meta = entry.metadata().map_err(|error| error.to_string())?;
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() { "symlink" } else if file_type.is_dir() { "dir" } else if file_type.is_file() { "file" } else { "other" };
        let modified = meta.modified().ok().map(|time| chrono::DateTime::<chrono::Utc>::from(time.into_std()).to_rfc3339());
        let entry = FileEntry { id, name, kind, bytes: if kind == "file" { meta.len() } else { 0 }, modified };
        // Every UTF-8 byte needs at most six JSON bytes; fixed fields, size
        // and RFC3339 metadata fit in the remaining 256 bytes per entry.
        let size = 256 + entry.id.len() + entry.name.len() * 6;
        if bytes + size > MAX_LIST_BYTES { truncated = true; break; }
        bytes += size;
        entries.push(entry);
    }
    entries.sort_by(|a, b| (a.kind != "dir").cmp(&(b.kind != "dir"))
        .then_with(|| a.name.chars().flat_map(char::to_lowercase).cmp(b.name.chars().flat_map(char::to_lowercase)))
        .then_with(|| a.id.cmp(&b.id)));
    Ok(FileListing { entries, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shenko-files-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn authority(home: &Path) -> FileMounts {
        FileMounts::from_validated_startup(&[MountSpec::new(
            home.to_path_buf(), "/home", MountMode::ReadWrite, true, false,
        )]).unwrap()
    }

    #[test]
    fn browsing_stays_inside_the_root_and_names_protocols_from_the_mount_table() {
        let dir = fixture();
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::write(home.join("notes/a.md"), "# a").unwrap();
        std::fs::write(dir.join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.txt"), home.join("escape")).unwrap();
        std::os::unix::fs::symlink(&dir, home.join("up")).unwrap();
        std::fs::write(dir.join("home-manifest.json"), serde_json::json!({
            "mounts": [{"source": home, "guest_path": "/home", "mode": "read_write", "persistent": true, "versioned": false}]
        }).to_string()).unwrap();

        let admitted = authority(&home);
        let root = &roots(&admitted, &[])[0];
        assert_eq!((root.kind.clone(), root.protocol.as_str()), (RootKind::Home, "wasm_home"));
        let top = list(&admitted, "mount:/home", "").unwrap();
        assert!(!top.truncated);
        let names: Vec<_> = top.entries.iter().map(|entry| (entry.name.as_str(), entry.kind)).collect();
        assert_eq!(names, [("notes", "dir"), ("escape", "symlink"), ("up", "symlink")]);
        assert_eq!(list(&admitted, "mount:/home", &native_id(OsStr::new("notes"))).unwrap().entries[0].name, "a.md");
        assert!(list(&admitted, "mount:/home", "../").is_err(), "a parent component is refused");
        for name in [".", "..", "/", "notes/../", "\0"] {
            assert!(list(&admitted, "mount:/home", &native_id(OsStr::new(name))).is_err());
        }
        assert!(list(&admitted, "mount:/home", &native_id(OsStr::new("up"))).is_err(), "a symlinked directory is not followed");
        let file_path = ["notes", "a.md"].map(|name| native_id(OsStr::new(name))).join("/");
        let error = list(&admitted, "mount:/home", &file_path).unwrap_err();
        assert!(error.contains("is not a directory"), "{error}");
        assert_eq!((protocol_for("fuse.sshfs"), protocol_for("nfs4"), protocol_for("cifs"), protocol_for("virtiofs")),
            ("sshfs", "nfs", "smb", "shared_folder"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn startup_capability_survives_manifest_and_source_path_replacement() {
        let dir = fixture();
        let home = dir.join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("admitted"), "original").unwrap();
        let admitted = authority(&home);
        let pinned = dir.join("pinned");
        std::fs::rename(&home, &pinned).unwrap();
        let foreign = dir.join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("foreign-only"), "outside").unwrap();
        std::os::unix::fs::symlink(&foreign, &home).unwrap();
        std::fs::write(dir.join("home-manifest.json"), serde_json::json!({
            "mounts": [{"source": foreign, "guest_path": "/home", "mode": "read_write", "persistent": true, "versioned": false}]
        }).to_string()).unwrap();
        assert_eq!(roots(&admitted, &[])[0].id, "mount:/home");
        let listing = list(&admitted, "mount:/home", "").unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "admitted");
        assert!(!listing.truncated);
        assert!(FileMounts::from_validated_startup(&[MountSpec::new(
            home, "/home", MountMode::ReadOnly, true, false,
        )]).is_err(), "a new admission must refuse the substituted symlink");
        drop(admitted);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bounded_listings_round_trip_native_names_without_display_collisions() {
        use std::os::unix::ffi::OsStringExt;
        let dir = fixture();
        let home = dir.join("home");
        std::fs::create_dir(&home).unwrap();
        let names = [OsString::from_vec(vec![0xff]), OsString::from_vec(vec![0xfe]), OsString::from("\u{fffd}")];
        for (index, name) in names.iter().enumerate() {
            std::fs::create_dir(home.join(name)).unwrap();
            std::fs::write(home.join(name).join(format!("marker-{index}")), "").unwrap();
        }
        let admitted = authority(&home);
        let listing = list(&admitted, "mount:/home", "").unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.entries.iter().all(|entry| entry.name == "\u{fffd}"));
        for (index, name) in names.iter().enumerate() {
            let id = native_id(name);
            assert_eq!(listing.entries.iter().filter(|entry| entry.id == id).count(), 1);
            assert_eq!(native_name(&id).unwrap(), *name);
            assert_eq!(list(&admitted, "mount:/home", &id).unwrap().entries[0].name, format!("marker-{index}"));
        }
        std::fs::create_dir(home.join("empty")).unwrap();
        let empty = list(&admitted, "mount:/home", &native_id(OsStr::new("empty"))).unwrap();
        assert!(empty.entries.is_empty() && !empty.truncated);
        let large = home.join("large");
        std::fs::create_dir(&large).unwrap();
        for index in 0..MAX_LIST_ENTRIES { std::fs::write(large.join(format!("{index:04}")), "").unwrap(); }
        let path = native_id(OsStr::new("large"));
        let exact = list(&admitted, "mount:/home", &path).unwrap();
        assert_eq!(exact.entries.len(), MAX_LIST_ENTRIES);
        assert!(!exact.truncated);
        std::fs::write(large.join("extra"), "").unwrap();
        let over = list(&admitted, "mount:/home", &path).unwrap();
        assert_eq!(over.entries.len(), MAX_LIST_ENTRIES);
        assert!(over.truncated);
        let escaped = home.join("escaped");
        std::fs::create_dir(&escaped).unwrap();
        for index in 0..180 { std::fs::write(escaped.join(format!("{index:04}-{}", "\"".repeat(230))), "").unwrap(); }
        let budget = list(&admitted, "mount:/home", &native_id(OsStr::new("escaped"))).unwrap();
        assert!(budget.truncated && !budget.entries.is_empty() && budget.entries.len() < MAX_LIST_ENTRIES);
        let encoded = serde_json::to_vec(&serde_json::json!({
            "available": true, "entries": budget.entries, "truncated": budget.truncated,
        })).unwrap();
        assert!(encoded.len() <= MAX_LIST_BYTES);
        drop(admitted);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
