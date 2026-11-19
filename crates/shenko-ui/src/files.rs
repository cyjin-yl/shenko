//! The Finder-like file view: every file system the subject can reach.
//!
//! Roots are facts, never guesses. The subject's WASM home and its other
//! declared mounts come from the installed home manifest. A mount's network
//! protocol (sshfs, NFS, SMB, a hypervisor shared folder) comes from the
//! host's own mount table for the mount's source. Other nodes come from this
//! node's sync-peer records, wired in by the runtime.
//!
//! Browsing walks from a directory capability admitted the way the guest's
//! preopens are (`OwnedDir`: per-component, no symlink following), so the
//! view can never reach outside a declared root through a link or a `..`.
//! The owner's view is read-only; writing is the subject's effect.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
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
    pub name: String,
    /// `dir`, `file`, `symlink` or `other`.
    pub kind: &'static str,
    pub bytes: u64,
    pub modified: Option<String>,
}

#[derive(Deserialize)]
struct ManifestMounts {
    mounts: Vec<MountSpec>,
}

fn declared_mounts(data_dir: &Path) -> Vec<MountSpec> {
    std::fs::read(data_dir.join("home-manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ManifestMounts>(&bytes).ok())
        .map(|manifest| manifest.mounts)
        .unwrap_or_default()
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
pub fn roots(data_dir: &Path, peers: &[FilePeer]) -> Vec<FileRoot> {
    let mut roots = Vec::new();
    let mut mounts = declared_mounts(data_dir);
    mounts.sort_by_key(|mount| (mount.guest_path != "/home", mount.guest_path.clone()));
    for mount in &mounts {
        let home = mount.guest_path == "/home";
        let protocol = if home {
            "wasm_home".to_owned()
        } else {
            host_fs_type(&mount.source).map_or_else(|| "local".to_owned(), |fs| protocol_for(&fs).to_owned())
        };
        roots.push(FileRoot {
            id: format!("mount:{}", mount.guest_path),
            label: mount.guest_path.trim_start_matches('/').to_owned(),
            kind: if home { RootKind::Home } else { RootKind::Mount },
            protocol,
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

/// A path inside a root: relative, normal components only.
fn relative(path: &str) -> Result<PathBuf, &'static str> {
    let mut out = PathBuf::new();
    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            _ => return Err("路径只能是根之下的普通名称"),
        }
    }
    Ok(out)
}

/// The entries of one directory inside a local root, directories first.
pub fn list(data_dir: &Path, root_id: &str, path: &str) -> Result<Vec<FileEntry>, String> {
    let guest = root_id.strip_prefix("mount:").ok_or("只能浏览本节点的挂载")?;
    let mount = declared_mounts(data_dir)
        .into_iter()
        .find(|mount| mount.guest_path == guest)
        .ok_or("没有这个挂载")?;
    let root = OwnedDir::admit_absolute(&mount.source).map_err(|error| error.to_string())?;
    let rel = relative(path)?;
    let dir = if rel.as_os_str().is_empty() { root } else { OwnedDir::admit_relative(&root, &rel).map_err(|error| error.to_string())? };
    let mut entries = Vec::new();
    for entry in dir.dir().entries().map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().map_err(|error| error.to_string())?;
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() { "symlink" } else if file_type.is_dir() { "dir" } else if file_type.is_file() { "file" } else { "other" };
        let modified = meta.modified().ok().map(|time| chrono::DateTime::<chrono::Utc>::from(time.into_std()).to_rfc3339());
        entries.push(FileEntry { name, kind, bytes: if kind == "file" { meta.len() } else { 0 }, modified });
    }
    entries.sort_by(|a, b| (a.kind != "dir").cmp(&(b.kind != "dir")).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsing_stays_inside_the_root_and_names_protocols_from_the_mount_table() {
        let dir = std::env::temp_dir().join(format!("shenko-files-{}", std::process::id()));
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::write(home.join("notes/a.md"), "# a").unwrap();
        std::fs::write(dir.join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.txt"), home.join("escape")).unwrap();
        std::os::unix::fs::symlink(&dir, home.join("up")).unwrap();
        std::fs::write(dir.join("home-manifest.json"), serde_json::json!({
            "mounts": [{"source": home, "guest_path": "/home", "mode": "read_write", "persistent": true, "versioned": false}]
        }).to_string()).unwrap();

        let root = &roots(&dir, &[])[0];
        assert_eq!((root.kind.clone(), root.protocol.as_str()), (RootKind::Home, "wasm_home"));
        let top = list(&dir, "mount:/home", "").unwrap();
        let names: Vec<_> = top.iter().map(|entry| (entry.name.as_str(), entry.kind)).collect();
        assert_eq!(names, [("notes", "dir"), ("escape", "symlink"), ("up", "symlink")]);
        assert_eq!(list(&dir, "mount:/home", "notes").unwrap()[0].name, "a.md");
        assert!(list(&dir, "mount:/home", "../").is_err(), "a parent component is refused");
        assert!(list(&dir, "mount:/home", "up").is_err(), "a symlinked directory is not followed");
        assert_eq!((protocol_for("fuse.sshfs"), protocol_for("nfs4"), protocol_for("cifs"), protocol_for("virtiofs")),
            ("sshfs", "nfs", "smb", "shared_folder"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
