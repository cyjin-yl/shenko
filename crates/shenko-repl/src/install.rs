//! Real package installation into a candidate environment (R083).
//!
//! Two installer routes exist, both writing into a *candidate* directory the
//! session commits only after verification:
//!
//! * **Wheels** (Python kernels, WASI and native): a self-contained ZIP
//!   reader verifies CRC32, enforces entry/total size ceilings, rejects
//!   path-escaping entries, checks the wheel filename tag against the
//!   kernel's actual ABI route, and unpacks into the candidate site dir.
//!   Both pure-Python (`*-none-any`) and ABI-matched platform wheels are
//!   accepted where the route can actually run them (`wasm32-*wasi*` tags
//!   for the WASI route, `manylinux`/`musllinux`/`linux` x86_64 tags for
//!   native); incompatible wheels are reported as skipped with the reason.
//! * **npm / bun** (native Node/Bun kernels): the real installer binary
//!   (resolved next to the bound interpreter) runs with an exact environment
//!   in the candidate dir; a nonzero exit discards the candidate and the
//!   previous environment keeps serving the session.
//!
//! No network client is implemented here: wheel files must already exist on
//! disk (the caller resolves and pins them), and npm/bun reach their
//! registries only through the interpreter's own tooling under the host's
//! real network authority.

use crate::contract::{InstallOutcome, SkippedPackage};
use crate::kernel::{KernelProfile, PackageSpec};
use anyhow::{Context, Result, bail};
use flate2::read::DeflateDecoder;
use shenko_types::ids::RuntimeGeneration;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Per-entry unpacked-size ceiling (zip-bomb bound).
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// Total unpacked-size ceiling for one wheel.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Which wheel platform tags a kernel route can actually execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelRoute {
    /// CPython WASI: pure-Python wheels, or wheels built for a WASI target.
    PythonWasi,
    /// Native CPython on x86_64 Linux.
    PythonNative,
}

impl WheelRoute {
    fn accepts(&self, py_tag: &str, abi_tag: &str, platform_tag: &str) -> Result<(), String> {
        let py_ok = py_tag.split('.').any(|part| {
            part == "py3" || part.starts_with("py3") || part == "py2.py3"
        });
        if !py_ok {
            return Err(format!("python tag {py_tag:?} is not Python 3"));
        }
        if abi_tag == "none" && platform_tag == "any" {
            return Ok(());
        }
        match self {
            Self::PythonWasi => {
                if platform_tag.contains("emscripten") {
                    return Err("emscripten platform wheels need the Pyodide ABI route, not WASI".into());
                }
                if platform_tag.contains("wasi") && !abi_tag.contains("win") {
                    return Ok(());
                }
                Err(format!(
                    "platform {platform_tag:?} is not executable on the WASI route (accepted: any, *wasi*)"
                ))
            }
            Self::PythonNative => {
                let platform_ok = platform_tag.starts_with("manylinux")
                    || platform_tag.starts_with("musllinux")
                    || platform_tag == "linux_x86_64";
                let abi_ok = abi_tag == "abi3"
                    || (abi_tag.starts_with("cp3") && !abi_tag.contains("win"));
                if platform_ok && abi_ok {
                    return Ok(());
                }
                Err(format!(
                    "platform {platform_tag:?}/abi {abi_tag:?} is not executable on native x86_64 Linux"
                ))
            }
        }
    }
}

fn route_for(profile: &KernelProfile) -> Result<WheelRoute> {
    use crate::contract::{BackendKind, ReplRuntime};
    match (profile.backend, &profile.runtime) {
        (BackendKind::WasiPreview1, ReplRuntime::CPythonWasi) => Ok(WheelRoute::PythonWasi),
        (BackendKind::NativeHost, ReplRuntime::CPythonWasi) => Ok(WheelRoute::PythonNative),
        _ => bail!(
            "REPL_INSTALL_UNSUPPORTED: kernel {} has no wheel route (runtime {:?} backend {:?})",
            profile.kernel_id,
            profile.runtime,
            profile.backend
        ),
    }
}

/// Parsed wheel filename: `{dist}-{version}(-{build})?-{py}-{abi}-{platform}.whl`.
struct WheelName {
    name: String,
    version: String,
    py: String,
    abi: String,
    platform: String,
}

