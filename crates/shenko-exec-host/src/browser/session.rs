//! Live browser sessions: open, close, observe.
//!
//! A session is one real browser process bound to one real protocol client
//! and one persistent profile. Sessions are independent: a failure in one
//! profile never blocks another profile's work, and there is no global
//! browser lock — only the per-profile single-writer rule.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use shenko_types::ids::EntityId;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::desktop::HumanInputObserver;
use super::engine::{self, EngineFamily, EngineSpec, SessionMode};
use super::protocol::{BrowserProtocol, PageTarget, RestoreCoverage, TabHandle, Viewport};
use super::{
    BrowserProvider, BrowserSession, ControllerRole, ObserveOptions,
    Observation, SessionFacts, SessionRequest, TabFacts, WriterFacts, digest_hex, now_rfc3339,
    redact_endpoint, writer_facts, MAX_NAME, MAX_URL, OBSERVATION_VALIDITY_MS,
};

impl HumanInputObserver for BrowserProvider {
    /// A human took over a live display domain. Every browser session bound
    /// to that domain loses its agent lease and bumps the input epoch, so the
    /// next agent act must be taken over explicitly instead of silently
    /// continuing the human's session.
    fn on_human_input(&self, domain_id: &str) {
        let sessions: Vec<Arc<BrowserSession>> = self
            .state()
            .sessions
            .values()
            .filter(|session| session.domain_id.as_deref() == Some(domain_id))
            .cloned()
            .collect();
        for session in sessions {
            session.epoch.fetch_add(1, Ordering::Relaxed);
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                let mut lease = session.lease.lock().await;
                if lease.as_ref().is_some_and(|lease| lease.role == ControllerRole::Agent) {
                    *lease = None;
                }
            });
        }
    }
}

/// The default persistent identity name.
pub const DEFAULT_PROFILE: &str = "default";

