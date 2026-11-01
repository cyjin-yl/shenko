//! Engine discovery and process launch for the native browser provider.
//!
//! SPEC §24.6: the browser is a real, installed engine. This module never
//! downloads an engine and never fabricates one: it looks for an already
//! installed browser at a pinned set of locations, probes the real binary,
//! and reports a typed-unavailable result naming exactly what is missing.
//!
//! The default route is virtual-headed (a real headful browser on a virtual
//! X display, so a human can attach to the same live session). True headless
//! stays available as an explicit option. A node with no display capability
//! is reported as such — not silently converted into "headed on nothing".
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use super::protocol::{BrowserProtocol, RestoreCoverage, http_get_json};
use super::protocol_bidi::BidiClient;
use super::protocol_cdp::CdpClient;

/// How long the engine may take to expose its control endpoint.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an engine version probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineFamily {
    /// Chromium family: CDP.
    Chromium,
    /// Firefox family: WebDriver BiDi.
    Firefox,
}

impl EngineFamily {
    pub fn protocol(&self) -> &'static str {
        match self {
            EngineFamily::Chromium => "cdp",
            EngineFamily::Firefox => "bidi",
        }
    }
}

/// A real installed browser binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineSpec {
    /// The absolute path of the binary that was actually executed.
    pub path: PathBuf,
    /// The binary's own file name, used as the engine name.
    pub name: String,
    pub family: EngineFamily,
    /// The version string the binary itself printed.
    pub version: String,
}

impl EngineSpec {
    pub fn describe(&self) -> String {
        format!("{} {} ({})", self.name, self.version, self.path.display())
    }
}

/// The session's display mode. `virtual_headed` is a real headful browser on
/// a virtual X display; it is NOT true headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    VirtualHeaded,
    Headless,
}

impl SessionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionMode::VirtualHeaded => "virtual_headed",
            SessionMode::Headless => "headless",
        }
    }
}

/// The pinned candidate locations. No engine is ever downloaded: only a
/// binary that already exists on this node can be used.
fn chromium_candidates() -> Vec<PathBuf> {
    let names = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "microsoft-edge",
        "brave-browser",
    ];
    let mut paths = Vec::new();
    for name in names {
        paths.push(PathBuf::from("/usr/bin").join(name));
        paths.push(PathBuf::from("/usr/local/bin").join(name));
        paths.push(PathBuf::from("/snap/bin").join(name));
        paths.push(PathBuf::from("/opt/google/chrome").join(name));
        paths.push(PathBuf::from("/opt/chromium").join(name));
        if let Ok(path) = which(name) {
            paths.push(path);
        }
    }
    paths
}

fn firefox_candidates() -> Vec<PathBuf> {
    let names = ["firefox", "firefox-esr", "camoufox", "camoufox-bin"];
    let mut paths = Vec::new();
    for name in names {
        paths.push(PathBuf::from("/usr/bin").join(name));
        paths.push(PathBuf::from("/usr/local/bin").join(name));
        paths.push(PathBuf::from("/snap/bin").join(name));
        paths.push(PathBuf::from("/opt/camoufox").join(name));
        if let Ok(path) = which(name) {
            paths.push(path);
        }
    }
    paths.push(PathBuf::from("/usr/lib/firefox/firefox"));
    paths
}

/// Resolve a program name through `PATH`.
fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!("{name} is not on PATH"))
}

/// Probe a binary's own version string. A binary that cannot report a
/// version is still usable, but the probe failure is kept so the session
/// receipt can say what was actually observed.
async fn probe_version(path: &Path) -> String {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        Command::new(path).arg("--version").stdin(Stdio::null()).output(),
    )
    .await;
    match output {
        Ok(Ok(output)) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let text = if text.trim().is_empty() {
                String::from_utf8_lossy(&output.stderr).to_string()
            } else {
                text.to_string()
            };
            text.lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        }
        _ => "unknown".into(),
    }
}