fn parse_wheel_name(path: &Path) -> Result<WheelName> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let stem = file_name.strip_suffix(".whl").with_context(|| {
        format!("REPL_INSTALL_INVALID: {file_name} is not a .whl file")
    })?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 || parts.len() > 6 {
        bail!("REPL_INSTALL_INVALID: wheel filename {file_name} must have 5 or 6 dash-separated fields");
    }
    let offset = parts.len() - 3;
    Ok(WheelName {
        name: parts[..offset].join("-").replace('-', "_"),
        version: parts[offset].to_string(),
        py: parts[offset + 1].to_string(),
        abi: parts[offset + 2].to_string(),
        platform: parts[offset + 3].to_string(),
    })
}

/// Install every requested wheel into `candidate`, returning the lock
/// entries for accepted wheels and the skip list for refused ones.
pub fn install_wheels(
    profile: &KernelProfile,
    wheels: &[PathBuf],
    candidate: &Path,
) -> Result<(Vec<PackageSpec>, Vec<SkippedPackage>)> {
    let route = route_for(profile)?;
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    for wheel in wheels {
        let parsed = match parse_wheel_name(wheel) {
            Ok(parsed) => parsed,
            Err(error) => {
                skipped.push(SkippedPackage {
                    name: wheel.display().to_string(),
                    pinned: String::new(),
                    reason: format!("{error:#}"),
                });
                continue;
            }
        };
        if let Err(reason) = route.accepts(&parsed.py, &parsed.abi, &parsed.platform) {
            skipped.push(SkippedPackage {
                name: parsed.name.clone(),
                pinned: parsed.version.clone(),
                reason,
            });
            continue;
        }
        unpack_wheel(wheel, candidate)
            .with_context(|| format!("unpack {}", wheel.display()))?;
        installed.push(PackageSpec {
            name: parsed.name,
            pinned: parsed.version,
        });
    }
    Ok((installed, skipped))
}

// ---------------------------------------------------------------------------
// ZIP reading (stored + deflated entries, CRC32 verified, path-confined).
// ---------------------------------------------------------------------------

const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;