impl BrowserProvider {
    /// Open a real browser session against a persistent identity.
    pub async fn open_session(
        &self,
        request: SessionRequest,
        caller: EntityId,
        node: &str,
    ) -> Result<SessionFacts> {
        let profile_id = request
            .profile_id
            .clone()
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
        Self::validate_profile_id(&profile_id)?;

        let engines = engine::discover(request.engine.as_deref())
            .await
            .context("discover an installed browser engine")?;
        ensure!(
            !engines.is_empty(),
            "no installed browser engine was found on this node (looked for chromium/chrome and \
             firefox/camoufox in the pinned system and PATH locations); no engine is ever \
             downloaded automatically"
        );
        let engine = self.select_engine(&engines, &profile_id, request.engine.as_deref())?;

        // The persistent identity is created before the browser starts, so the
        // profile directory is real and the writer lock has a subject.
        let family = engine.family;
        self.create_identity(&profile_id, family)?;
        let directory = self.profile_dir(&profile_id);

        let requested_mode = request.mode.clone().unwrap_or_else(|| "auto".into());
        ensure!(requested_mode.len() <= MAX_NAME, "mode name is too long");
        let (mode, display_handle, degradation) = match requested_mode.as_str() {
            "headless" => (SessionMode::Headless, None, None),
            "virtual_headed" => {
                let display = engine::acquire_display().await.map_err(|error| {
                    anyhow!("virtual-headed mode was requested but unavailable: {error}")
                })?;
                (SessionMode::VirtualHeaded, Some(display), None)
            }
            "auto" => match engine::acquire_display().await {
                Ok(display) => (SessionMode::VirtualHeaded, Some(display), None),
                Err(error) => (
                    SessionMode::Headless,
                    None,
                    Some(format!(
                        "virtual-headed was the default but this node has no display capability, \
                         so the session runs truly headless: {error}"
                    )),
                ),
            },
            other => bail!("unknown browser mode {other}; use auto, virtual_headed or headless"),
        };

        let session_id = EntityId::new();
        let writer = self.acquire_writer(&profile_id, caller, node, session_id)?;

        let launched = engine::launch(&engine, &directory, mode, display_handle.as_ref()).await;
        let launched = match launched {
            Ok(launched) => launched,
            Err(error) => {
                self.release_writer(&profile_id, caller);
                return Err(error);
            }
        };

        let group = request
            .group
            .clone()
            .unwrap_or_else(|| "default".into());
        ensure!(group.len() <= MAX_NAME, "group name is too long");
        let url = request.url.clone().unwrap_or_else(|| "about:blank".into());
        ensure!(url.len() <= MAX_URL, "url is too long");

        let protocol = Arc::clone(&launched.protocol);
        let tab = match protocol.create_tab(&url).await {
            Ok(tab) => tab,
            Err(error) => {
                if let Some(mut child) = launched.process {
                    let _ = child.start_kill();
                }
                self.release_writer(&profile_id, caller);
                return Err(error.context("open the first browser tab"));
            }
        };
        let mut tabs = BTreeMap::new();
        tabs.insert(
            tab.id.clone(),
            super::TabEntry {
                handle: tab.clone(),
                group: group.clone(),
                url: url.clone(),
                title: String::new(),
                revision: 1,
            },
        );

        let session = Arc::new(BrowserSession {
            session_id,
            profile_id: profile_id.clone(),
            run_id: digest_hex(session_id.to_string().as_bytes()),
            engine: engine.clone(),
            mode_requested: requested_mode.clone(),
            mode_effective: mode,
            mode_degradation: degradation.clone(),
            display: display_handle.as_ref().map(|display| display.display.clone()),
            domain_id: display_handle.as_ref().and_then(|display| display.domain_id.clone()),
            endpoint: redact_endpoint(&launched.endpoint),
            product: launched.product.clone(),
            protocol,
            process: super::AsyncMutex::new(launched.process),
            display_handle: super::AsyncMutex::new(display_handle),
            epoch: std::sync::atomic::AtomicU64::new(1),
            lease: super::AsyncMutex::new(None),
            tabs: super::AsyncMutex::new(tabs),
            observations: super::AsyncMutex::new(BTreeMap::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        {
            let mut state = self.state();
            state.sessions.insert(session_id, Arc::clone(&session));
            if let Some(identity) = state.identities.get_mut(&profile_id) {
                identity.writer = Some(super::WriterRecord {
                    instance: caller,
                    node: node.to_string(),
                    pid: std::process::id(),
                    session_id,
                    acquired_at: writer.acquired_at.clone(),
                });
            }
        }
        let facts = self.session_facts(&session).await?;
        Ok(facts)
    }

    fn select_engine(
        &self,
        engines: &[EngineSpec],
        profile_id: &str,
        requested: Option<&str>,
    ) -> Result<EngineSpec> {
        if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
            let wanted = requested.to_ascii_lowercase();
            if let Some(found) = engines.iter().find(|engine| {
                engine.name.to_ascii_lowercase() == wanted
                    || engine.path.to_string_lossy().to_ascii_lowercase() == wanted
            }) {
                return Ok(found.clone());
            }
            if let Some(found) = engines.iter().find(|engine| {
                wanted.contains(engine.family.protocol())
                    || wanted.contains(&engine.name.to_ascii_lowercase())
            }) {
                return Ok(found.clone());
            }
            bail!("requested browser engine {requested} was not found among the installed engines");
        }
        // An identity keeps the engine family it was created with, so a
        // profile is not silently moved between incompatible engines.
        let state = self.state();
        if let Some(identity) = state.identities.get(profile_id) {
            if let Some(found) = engines
                .iter()
                .find(|engine| engine.family == identity.family)
            {
                return Ok(found.clone());
            }
        }
        Ok(engines[0].clone())
    }

    /// Close a live session: the protocol client is shut down, the browser
    /// process is killed, the virtual display is released and the profile's
    /// single write ownership is given back.
    pub async fn close_session(
        &self,
        session_id: EntityId,
        caller: EntityId,
    ) -> Result<SessionFacts> {
        let session = self.session(session_id)?;
        let facts = self.session_facts(&session).await?;
        session.closed.store(true, Ordering::Relaxed);
        session.protocol.shutdown().await;
        let mut process = session.process.lock().await;
        if let Some(child) = process.as_mut() {
            let _ = child.start_kill();
        }
        *process = None;
        drop(process);
        let mut display = session.display_handle.lock().await;
        *display = None;
        drop(display);
        {
            let mut state = self.state();
            state.sessions.remove(&session_id);
            if let Some(identity) = state.identities.get_mut(&session.profile_id) {
                if identity
                    .writer
                    .as_ref()
                    .is_some_and(|writer| writer.session_id == session_id)
                {
                    identity.writer = None;
                }
            }
        }
        self.release_writer(&session.profile_id, caller);
        Ok(facts)
    }

    /// Every live session of this provider.
    pub async fn list_sessions(&self) -> Vec<SessionFacts> {
        let sessions: Vec<Arc<BrowserSession>> = {
            let state = self.state();
            state.sessions.values().cloned().collect()
        };
        let mut facts = Vec::new();
        for session in sessions {
            if let Ok(facts) = self.session_facts(&session).await {
                facts.push(facts);
            }
        }
        facts
    }

    pub(crate) fn session(&self, session_id: EntityId) -> Result<Arc<BrowserSession>> {
        let state = self.state();
        let session = state
            .sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("browser session {session_id} is not live in this process"))?;
        if session.closed.load(Ordering::Relaxed) {
            bail!("browser session {session_id} is closed");
        }
        Ok(session)
    }

    pub(crate) async fn session_facts(&self, session: &Arc<BrowserSession>) -> Result<SessionFacts> {
        let tabs = session.tabs.lock().await;
        let tab_facts = tabs
            .values()
            .map(|tab| TabFacts {
                tab_id: tab.handle.id.clone(),
                group: tab.group.clone(),
                url: tab.url.clone(),
                title: tab.title.clone(),
                revision: tab.revision,
            })
            .collect();
        drop(tabs);
        let writer = {
            let state = self.state();
            state
                .identities
                .get(&session.profile_id)
                .and_then(|identity| identity.writer.as_ref().map(writer_facts))
        };
        Ok(SessionFacts {
            session_id: session.session_id,
            profile_id: session.profile_id.clone(),
            run_id: session.run_id.clone(),
            engine_name: session.engine.name.clone(),
            engine_version: session.engine.version.clone(),
            engine_path: session.engine.path.to_string_lossy().to_string(),
            engine_family: match session.engine.family {
                EngineFamily::Chromium => "chromium",
                EngineFamily::Firefox => "firefox",
            }
            .into(),
            protocol: session.protocol.kind().to_string(),
            product: session.product.clone(),
            mode_requested: session.mode_requested.clone(),
            mode_effective: session.mode_effective.as_str().to_string(),
            mode_degradation: session.mode_degradation.clone(),
            display: session.display.clone(),
            display_domain_id: session.domain_id.clone(),
            endpoint: session.endpoint.clone(),
            tabs: tab_facts,
            controller_epoch: session.epoch.load(Ordering::Relaxed),
            restore: RestoreCoverage::default(),
            opened_at: now_rfc3339(),
            writer_owner: writer.unwrap_or(WriterFacts {
                instance: EntityId::nil(),
                node: String::new(),
                pid: 0,
                acquired_at: String::new(),
            }),
        })
    }

    // ------------------------------------------------------------------
    // Observation
    // ------------------------------------------------------------------

    /// Take a fresh observation of one page. The observation is
    /// content-bound: its id is the digest of its own content.
    pub async fn observe(
        &self,
        session_id: EntityId,
        tab_id: Option<&str>,
        options: ObserveOptions,
    ) -> Result<Observation> {
        let session = self.session(session_id)?;
        let tab = self.resolve_tab(&session, tab_id).await?;
        let observation = self.observe_tab(&session, &tab.handle, &options).await?;
        let mut tabs = session.tabs.lock().await;
        if let Some(entry) = tabs.get_mut(&tab.handle.id) {
            entry.url = observation.url.clone();
            entry.title = observation.title.clone();
            entry.revision += 1;
        }
        drop(tabs);
        session
            .observations
            .lock()
            .await
            .insert(tab.handle.id.clone(), observation.clone());
        Ok(observation)
    }

    pub(crate) async fn resolve_tab(
        &self,
        session: &Arc<BrowserSession>,
        tab_id: Option<&str>,
    ) -> Result<super::TabEntry> {
        let tabs = session.tabs.lock().await;
        match tab_id {
            Some(tab_id) => tabs
                .get(tab_id)
                .cloned()
                .ok_or_else(|| anyhow!("tab {tab_id} is not part of this browser session")),
            None => tabs
                .values()
                .next()
                .cloned()
                .ok_or_else(|| anyhow!("this browser session has no open tab")),
        }
    }

    /// Observe one tab through the real protocol client.
    pub(crate) async fn observe_tab(
        &self,
        session: &Arc<BrowserSession>,
        tab: &TabHandle,
        options: &ObserveOptions,
    ) -> Result<Observation> {
        let raw = session
            .protocol
            .evaluate_json(tab, &super::scripts::snapshot_script())
            .await
            .context("read the page snapshot")?;
        let url = string_field(&raw, "url");
        let title = string_field(&raw, "title");
        let ready_state = string_field(&raw, "readyState");
        let text = string_field(&raw, "text");
        let text_truncated = raw
            .get("textTruncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let viewport = raw
            .get("viewport")
            .map(|viewport| Viewport {
                width: viewport
                    .get("width")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                height: viewport
                    .get("height")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                device_scale_factor: viewport
                    .get("deviceScaleFactor")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0),
            })
            .unwrap_or(Viewport {
                width: 0,
                height: 0,
                device_scale_factor: 1.0,
            });
        let links = targets(&raw, "links");
        let inputs = targets(&raw, "inputs");
        let buttons = targets(&raw, "buttons");

        let mut ax_nodes = None;
        let mut ax_available = false;
        let mut ax_note = None;
        if options.accessibility {
            match session.protocol.ax_nodes(tab).await {
                Ok(Some(nodes)) => {
                    ax_available = true;
                    ax_nodes = Some(nodes);
                }
                Ok(None) => {
                    ax_note = Some(
                        "this engine exposes no accessibility tree through its protocol \
                         (CDP does, BiDi does not in this adapter)"
                            .into(),
                    );
                }
                Err(error) => {
                    ax_note = Some(format!("accessibility tree unavailable: {error}"));
                }
            }
        } else {
            ax_note = Some("accessibility tree was not requested for this observation".into());
        }

        let mut screenshot_digest = None;
        if options.screenshot {
            match session.protocol.screenshot(tab).await {
                Ok(bytes) => {
                    let sealed = self.store.cas.seal_object(
                        &bytes,
                        "image/png",
                        "browser-effects",
                        "browser-screenshot",
                        1,
                    );
                    match sealed {
                        Ok(sealed) => {
                            screenshot_digest = Some(sealed.object.digest.to_string());
                        }
                        Err(error) => {
                            self.store.close_external_writer();
                            return Err(error.into());
                        }
                    }
                }
                Err(error) => {
                    screenshot_digest = None;
                    tracing::warn!(%error, "browser screenshot failed; the observation stays without one");
                }
            }
        }

        let identity_digest = digest_hex(
            format!("{url}\n{title}\n{ready_state}\n{}", structure_digest(&raw)).as_bytes(),
        );
        let content_digest = digest_hex(
            format!("{url}\n{title}\n{ready_state}\n{text}\n{}", structure_digest(&raw)).as_bytes(),
        );
        let revision = {
            let tabs = session.tabs.lock().await;
            tabs.get(&tab.id).map(|entry| entry.revision).unwrap_or(0)
        };
        let observation = Observation {
            observation_id: String::new(),
            session_id: session.session_id,
            profile_id: session.profile_id.clone(),
            tab_id: tab.id.clone(),
            group: session
                .tabs
                .lock()
                .await
                .get(&tab.id)
                .map(|entry| entry.group.clone())
                .unwrap_or_default(),
            url,
            title,
            ready_state,
            revision,
            captured_at: now_rfc3339(),
            viewport,
            text,
            text_truncated,
            links,
            inputs,
            buttons,
            ax_nodes,
            ax_available,
            ax_note,
            content_digest,
            identity_digest,
            controller_epoch: session.epoch.load(Ordering::Relaxed),
            valid_for_ms: OBSERVATION_VALIDITY_MS,
            screenshot_digest,
            restore: RestoreCoverage::default(),
        };
        let observation_id = digest_hex(
            serde_json::to_vec(&serde_json::json!({
                "session": observation.session_id.to_string(),
                "tab": &observation.tab_id,
                "url": &observation.url,
                "title": &observation.title,
                "content": &observation.content_digest,
                "identity": &observation.identity_digest,
                "epoch": observation.controller_epoch,
            }))
            .unwrap_or_default()
            .as_slice(),
        );
        Ok(Observation {
            observation_id,
            ..observation
        })
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn targets(raw: &Value, key: &str) -> Vec<PageTarget> {
    raw.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(super::protocol::MAX_TARGETS)
                .filter_map(|item| {
                    Some(PageTarget {
                        role: item.get("role")?.as_str()?.to_string(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        selector: item
                            .get("selector")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        href: item
                            .get("href")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        input_type: item
                            .get("input_type")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        value_len: item
                            .get("value_len")
                            .and_then(Value::as_u64)
                            .map(|length| length as usize),
                        visible: item
                            .get("visible")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        rect: item.get("rect").and_then(|rect| {
                            let values = rect.as_array()?;
                            if values.len() != 4 {
                                return None;
                            }
                            Some([
                                values[0].as_f64().unwrap_or(0.0),
                                values[1].as_f64().unwrap_or(0.0),
                                values[2].as_f64().unwrap_or(0.0),
                                values[3].as_f64().unwrap_or(0.0),
                            ])
                        }),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A digest over the page structure (not its prose), so a live ticker does
/// not invalidate an observation while a real navigation does.
fn structure_digest(raw: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["links", "inputs", "buttons"] {
        if let Some(items) = raw.get(key).and_then(Value::as_array) {
            for item in items {
                parts.push(
                    item.get("selector")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    digest_hex(parts.join("\n").as_bytes())
}