/// Discover every real installed engine on this node.
///
/// `preferred` may be an engine family (`chromium` / `firefox`), a binary
/// name, or an absolute path. When an explicit path is given, only that
/// binary is considered, and a missing binary is an honest error.
pub async fn discover(preferred: Option<&str>) -> Result<Vec<EngineSpec>> {
    let mut engines = Vec::new();
    if let Some(preferred) = preferred.map(str::trim).filter(|value| !value.is_empty()) {
        let path = if preferred.contains('/') {
            PathBuf::from(preferred)
        } else {
            which(preferred).unwrap_or_else(|_| PathBuf::from(preferred))
        };
        ensure!(
            path.is_file(),
            "requested browser engine {preferred} is not an installed file"
        );
        let family = family_for(&path, preferred);
        let version = probe_version(&path).await;
        engines.push(EngineSpec {
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| preferred.to_string()),
            path,
            family,
            version,
        });
        return Ok(engines);
    }
    for (family, candidates) in [
        (EngineFamily::Chromium, chromium_candidates()),
        (EngineFamily::Firefox, firefox_candidates()),
    ] {
        let mut seen = Vec::new();
        for candidate in candidates {
            let canonical = candidate.canonicalize().unwrap_or(candidate.clone());
            if seen.contains(&canonical) {
                continue;
            }
            seen.push(canonical.clone());
            if !canonical.is_file() {
                continue;
            }
            let version = probe_version(&canonical).await;
            if version == "unknown" {
                // A binary that cannot even report its version is not a
                // usable engine for this adapter.
                continue;
            }
            engines.push(EngineSpec {
                name: canonical
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default(),
                path: canonical,
                family,
                version,
            });
        }
    }
    Ok(engines)
}