struct CentralEntry {
    name: String,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16> {
    let slice = bytes.get(at..at + 2).context("zip: truncated u16")?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32> {
    let slice = bytes.get(at..at + 4).context("zip: truncated u32")?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn find_eocd(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 22 {
        bail!("REPL_INSTALL_INVALID: not a ZIP archive (too small)");
    }
    let scan_start = bytes.len().saturating_sub(22 + 0xFFFF);
    for at in (scan_start..=bytes.len() - 22).rev() {
        if read_u32(bytes, at)? == EOCD_SIG {
            return Ok(at);
        }
    }
    bail!("REPL_INSTALL_INVALID: ZIP end-of-central-directory not found")
}

fn parse_central_directory(bytes: &[u8]) -> Result<Vec<CentralEntry>> {
    let eocd = find_eocd(bytes)?;
    let entry_count = read_u16(bytes, eocd + 10)? as usize;
    let cd_offset = read_u32(bytes, eocd + 16)? as usize;
    let mut entries = Vec::with_capacity(entry_count);
    let mut at = cd_offset;
    for _ in 0..entry_count {
        if read_u32(bytes, at)? != CENTRAL_HEADER_SIG {
            bail!("REPL_INSTALL_INVALID: corrupt ZIP central directory");
        }
        let method = read_u16(bytes, at + 10)?;
        let crc32 = read_u32(bytes, at + 16)?;
        let compressed_size = read_u32(bytes, at + 20)? as u64;
        let uncompressed_size = read_u32(bytes, at + 24)? as u64;
        let name_len = read_u16(bytes, at + 28)? as usize;
        let extra_len = read_u16(bytes, at + 30)? as usize;
        let comment_len = read_u16(bytes, at + 32)? as usize;
        let local_header_offset = read_u32(bytes, at + 42)? as u64;
        let name_bytes = bytes
            .get(at + 46..at + 46 + name_len)
            .context("zip: truncated entry name")?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        entries.push(CentralEntry {
            name,
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// Confine an entry name to `dest`: no absolutes, no `..`, no drives.
fn confined_target(dest: &Path, name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        bail!("REPL_INSTALL_INVALID: ZIP entry {name:?} is absolute");
    }
    let mut target = dest.to_path_buf();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            bail!("REPL_INSTALL_INVALID: ZIP entry {name:?} escapes the target directory");
        }
        target.push(part);
    }
    Ok(target)
}

fn unpack_wheel(wheel: &Path, dest: &Path) -> Result<()> {
    let bytes = fs::read(wheel)
        .with_context(|| format!("read wheel {}", wheel.display()))?;
    let entries = parse_central_directory(&bytes)?;
    let mut total: u64 = 0;
    for entry in &entries {
        if entry.name.ends_with('/') {
            continue; // directory record; created on demand below
        }
        if entry.uncompressed_size > MAX_ENTRY_BYTES {
            bail!(
                "REPL_INSTALL_INVALID: ZIP entry {} exceeds the {} byte ceiling",
                entry.name,
                MAX_ENTRY_BYTES
            );
        }
        total = total
            .checked_add(entry.uncompressed_size)
            .context("zip total size overflow")?;
        if total > MAX_TOTAL_BYTES {
            bail!(
                "REPL_INSTALL_INVALID: wheel {} exceeds the {} byte unpacked ceiling",
                wheel.display(),
                MAX_TOTAL_BYTES
            );
        }
        if entry.method != METHOD_STORED && entry.method != METHOD_DEFLATED {
            bail!(
                "REPL_INSTALL_INVALID: ZIP entry {} uses unsupported method {}",
                entry.name,
                entry.method
            );
        }
        let target = confined_target(dest, &entry.name)?;
        let data_start = data_offset(&bytes, entry)?;
        let compressed = bytes
            .get(data_start..data_start + entry.compressed_size as usize)
            .context("zip: truncated entry data")?;
        let mut payload = Vec::with_capacity(entry.uncompressed_size as usize);
        match entry.method {
            METHOD_STORED => payload.extend_from_slice(compressed),
            METHOD_DEFLATED => {
                let mut decoder = DeflateDecoder::new(compressed);
                decoder
                    .read_to_end(&mut payload)
                    .context("zip: deflate decode failed")?;
            }
            _ => unreachable!("method validated above"),
        }
        if payload.len() as u64 != entry.uncompressed_size {
            bail!("REPL_INSTALL_INVALID: ZIP entry {} size mismatch", entry.name);
        }
        let actual_crc = crc32fast::hash(&payload);
        if actual_crc != entry.crc32 {
            bail!(
                "REPL_INSTALL_INVALID: ZIP entry {} CRC mismatch (corrupt wheel)",
                entry.name
            );
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&target)?;
        file.write_all(&payload)?;
        file.sync_all()?;
    }
    sync_parents(dest)?;
    Ok(())
}

fn data_offset(bytes: &[u8], entry: &CentralEntry) -> Result<usize> {
    let at = entry.local_header_offset as usize;
    if read_u32(bytes, at)? != LOCAL_HEADER_SIG {
        bail!("REPL_INSTALL_INVALID: corrupt ZIP local header");
    }
    let name_len = read_u16(bytes, at + 26)? as usize;
    let extra_len = read_u16(bytes, at + 28)? as usize;
    Ok(at + 30 + name_len + extra_len)
}

fn sync_parents(dir: &Path) -> Result<()> {
    let mut current = Some(dir);
    while let Some(path) = current {
        File::open(path)?.sync_all()?;
        current = path.parent();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// npm / bun registry installers (native JS kernels only).
// ---------------------------------------------------------------------------

/// Install pinned registry packages into `candidate` with the real installer
/// resolved next to the bound interpreter. Returns lock entries on success.
pub fn install_registry_packages(
    profile: &KernelProfile,
    packages: &[PackageSpec],
    candidate: &Path,
    log_dir: &Path,
) -> Result<Vec<PackageSpec>> {
    use crate::contract::BackendKind;
    let ModuleSource::ModulePath(executable) = &profile.module else {
        bail!("REPL_INSTALL_UNSUPPORTED: native kernel has no executable path");
    };
    let exe_dir = executable
        .parent()
        .context("REPL_INSTALL_UNSUPPORTED: interpreter path has no parent")?;
    let (tool, args_prefix): (PathBuf, Vec<String>) = match profile.backend {
        BackendKind::NativeHost if profile.runtime == crate::contract::ReplRuntime::NodeWasm => {
            let npm = exe_dir.join("npm");
            if !npm.is_file() {
                bail!(
                    "REPL_INSTALL_UNSUPPORTED: no npm next to the bound node executable ({})",
                    exe_dir.display()
                );
            }
            (npm, vec!["install".into(), "--no-audit".into(), "--no-fund".into(), "--loglevel=error".into()])
        }
        BackendKind::NativeHost if profile.runtime == crate::contract::ReplRuntime::BunWasm => {
            (executable.clone(), vec!["add".into()])
        }
        _ => bail!(
            "REPL_INSTALL_UNSUPPORTED: kernel {} has no registry installer (runtime {:?} backend {:?})",
            profile.kernel_id,
            profile.runtime,
            profile.backend
        ),
    };

    // Minimal package.json so both tools treat the dir as a project root.
    fs::create_dir_all(candidate)?;
    let manifest = candidate.join("package.json");
    if !manifest.exists() {
        fs::write(
            &manifest,
            br#"{"name":"shenko-repl-env","private":true,"version":"0.0.0"}"#,
        )?;
    }

    let mut args = args_prefix;
    for package in packages {
        if package.name.is_empty()
            || package.name.starts_with('-')
            || package.name.contains(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&')
        {
            bail!("REPL_INSTALL_INVALID: package name {:?} is not a plain npm/bun package name", package.name);
        }
        args.push(format!("{}@{}", package.name, package.pinned));
    }

    let stdout_path = log_dir.join("installer.stdout.log");
    let stderr_path = log_dir.join("installer.stderr.log");
    let mut stdout = File::create(&stdout_path)?;
    let mut stderr = File::create(&stderr_path)?;
    let mut envs: Vec<(String, String)> = profile.env.clone();
    let path_entry = format!("{}:{}", exe_dir.display(), std::env::var("PATH").unwrap_or_default());
    envs.retain(|(key, _)| key != "PATH");
    envs.push(("PATH".into(), path_entry));

    let status = std::process::Command::new(&tool)
        .args(&args)
        .env_clear()
        .envs(envs)
        .current_dir(candidate)
        .stdout(std::process::Stdio::from(stdout.try_clone()?))
        .stderr(std::process::Stdio::from(stderr.try_clone()?))
        .status()
        .with_context(|| format!("spawn installer {}", tool.display()))?;
    stdout.sync_all()?;
    stderr.sync_all()?;
    if !status.success() {
        let tail = fs::read_to_string(&stderr_path)
            .unwrap_or_default()
            .chars()
            .rev()
            .take(512)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        bail!(
            "REPL_INSTALL_FAILED: {} exited with {status}; the candidate environment was discarded; stderr tail: {tail}",
            tool.display()
        );
    }
    Ok(packages.to_vec())
}

use crate::kernel::ModuleSource;

/// Recompute the environment generation a commit would produce.
pub fn next_generation(
    profile: &KernelProfile,
    added: &[PackageSpec],
) -> RuntimeGeneration {
    let mut kernel = profile.clone();
    kernel.dependency_lock.extend(added.iter().cloned());
    crate::kernel::environment_generation(&kernel)
}

/// Classification of one install attempt for the caller's outcome report.
pub struct InstallAttempt {
    pub installed: Vec<PackageSpec>,
    pub skipped: Vec<SkippedPackage>,
}

/// Run every requested install route into `candidate`. Partial success is
/// reported; nothing is committed here — the session commits the candidate
/// only when at least one package installed and no fatal fault occurred.
pub fn install_into_candidate(
    profile: &KernelProfile,
    request: &crate::contract::InstallRequest,
    candidate: &Path,
    log_dir: &Path,
) -> Result<InstallAttempt> {
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    if !request.wheels.is_empty() {
        let (wheel_installed, wheel_skipped) =
            install_wheels(profile, &request.wheels, candidate)?;
        installed.extend(wheel_installed);
        skipped.extend(wheel_skipped);
    }
    if !request.packages.is_empty() {
        match install_registry_packages(profile, &request.packages, candidate, log_dir) {
            Ok(packages) => installed.extend(packages),
            Err(error) => {
                if installed.is_empty() {
                    return Ok(InstallAttempt { installed, skipped: vec![SkippedPackage {
                        name: "registry".into(),
                        pinned: String::new(),
                        reason: format!("{error:#}"),
                    }] });
                }
                // Wheels already landed; keep them and report the registry
                // failure as skipped so the caller can decide.
                skipped.push(SkippedPackage {
                    name: "registry".into(),
                    pinned: String::new(),
                    reason: format!("{error:#}"),
                });
            }
        }
    }
    Ok(InstallAttempt { installed, skipped })
}

/// Honest wrapper used by the session when a route does not exist at all.
pub fn unsupported(profile: &KernelProfile) -> InstallOutcome {
    InstallOutcome::Unsupported {
        reason: format!(
            "kernel {} (runtime {:?} backend {:?}) has no package installer route",
            profile.kernel_id, profile.runtime, profile.backend
        ),
    }
}
