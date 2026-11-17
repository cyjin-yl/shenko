//! Explicit browser diagnostic batches sharing one native provider session.
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::runtime_bootstrap::create_data_directory_async;

/// `shenko browser` — drive the native browser provider directly.
///
/// Reads a JSON array of steps (`--file` or stdin) and prints one JSON result
/// per step. Steps share one provider, so `session open` handles stay valid
/// for later steps of the same batch. This is a native proof/diagnostics
/// surface; the agent-facing surface is the `browser.*` native task kinds.
pub(crate) async fn browser_batch(data_dir: &std::path::Path, file: Option<&std::path::Path>) -> Result<()> {
    use shenko_exec_host::browser::{
        ActionRequest, BrowserProvider, ControlRequest, IdentityRequest, ObserveOptions,
        PageAction, SessionRequest,
    };
    async fn action_request(
        provider: &BrowserProvider,
        step: &serde_json::Value,
        caller: shenko_types::ids::EntityId,
        actions: Vec<PageAction>,
    ) -> Result<ActionRequest> {
        let session_id = session_id_of(step)?;
        let tab_id = step.get("tab_id").and_then(serde_json::Value::as_str).map(str::to_owned);
        // Both CLI action routes use the same live observation/lease binding.
        let (observation_id, lease_token) = match (
            step.get("observation_id").and_then(serde_json::Value::as_str),
            step.get("lease_token").and_then(serde_json::Value::as_str),
        ) {
            (Some(observation), Some(token)) => (observation.to_owned(), token.parse()?),
            _ => {
                let observation = provider.observe(session_id, tab_id.as_deref(), ObserveOptions {
                    screenshot: false, accessibility: false,
                }).await?;
                let control = provider.control(ControlRequest {
                    session_id, op: "acquire".into(),
                    role: Some(shenko_exec_host::browser::ControllerRole::Agent), lease_token: None,
                }, caller).await?;
                (observation.observation_id, control.lease_token.context("acquire did not return a lease token")?)
            }
        };
        Ok(ActionRequest { session_id, tab_id, observation_id, lease_token, actions })
    }
    let input = if let Some(path) = file {
        tokio::fs::read_to_string(path).await?
    } else {
        use tokio::io::AsyncReadExt;
        let mut buffer = String::new();
        tokio::io::stdin().read_to_string(&mut buffer).await?;
        buffer
    };
    let steps: Vec<serde_json::Value> = serde_json::from_str(&input)?;
    create_data_directory_async(data_dir).await?;
    let store = crate::runtime_bootstrap::open_installation_store(data_dir)?;
    let provider = Arc::new(BrowserProvider::new(Arc::new(store), data_dir.join("browser")));
    let caller = shenko_types::ids::EntityId::new();
    let node = "browser-cli".to_string();
    for step in &steps {
        let op = step
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("session");
        let result: serde_json::Value = match op {
            "identity" => {
                let request: IdentityRequest = serde_json::from_value(step.clone())?;
                serde_json::to_value(provider.identity_op(request, caller, &node).await?)?
            }
            "session" => {
                let action = step
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("open");
                match action {
                    "open" => {
                        let request: SessionRequest = serde_json::from_value(step.clone())?;
                        serde_json::to_value(provider.open_session(request, caller, &node).await?)?
                    }
                    "close" => {
                        let session_id = session_id_of(step)?;
                        serde_json::to_value(provider.close_session(session_id, caller).await?)?
                    }
                    "list" => serde_json::to_value(provider.list_sessions().await?)?,
                    "facts" => {
                        let session_id = session_id_of(step)?;
                        let facts = provider
                            .list_sessions()
                            .await?
                            .into_iter()
                            .find(|facts| facts.session_id == session_id)
                            .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
                        serde_json::to_value(&facts)?
                    }
                    other => anyhow::bail!("unknown browser session action {other}"),
                }
            }
            "page" => {
                let action = step
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("observe");
                let session_id = session_id_of(step)?;
                match action {
                    "navigate" => {
                        let url = step
                            .get("url")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("page navigate requires a url"))?;
                        let request = action_request(&provider, step, caller,
                            vec![PageAction::Navigate { url: url.to_string() }]).await?;
                        let report = provider.act(request, caller).await?;
                        serde_json::to_value(&report)?
                    }
                    "observe" => {
                        let options = ObserveOptions {
                            screenshot: step
                                .get("screenshot")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(true),
                            accessibility: step
                                .get("accessibility")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(true),
                        };
                        let tab_id = step
                            .get("tab_id")
                            .and_then(serde_json::Value::as_str);
                        serde_json::to_value(provider.observe(session_id, tab_id, options).await?)?
                    }
                    other => anyhow::bail!("unknown browser page action {other}"),
                }
            }
            "action" => {
                let actions: Vec<PageAction> = serde_json::from_value(
                    step.get("actions")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("action step requires an actions array"))?,
                )?;
                let request = action_request(&provider, step, caller, actions).await?;
                serde_json::to_value(provider.act(request, caller).await?)?
            }
            "control" => {
                let request: ControlRequest = serde_json::from_value(step.clone())?;
                serde_json::to_value(provider.control(request, caller).await?)?
            }
            other => anyhow::bail!("unknown browser op {other}"),
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

fn session_id_of(step: &serde_json::Value) -> Result<shenko_types::ids::EntityId> {
    step.get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("step requires a session_id"))
        .and_then(|text| {
            text.parse::<shenko_types::ids::EntityId>()
                .map_err(|error| anyhow::anyhow!("invalid session_id: {error}"))
        })
}
