//! Protocol-only MCP client. No server implementation, vendor catalogue,
//! installer or account authorization flow is embedded here.

use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, ensure};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{ClientCapabilities, ClientConfig, ClientRequest, Implementation, ProtocolVersion},
    service::{PeerRequestOptions, RunningService, ServiceError},
    transport::{async_rw::AsyncRwTransport, StreamableHttpClientTransport,
        streamable_http_client::StreamableHttpClientTransportConfig},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Public connection description. Secrets are installation-local environment
/// references, resolved only while opening a connection and never serialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectionSpec {
    Stdio {
        program: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        cwd: Option<PathBuf>,
        #[serde(default)]
        env_refs: BTreeMap<String, String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default)]
        header_env_refs: BTreeMap<String, String>,
    },
}

impl ConnectionSpec {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Stdio { program, args, env_refs, .. } => {
                ensure!(!program.as_os_str().is_empty(), "MCP program is empty");
                ensure!(args.len() <= 64 && args.iter().all(|arg| arg.len() <= 4096 && !arg.contains('\0')), "invalid MCP command arguments");
                for (key, source) in env_refs {
                    ensure!(!key.is_empty() && !key.contains(['=', '\0']) && !source.is_empty() && !source.contains(['=', '\0']), "invalid MCP environment reference");
                }
            }
            Self::StreamableHttp { url, header_env_refs } => {
                let url = reqwest::Url::parse(url).context("invalid MCP endpoint URL")?;
                ensure!(matches!(url.scheme(), "http" | "https") && url.username().is_empty() && url.password().is_none() && url.fragment().is_none(), "MCP endpoint must be HTTP(S) without URL credentials or fragment");
                for (header, source) in header_env_refs {
                    reqwest::header::HeaderName::from_bytes(header.as_bytes()).context("invalid MCP header name")?;
                    ensure!(!source.is_empty() && !source.contains(['=', '\0']), "invalid MCP header environment reference");
                }
            }
        }
        Ok(())
    }
}

/// One request's actual protocol observation. An MCP result can still report
/// `isError`, `input_required`, or an ongoing task; none means business success.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Received { result: Value },
    Rejected { error: Value },
    Unknown { reason: &'static str },
}

pub struct Session {
    service: RunningService<RoleClient, ClientConfig>,
    child: Option<Box<dyn ChildWrapper>>,
}