/// Guess the protocol family of an explicitly requested binary.
fn family_for(path: &Path, hint: &str) -> EngineFamily {
    let hint = hint.to_ascii_lowercase();
    if hint.contains("firefox") || hint.contains("camoufox") {
        return EngineFamily::Firefox;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("firefox") || name.contains("camoufox") {
        EngineFamily::Firefox
    } else {
        EngineFamily::Chromium
    }
}

// ---------------------------------------------------------------------------
// Virtual display
// ---------------------------------------------------------------------------

/// A virtual X display for a virtual-headed session.
pub struct VirtualDisplay {
    /// The display address handed to the browser (`DISPLAY=:99`).
    pub display: String,
    /// The shared display-domain id, in the same `x11/<endpoint>/root<hex>`
    /// shape the desktop provider uses, so both providers agree on which
    /// physical display a session owns.
    pub domain_id: Option<String>,
    child: Option<Child>,
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// Acquire a display for a virtual-headed session.
///
/// An explicit `SHENKO_BROWSER_DISPLAY` is used as-is. Otherwise a private
/// Xvfb display is started. When neither is possible the caller is told
/// exactly which capability is missing instead of pretending to be headed.
pub async fn acquire_display() -> Result<VirtualDisplay> {
    if let Ok(explicit) = std::env::var("SHENKO_BROWSER_DISPLAY") {
        let explicit = explicit.trim().to_string();
        if !explicit.is_empty() {
            let domain_id = describe_display(&explicit).await.ok();
            return Ok(VirtualDisplay {
                display: explicit,
                domain_id,
                child: None,
            });
        }
    }
    let xvfb = which("Xvfb").map_err(|_| {
        anyhow!(
            "no display capability: Xvfb is not installed and SHENKO_BROWSER_DISPLAY is unset, \
             so a virtual-headed browser cannot be started (install Xvfb or use headless mode)"
        )
    })?;
    for number in 90..=140u16 {
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        if socket.exists() || lock.exists() {
            continue;
        }
        let display = format!(":{number}");
        let mut child = Command::new(&xvfb)
            .arg(&display)
            .args(["-screen", "0", "1280x800x24", "-nolisten", "tcp", "-noreset"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start Xvfb virtual display")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if socket.exists() {
                let domain_id = describe_display(&display).await.ok();
                return Ok(VirtualDisplay {
                    display,
                    domain_id,
                    child: Some(child),
                });
            }
            if let Ok(Some(status)) = child.try_wait() {
                bail!("Xvfb exited before the display {display} became usable: {status}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = child.start_kill();
    }
    bail!("no free virtual display number was available for a virtual-headed session")
}

/// Read the real display endpoint and root window so the shared display
/// domain id matches the desktop provider's spelling.
async fn describe_display(display: &str) -> Result<String> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_index) = RustConnection::connect(Some(display))?;
    let root = conn.setup().roots[screen_index].root;
    let endpoint = display_endpoint_key(&conn).unwrap_or_else(|| display.to_string());
    Ok(format!("x11/{endpoint}/root{root:08x}"))
}

#[cfg(unix)]
fn display_endpoint_key(conn: &x11rb::rust_connection::RustConnection) -> std::io::Result<String> {
    use std::fmt::Write as _;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    let unix = UnixStream::from(conn.stream().as_fd().try_clone_to_owned()?);
    let peer = unix.peer_addr()?;
    if let Some(path) = peer.as_pathname() {
        let mut key = String::from("unix:path:");
        for byte in path.as_os_str().as_bytes() {
            let _ = write!(key, "{byte:02x}");
        }
        return Ok(key);
    }
    Ok(format!("unix:{:?}", peer))
}

#[cfg(not(unix))]
fn display_endpoint_key(_conn: &x11rb::rust_connection::RustConnection) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "display endpoint inspection unsupported on this platform",
    ))
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// A launched (or reattached) browser instance bound to its own protocol
/// client. `process` is `None` when the session reattached to an engine
/// process a previous provider run left running in this profile; a reattached
/// process is never killed on close.
pub struct Launched {
    pub protocol: Arc<dyn BrowserProtocol>,
    pub endpoint: String,
    pub product: String,
    pub process: Option<Child>,
    pub restore: RestoreCoverage,
}

/// Launch the engine against a real persistent profile directory.
pub async fn launch(
    engine: &EngineSpec,
    profile_dir: &Path,
    mode: SessionMode,
    display: Option<&VirtualDisplay>,
) -> Result<Launched> {
    match engine.family {
        EngineFamily::Chromium => launch_chromium(engine, profile_dir, mode, display).await,
        EngineFamily::Firefox => launch_firefox(engine, profile_dir, mode, display).await,
    }
}

async fn launch_chromium(
    engine: &EngineSpec,
    profile_dir: &Path,
    mode: SessionMode,
    display: Option<&VirtualDisplay>,
) -> Result<Launched> {
    // A still-running instance from the same profile (e.g. the provider
    // restarted but the browser outlived it) is reused instead of relaunched,
    // so provider crash recovery never re-executes anything the browser
    // already did.
    if let Some(reattached) = reattach_chromium(profile_dir).await? {
        return Ok(reattached);
    }
    let mut command = Command::new(&engine.path);
    command
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        // Port 0 makes the browser pick a free port and publish it in
        // DevToolsActivePort; the port is then read from the file, never
        // guessed.
        .arg("--remote-debugging-port=0")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if mode == SessionMode::Headless {
        command.arg("--headless=new");
    } else if let Some(display) = display {
        command.env("DISPLAY", &display.display);
    }
    let mut child = command.spawn().context("start chromium browser process")?;
    let port_file = profile_dir.join("DevToolsActivePort");
    let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT;
    let mut port: Option<u16> = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = tokio::fs::read_to_string(&port_file).await {
            if let Some(first) = text.lines().next() {
                if let Ok(parsed) = first.trim().parse::<u16>() {
                    port = Some(parsed);
                    break;
                }
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!("chromium exited before publishing its DevTools port: {status}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Some(port) = port else {
        let _ = child.start_kill();
        bail!("chromium did not publish DevToolsActivePort within {}s", LAUNCH_TIMEOUT.as_secs());
    };
    let (endpoint, product) = match CdpClient::discover_endpoint(port).await {
        Ok(discovered) => discovered,
        Err(error) => {
            let _ = child.start_kill();
            return Err(error.context("read the chromium devtools endpoint"));
        }
    };
    let protocol = match CdpClient::connect(endpoint.clone(), product.clone()).await {
        Ok(client) => client,
        Err(error) => {
            let _ = child.start_kill();
            return Err(error.context("attach to the chromium devtools endpoint"));
        }
    };
    Ok(Launched {
        protocol: Arc::new(protocol),
        endpoint,
        product,
        process: Some(child),
        restore: RestoreCoverage::default(),
    })
}

/// Reattach to a chromium instance still serving the profile's
/// DevToolsActivePort. Returns `None` when the file is absent or nothing
/// answers on the port, so the caller falls through to a real launch.
async fn reattach_chromium(profile_dir: &Path) -> Result<Option<Launched>> {
    let port = match std::fs::read_to_string(profile_dir.join("DevToolsActivePort")) {
        Ok(content) => match content.lines().next().and_then(|line| line.parse::<u16>().ok()) {
            Some(port) => port,
            None => return Ok(None),
        },
        Err(_) => return Ok(None),
    };
    let version = match http_get_json("127.0.0.1", port, "/json/version").await {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let browser_ws = match version.get("webSocketDebuggerUrl").and_then(|url| url.as_str()) {
        Some(url) => url.to_string(),
        None => return Ok(None),
    };
    let protocol = match CdpClient::connect(&browser_ws, "chromium (reattached)").await {
        Ok(protocol) => protocol,
        Err(_) => return Ok(None),
    };
    let targets = protocol.targets().await.unwrap_or_default();
    if !targets.iter().any(|target| target.kind == "page") {
        let _ = protocol.create_tab("about:blank").await;
    }
    tracing::info!(%port, "reattached to an existing chromium instance for this profile");
    Ok(Some(Launched {
        protocol: Arc::new(protocol),
        endpoint: format!("127.0.0.1:{port}"),
        product: "chromium (reattached)".into(),
        process: None,
        restore: RestoreCoverage::default(),
    }))
}

async fn launch_firefox(
    engine: &EngineSpec,
    profile_dir: &Path,
    mode: SessionMode,
    display: Option<&VirtualDisplay>,
) -> Result<Launched> {
    // Firefox reads its profile preferences from user.js on every start.
    std::fs::create_dir_all(profile_dir).context("create firefox profile directory")?;
    let prefs = "user_pref(\"remote.active-protocols\", 1);\n\
                user_pref(\"browser.shell.checkDefaultBrowser\", false);\n\
                user_pref(\"browser.startup.homepage\", \"about:blank\");\n\
                user_pref(\"datareporting.policy.dataSubmissionEnabled\", false);\n\
                user_pref(\"toolkit.telemetry.enabled\", false);\n\
                user_pref(\"browser.tabs.warnOnClose\", false);\n\
                user_pref(\"app.update.auto\", false);\n";
    std::fs::write(profile_dir.join("user.js"), prefs).context("write firefox profile prefs")?;
    let port = reserve_loopback_port().await?;
    let mut command = Command::new(&engine.path);
    command
        .arg("--profile")
        .arg(profile_dir)
        .arg("--remote-debugging-port")
        .arg(port.to_string())
        .arg("--no-remote")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if mode == SessionMode::Headless {
        command.arg("--headless");
    } else if let Some(display) = display {
        command.env("DISPLAY", &display.display);
    }
    let mut child = command.spawn().context("start firefox browser process")?;
    let stderr = child.stderr.take();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let stderr_task = tokio::spawn(async move {
        let Some(stderr) = stderr else {
            return;
        };
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = extract_bidi_url(&line) {
                let _ = tx.send(url);
            }
        }
    });
    let endpoint = match tokio::time::timeout(LAUNCH_TIMEOUT, rx).await {
        Ok(Ok(url)) => url,
        Ok(Err(_)) => {
            let _ = child.start_kill();
            stderr_task.abort();
            bail!("firefox closed before publishing a WebDriver BiDi endpoint");
        }
        Err(_) => {
            let _ = child.start_kill();
            stderr_task.abort();
            bail!(
                "firefox did not publish a WebDriver BiDi endpoint within {}s",
                LAUNCH_TIMEOUT.as_secs()
            );
        }
    };
    let product = format!("firefox-bidi {}", engine.version);
    let protocol = match BidiClient::connect(endpoint.clone(), product.clone()).await {
        Ok(client) => client,
        Err(error) => {
            let _ = child.start_kill();
            stderr_task.abort();
            return Err(error.context("attach to the firefox BiDi endpoint"));
        }
    };
    stderr_task.abort();
    Ok(Launched {
        protocol: Arc::new(protocol),
        endpoint,
        product,
        process: Some(child),
        restore: RestoreCoverage::default(),
    })
}

/// Extract the BiDi endpoint from a Firefox startup line.
fn extract_bidi_url(line: &str) -> Option<String> {
    let index = line.find("ws://")?;
    let url = line[index..].trim();
    let end = url.find(char::is_whitespace).unwrap_or(url.len());
    let url = &url[..end];
    if url.ends_with("/session") {
        Some(url.to_string())
    } else {
        None
    }
}

/// Reserve a loopback port for Firefox's own debugging server.
async fn reserve_loopback_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("reserve a loopback port for the firefox debugging server")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