impl Session {
    /// Only called for an explicitly configured external service. The caller
    /// must arm its durable operation before this can launch a process.
    pub async fn connect(spec: &ConnectionSpec, deadline: Duration) -> Result<Self> {
        spec.validate()?;
        let client = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("shenko-mcp-compat", env!("CARGO_PKG_VERSION")),
        );
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let mut child: Option<Box<dyn ChildWrapper>> = None;
        let connect = async {
            match spec {
                ConnectionSpec::Stdio { program, args, cwd, env_refs } => {
                    let mut command = tokio::process::Command::new(program);
                    command.args(args).env_clear();
                    if let Some(cwd) = cwd { command.current_dir(cwd); }
                    for (key, source) in env_refs {
                        let value = std::env::var_os(source).with_context(|| format!("MCP environment reference is unavailable: {source}"))?;
                        command.env(key, value);
                    }
                    let mut wrapped = CommandWrap::from(command);
                    #[cfg(unix)]
                    wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
                    #[cfg(windows)]
                    wrapped.wrap(process_wrap::tokio::JobObject);
                    wrapped.wrap(KillOnDrop);
                    // Third-party stderr must not inherit Shenko's pane/logs
                    // (it may contain account credentials). Protocol failures
                    // remain visible through the MCP connection itself.
                    wrapped.command_mut().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
                    child = Some(wrapped.spawn()?);
                    let process = child.as_mut().expect("just spawned");
                    let stdout = process.inner_mut().stdout().take().context("MCP stdout pipe missing")?;
                    let stdin = process.inner_mut().stdin().take().context("MCP stdin pipe missing")?;
                    let transport = AsyncRwTransport::new_client(stdout, stdin);
                    client.serve_with_lifecycle(transport, lifecycle).await
                        .context("MCP stdio negotiation failed")
                }
                ConnectionSpec::StreamableHttp { url, header_env_refs } => {
                    // No POST replay on expired sessions, HTTP/2 retries or
                    // redirects. Reconnecting SSE resumes responses, not calls.
                    let http = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .retry(reqwest::retry::never())
                        .build()?;
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .reinit_on_expired_session(false);
                    for (header, source) in header_env_refs {
                        let value = std::env::var(source).with_context(|| format!("MCP header environment reference is unavailable: {source}"))?;
                        let name = reqwest::header::HeaderName::from_bytes(header.as_bytes())?;
                        let mut value = reqwest::header::HeaderValue::from_str(&value).context("invalid MCP credential header value")?;
                        value.set_sensitive(true);
                        config.custom_headers.insert(name, value);
                    }
                    let transport = StreamableHttpClientTransport::with_client(crate::http::BoundedHttpClient::new(http), config);
                    client.serve_with_lifecycle(transport, lifecycle).await
                        .context("MCP HTTP negotiation failed")
                }
            }
        };
        let result = tokio::time::timeout(deadline, connect).await.context("MCP connection deadline elapsed").and_then(|result| result);
        match result {
            Ok(service) => Ok(Self { service, child }),
            Err(error) => {
                if let Some(mut child) = child {
                    if let Err(cleanup) = Box::into_pin(child.kill()).await {
                        return Err(error.context(format!("MCP process cleanup also failed: {cleanup}")));
                    }
                }
                Err(error)
            }
        }
    }

    pub fn server_info(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.service.peer().peer_info())?)
    }

    /// One request, including custom protocol extensions. Pagination and MRTR
    /// continuations are explicit calls with their original returned tokens;
    /// this adapter never guesses a follow-up or reissues an unknown effect.
    pub async fn request(&self, request: ClientRequest, deadline: Duration, cancellation: &CancellationToken) -> Result<Response> {
        if cancellation.is_cancelled() {
            return Ok(Response::Unknown { reason: "cancelled_before_request_dispatch" });
        }
        // Bound the whole exchange, not only the response handle. Queue
        // admission and the SDK's timeout-cancellation notification can both
        // block behind a stalled transport writer.
        let exchange = async {
            let handle = self.service.peer().send_cancellable_request(request, PeerRequestOptions::with_timeout(deadline)).await?;
            handle.await_response().await
        };
        let result = tokio::select! {
            biased;
            result = exchange => result,
            _ = cancellation.cancelled() => {
                // Interrupt the protocol worker; the owner must still await
                // close() to join the stdio group. HTTP effects may continue.
                self.service.cancellation_token().cancel();
                return Ok(Response::Unknown { reason: "cancelled_without_remote_completion" });
            }
            _ = tokio::time::sleep(deadline) => {
                self.service.cancellation_token().cancel();
                return Ok(Response::Unknown { reason: "request_deadline_elapsed" });
            }
        };
        Self::response(result)
    }

    fn response(result: std::result::Result<rmcp::model::ServerResult, ServiceError>) -> Result<Response> {
        Ok(match result {
            Ok(value) => Response::Received { result: serde_json::to_value(value)? },
            Err(ServiceError::McpError(error)) => Response::Rejected { error: serde_json::to_value(error)? },
            Err(ServiceError::Timeout { .. }) => Response::Unknown { reason: "request_deadline_elapsed" },
            Err(ServiceError::Cancelled { .. }) => Response::Unknown { reason: "cancelled_without_remote_completion" },
            Err(_) => Response::Unknown { reason: "protocol_or_transport_completion_unknown" },
        })
    }

    pub async fn close(&mut self) -> Result<()> {
        self.service.cancellation_token().cancel();
        let process = async {
            if let Some(mut child) = self.child.take() {
                // Run beside SDK cleanup: its close may need the transport's
                // write lock, held by a send to this non-reading process.
                let waited = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                if !matches!(waited, Ok(Ok(_))) {
                    Box::into_pin(child.kill()).await.context("MCP process retirement failed")?;
                }
            }
            Ok(())
        };
        let (service, process) = tokio::join!(self.service.close(), process);
        let service = service.context("MCP runtime teardown failed");
        match (service, process) {
            (Err(error), Err(cleanup)) => Err(error.context(format!("MCP process teardown also failed: {cleanup}"))),
            (Err(error), _) => Err(error),
            (Ok(_), process) => process,
        }
    }
}
