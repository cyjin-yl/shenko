//! Protocol-only MCP client. No server implementation, vendor catalogue,
//! installer or account authorization flow is embedded here.

use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, process::Stdio, sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::{
    ClientLifecycleMode, RoleClient,
    model::{ClientCapabilities, ClientConfig, ClientRequest, GetExtensions, Implementation, ProtocolVersion},
    service::{
        PeerRequestOptions, RunningService, RxJsonRpcMessage, ServiceError, TxJsonRpcMessage,
        serve_client_with_lifecycle_and_ct,
    },
    transport::{
        StreamableHttpClientTransport, Transport, async_rw::AsyncRwTransport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Per-request authorization owned by the application that holds credentials.
/// Implementations only change headers, never the method, URL or body that the
/// durable caller froze. Proofs are made after all protocol headers are present.
/// There is no retry on an authorization challenge or a failed observation.
pub trait HttpRequestAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    /// Retain the final serialized request durably, after authorization and the
    /// frozen-body checks and before any socket dispatch. The credential owner
    /// decides how to protect its bytes; this protocol layer never logs them.
    ///
    /// [`CaptureState::capture`] invokes this only on an owned blocking
    /// worker. Check `cancellation` before starting persistence and between
    /// durable stages. Cancellation cannot undo a write already in progress;
    /// preserve any partial record without granting dispatch or replay.
    fn capture_request(&self, request: &reqwest::Request, cancellation: &CancellationToken) -> Result<()>;

    /// An optional isolated client for this exact request. For discovered
    /// services this lets the credential owner pin checked DNS addresses at
    /// every send. Such a client must disable redirects and automatic retries.
    fn prepare_client<'a>(
        &'a self,
        _url: &'a reqwest::Url,
    ) -> Pin<Box<dyn Future<Output = Result<Option<reqwest::Client>>> + Send + 'a>> {
        Box::pin(async { Ok(None) })
    }

    /// A bounded response budget for a known service's large tool catalogue.
    /// The backend caps this at 1 MiB regardless of the value supplied here.
    fn max_json_bytes(&self) -> usize {
        crate::http::MAX_JSON_BODY_BYTES
    }

    /// The owning operation can interrupt negotiation and active HTTP work.
    /// Session cleanup still joins the SDK transport before returning.
    fn cancellation_token(&self) -> Option<CancellationToken> {
        None
    }

    /// Record challenge metadata (e.g. a DPoP nonce) for a later explicit
    /// operation. This callback does not authorize retransmitting a request.
    fn observe_response(
        &self,
        _url: &reqwest::Url,
        _status: u16,
        _headers: &reqwest::header::HeaderMap,
    ) -> Result<()> {
        Ok(())
    }
}

/// Why a pre-dispatch capture refused this request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureFailureKind {
    /// The capture refused or failed. Nothing was dispatched.
    Refused,
    /// The capture did not finish inside its budget. Nothing was dispatched.
    Timeout,
    /// The capture worker itself failed. Nothing was dispatched.
    Worker,
}

/// A typed local failure of the durable pre-dispatch capture.
///
/// It proves this particular request was not sent. Earlier protocol exchanges
/// and other requests retain their own outcomes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureFailure {
    pub kind: CaptureFailureKind,
    pub cause: String,
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            CaptureFailureKind::Refused => write!(
                formatter,
                "final request was not retained before dispatch: {}",
                self.cause
            ),
            CaptureFailureKind::Timeout => write!(
                formatter,
                "final request retention exceeded its budget: {}",
                self.cause
            ),
            CaptureFailureKind::Worker => write!(
                formatter,
                "final request retention worker failed: {}",
                self.cause
            ),
        }
    }
}

impl std::error::Error for CaptureFailure {}

/// The default budget for a pre-dispatch capture when no request deadline
/// applies, such as the connection handshake.
pub const DEFAULT_CAPTURE_BUDGET: Duration = Duration::from_secs(5);

/// Owns blocking capture workers through timeout and caller cancellation.
/// The operation/session must await [`Self::close`] before publishing its
/// terminal observation or releasing the vault and ledger owners.
#[derive(Clone, Default)]
pub struct CaptureState {
    inner: Arc<CaptureInner>,
}

#[derive(Default)]
struct CaptureInner {
    workers: Mutex<CaptureWorkers>,
    tracker: TaskTracker,
    stop: CancellationToken,
}

#[derive(Default)]
struct CaptureWorkers {
    closed: bool,
    abandoned: Vec<JoinHandle<Result<()>>>,
}

impl CaptureState {
    pub async fn capture(
        &self,
        authorizer: Arc<dyn HttpRequestAuthorizer>,
        request: &reqwest::Request,
        budget: Duration,
    ) -> Result<()> {
        // reqwest clones its materialized Body through shared Bytes, keeping
        // an abandoned worker independent without copying credential payloads.
        let request = request.try_clone().ok_or_else(|| CaptureFailure {
            kind: CaptureFailureKind::Refused,
            cause: "final request capture requires a materialized body".into(),
        })?;
        let lifetime = {
            let workers = self.inner.workers.lock();
            if workers.closed {
                return Err(CaptureFailure {
                    kind: CaptureFailureKind::Refused,
                    cause: "capture owner is retiring".into(),
                }.into());
            }
            self.inner.tracker.token()
        };
        if budget.is_zero() {
            return Err(CaptureFailure {
                kind: CaptureFailureKind::Timeout,
                cause: "no request deadline remains for retention".into(),
            }.into());
        }
        let cancellation = self.inner.stop.child_token();
        let worker_cancellation = cancellation.clone();
        let worker_lifetime = lifetime.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _lifetime = worker_lifetime;
            ensure!(!worker_cancellation.is_cancelled(), "capture cancelled before persistence");
            authorizer.capture_request(&request, &worker_cancellation)
        });
        let mut pending = PendingCapture {
            owner: self.clone(), worker: Some(worker), cancellation, _lifetime: lifetime,
        };
        let worker = pending.worker.as_mut().context("capture worker ownership missing")?;
        match tokio::time::timeout(budget, worker).await {
            Ok(result) => {
                pending.worker.take();
                capture_result(result)
            }
            Err(_) => Err(CaptureFailure {
                kind: CaptureFailureKind::Timeout,
                cause: format!("final request retention exceeded {budget:?}"),
            }.into()),
        }
    }

    pub fn stop(&self) {
        self.inner.workers.lock().closed = true;
        self.inner.stop.cancel();
        self.inner.tracker.close();
    }

    /// A blocking filesystem call cannot be forcibly cancelled. Wait for it
    /// and retain any late failure instead of detaching it after a timeout.
    pub async fn close(&self) -> Result<()> {
        self.stop();
        self.inner.tracker.wait().await;
        let abandoned = std::mem::take(&mut self.inner.workers.lock().abandoned);
        let mut failure: Option<anyhow::Error> = None;
        for worker in abandoned {
            if let Err(error) = capture_result(worker.await) {
                failure = Some(match failure {
                    Some(primary) => primary.context(format!("another capture also failed: {error:#}")),
                    None => error,
                });
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct PendingCapture {
    owner: CaptureState,
    worker: Option<JoinHandle<Result<()>>>,
    cancellation: CancellationToken,
    _lifetime: TaskTrackerToken,
}

impl Drop for PendingCapture {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            self.owner.inner.workers.lock().abandoned.push(worker);
        }
    }
}

fn capture_result(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(match error.downcast::<CaptureFailure>() {
            Ok(failure) => failure.into(),
            Err(error) => CaptureFailure {
                kind: CaptureFailureKind::Refused, cause: format!("{error:#}"),
            }.into(),
        }),
        Err(error) => Err(CaptureFailure {
            kind: CaptureFailureKind::Worker, cause: error.to_string(),
        }.into()),
    }
}

/// Carried in the SDK's non-serialized request extensions. Budgets and typed
/// failures belong to the exact RPC, including concurrent calls; SSE, DELETE
/// and another request cannot lend it a local no-dispatch observation.
#[derive(Clone)]
pub(crate) struct RequestCapture {
    deadline: Instant,
    budget: Duration,
    failure: Arc<Mutex<Option<CaptureFailure>>>,
}

impl RequestCapture {
    fn new(deadline: Duration) -> Self {
        Self {
            deadline: Instant::now() + deadline, budget: capture_budget(deadline),
            failure: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn budget(&self) -> Duration {
        self.budget.min(self.deadline.saturating_duration_since(Instant::now()))
    }

    pub(crate) fn record_failure(&self, failure: CaptureFailure) {
        self.failure.lock().get_or_insert(failure);
    }

    fn failure(&self) -> Option<CaptureFailure> {
        self.failure.lock().clone()
    }
}

/// Leave time for the remote exchange, without extending short deadlines.
pub fn capture_budget(deadline: Duration) -> Duration {
    (deadline / 4).min(DEFAULT_CAPTURE_BUDGET)
}

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
            Self::Stdio {
                program,
                args,
                env_refs,
                ..
            } => {
                ensure!(!program.as_os_str().is_empty(), "MCP program is empty");
                ensure!(
                    args.len() <= 64
                        && args
                            .iter()
                            .all(|arg| arg.len() <= 4096 && !arg.contains('\0')),
                    "invalid MCP command arguments"
                );
                for (key, source) in env_refs {
                    ensure!(
                        !key.is_empty()
                            && !key.contains(['=', '\0'])
                            && !source.is_empty()
                            && !source.contains(['=', '\0']),
                        "invalid MCP environment reference"
                    );
                }
            }
            Self::StreamableHttp {
                url,
                header_env_refs,
            } => {
                let url = reqwest::Url::parse(url).context("invalid MCP endpoint URL")?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none(),
                    "MCP endpoint must be HTTP(S) without URL credentials or fragment"
                );
                for (header, source) in header_env_refs {
                    reqwest::header::HeaderName::from_bytes(header.as_bytes())
                        .context("invalid MCP header name")?;
                    ensure!(
                        !source.is_empty() && !source.contains(['=', '\0']),
                        "invalid MCP header environment reference"
                    );
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

/// Owns negotiation, transport tasks and all HTTP stream lifetimes. Callers
/// retain their work future through cancellation and await `close()`; Drop
/// signals stop but cannot perform asynchronous joining.
///
/// Embeddings must suppress RMCP payload logging (including `rmcp::*` targets):
/// the SDK emits peer metadata/notifications at INFO and complete RPC payloads
/// at TRACE, which can contain credentials even when HTTP headers are marked
/// sensitive. Shenko's CLI enforces this boundary in its subscriber.
pub struct Session {
    service: RunningService<RoleClient, ClientConfig>,
    child: Option<Box<dyn ChildWrapper>>,
    retirement: CancellationToken,
    // The cleanup future is polled only by the exclusive owner in close().
    // A mutex keeps Session shareable while that Send future is parked.
    reclaim: Mutex<Option<ReclaimFuture>>,
    http_lifetimes: TaskTracker,
    capture_state: CaptureState,
    capture_requests: bool,
}

impl Session {
    /// Only called for an explicitly configured external service. The caller
    /// must arm its durable operation before this can launch a process.
    pub async fn connect(spec: &ConnectionSpec, deadline: Duration) -> Result<Self> {
        Self::connect_with_authorizer(spec, deadline, None).await
    }

    /// Use a live credential/proof provider for every HTTP exchange, including
    /// negotiation, notifications, SSE connection/resumption and DELETE.
    pub async fn connect_authorized(
        spec: &ConnectionSpec,
        deadline: Duration,
        authorizer: Arc<dyn HttpRequestAuthorizer>,
    ) -> Result<Self> {
        ensure!(
            matches!(spec, ConnectionSpec::StreamableHttp { .. }),
            "HTTP authorization requires an HTTP MCP connection"
        );
        Self::connect_with_authorizer(spec, deadline, Some(authorizer)).await
    }

    async fn connect_with_authorizer(
        spec: &ConnectionSpec,
        deadline: Duration,
        authorizer: Option<Arc<dyn HttpRequestAuthorizer>>,
    ) -> Result<Self> {
        spec.validate()?;
        let client = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("shenko-mcp-compat", env!("CARGO_PKG_VERSION")),
        );
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let retirement = authorizer
            .as_ref()
            .and_then(|provider| provider.cancellation_token())
            .map_or_else(CancellationToken::new, |token| token.child_token());
        let http_lifetimes = TaskTracker::new();
        let mut reclaim = None;
        let mut child: Option<Box<dyn ChildWrapper>> = None;
        let capture_state = CaptureState::default();
        let connection_capture = RequestCapture::new(deadline);
        let connect = async {
            match spec {
                ConnectionSpec::Stdio {
                    program,
                    args,
                    cwd,
                    env_refs,
                } => {
                    let mut command = tokio::process::Command::new(program);
                    command.args(args).env_clear();
                    if let Some(cwd) = cwd {
                        command.current_dir(cwd);
                    }
                    for (key, source) in env_refs {
                        let value = std::env::var_os(source).with_context(|| {
                            format!("MCP environment reference is unavailable: {source}")
                        })?;
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
                    wrapped
                        .command_mut()
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());
                    child = Some(wrapped.spawn()?);
                    let process = child
                        .as_mut()
                        .context("MCP process disappeared after spawn")?;
                    let stdout = process
                        .inner_mut()
                        .stdout()
                        .take()
                        .context("MCP stdout pipe missing")?;
                    let stdin = process
                        .inner_mut()
                        .stdin()
                        .take()
                        .context("MCP stdin pipe missing")?;
                    let transport = AsyncRwTransport::new_client(stdout, stdin);
                    let (transport, cleanup) = ReclaimTransport::new(transport, || {
                        std::io::Error::other("MCP transport already reclaimed")
                    });
                    reclaim = Some(cleanup);
                    serve_client_with_lifecycle_and_ct(
                        client,
                        transport,
                        lifecycle,
                        retirement.clone(),
                    )
                    .await
                    .context("MCP stdio negotiation failed")
                }
                ConnectionSpec::StreamableHttp {
                    url,
                    header_env_refs,
                } => {
                    // No POST replay on expired sessions, HTTP/2 retries or
                    // redirects. Reconnecting SSE resumes responses, not calls.
                    let http = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .retry(reqwest::retry::never())
                        .build()?;
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .reinit_on_expired_session(false);
                    for (header, source) in header_env_refs {
                        let value = std::env::var(source).with_context(|| {
                            format!("MCP header environment reference is unavailable: {source}")
                        })?;
                        let name = reqwest::header::HeaderName::from_bytes(header.as_bytes())?;
                        let mut value = reqwest::header::HeaderValue::from_str(&value)
                            .context("invalid MCP credential header value")?;
                        value.set_sensitive(true);
                        config.custom_headers.insert(name, value);
                    }
                    let http = crate::http::BoundedHttpClient::new(
                        http, authorizer.clone(), capture_state.clone(), connection_capture.clone(),
                    ).with_retirement(retirement.clone(), http_lifetimes.token());
                    let transport = StreamableHttpClientTransport::with_client(http, config);
                    let (transport, cleanup) = ReclaimTransport::new(transport, || {
                        rmcp::transport::streamable_http_client::StreamableHttpError::TransportChannelClosed
                    });
                    reclaim = Some(cleanup);
                    serve_client_with_lifecycle_and_ct(
                        client,
                        transport,
                        lifecycle,
                        retirement.clone(),
                    )
                    .await
                    .context("MCP HTTP negotiation failed")
                }
            }
        };
        let result = tokio::time::timeout(deadline, connect)
            .await
            .context("MCP connection deadline elapsed")
            .and_then(|result| result);
        match result {
            Ok(service) => Ok(Self {
                service,
                child,
                retirement,
                reclaim: Mutex::new(reclaim),
                http_lifetimes,
                capture_state,
                capture_requests: authorizer.is_some(),
            }),
            Err(error) => {
                let error = match connection_capture.failure() {
                    Some(failure) => anyhow::Error::new(failure).context(error),
                    None => error,
                };
                retirement.cancel();
                let process = async {
                    if let Some(mut child) = child {
                        Box::into_pin(child.kill())
                            .await
                            .context("MCP process cleanup failed")?;
                    }
                    Ok(())
                };
                let transport = async {
                    if let Some(reclaim) = reclaim {
                        reclaim.await?;
                    }
                    Ok(())
                };
                let (process, transport) = tokio::join!(process, transport);
                http_lifetimes.close();
                http_lifetimes.wait().await;
                let error = match capture_state.close().await {
                    Ok(()) => error,
                    Err(cleanup) => error.context(format!("MCP capture cleanup also failed: {cleanup:#}")),
                };
                Err(with_cleanup_error(error, process, transport))
            }
        }
    }

    pub fn server_info(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.service.peer().peer_info())?)
    }

    /// One request, including custom protocol extensions. Pagination and MRTR
    /// continuations are explicit calls with their original returned tokens;
    /// this adapter never guesses a follow-up or reissues an unknown effect.
    pub async fn request(
        &self,
        mut request: ClientRequest,
        deadline: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Response> {
        if cancellation.is_cancelled() {
            return Ok(Response::Unknown {
                reason: "cancelled_before_request_dispatch",
            });
        }
        let capture = self.capture_requests.then(|| RequestCapture::new(deadline));
        if let Some(capture) = &capture {
            request.extensions_mut().insert(capture.clone());
        }
        // Bound the whole exchange, not only the response handle. Queue
        // admission and the SDK's timeout-cancellation notification can both
        // block behind a stalled transport writer.
        let exchange = async {
            let result = async {
                let handle = self
                    .service
                    .peer()
                    .send_cancellable_request(request, PeerRequestOptions::with_timeout(deadline))
                    .await?;
                handle.await_response().await
            }.await;
            Self::response(result)
        };
        let result = tokio::select! {
            biased;
            _result = exchange => _result,
            _ = cancellation.cancelled() => {
                // Interrupt the protocol worker; the owner must still await
                // close() to join the stdio group. HTTP effects may continue.
                self.retirement.cancel();
                self.service.cancellation_token().cancel();
                Ok(Response::Unknown { reason: "cancelled_without_remote_completion" })
            }
            _ = tokio::time::sleep(deadline) => {
                self.retirement.cancel();
                self.service.cancellation_token().cancel();
                Ok(Response::Unknown { reason: "request_deadline_elapsed" })
            }
        };
        // A refused capture is a local refusal, not an unknown remote
        // outcome. Only this RPC's extension can supply that observation.
        if let Some(failure) = capture.and_then(|capture| capture.failure()) {
            return Err(anyhow::Error::new(failure));
        }
        result
    }

    fn response(
        result: std::result::Result<rmcp::model::ServerResult, ServiceError>,
    ) -> Result<Response> {
        Ok(match result {
            Ok(value) => Response::Received {
                result: serde_json::to_value(value)?,
            },
            Err(ServiceError::McpError(error)) => Response::Rejected {
                error: serde_json::to_value(error)?,
            },
            Err(ServiceError::Timeout { .. }) => Response::Unknown {
                reason: "request_deadline_elapsed",
            },
            Err(ServiceError::Cancelled { .. }) => Response::Unknown {
                reason: "cancelled_without_remote_completion",
            },
            Err(_) => Response::Unknown {
                reason: "protocol_or_transport_completion_unknown",
            },
        })
    }

    pub async fn close(&mut self) -> Result<()> {
        self.retirement.cancel();
        self.service.cancellation_token().cancel();
        let process = async {
            if let Some(mut child) = self.child.take() {
                // Run beside SDK cleanup: its close may need the transport's
                // write lock, held by a send to this non-reading process.
                let waited = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                if !matches!(waited, Ok(Ok(_))) {
                    Box::into_pin(child.kill())
                        .await
                        .context("MCP process retirement failed")?;
                }
            }
            Ok(())
        };
        let (service, process) = tokio::join!(self.service.close(), process);
        let service = service.context("MCP runtime teardown failed");
        let joined = match (service, process) {
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("MCP process teardown also failed: {cleanup}")))
            }
            (Err(error), _) => Err(error),
            (Ok(_), process) => process,
        };
        let reclaim = self.reclaim.get_mut().take();
        let reclaimed = if let Some(reclaim) = reclaim {
            reclaim.await
        } else {
            Ok(())
        };
        self.http_lifetimes.close();
        // RMCP aborts SSE tasks during retirement. Their cloned HTTP clients
        // and streams carry these tokens, so abort alone is not reported as
        // drained: wait until every future has actually released its resources.
        self.http_lifetimes.wait().await;
        let reclaimed = match (reclaimed, self.capture_state.close().await) {
            (Err(error), Err(cleanup)) => Err(error.context(format!("MCP capture cleanup also failed: {cleanup:#}"))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        };
        match (joined, reclaimed) {
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("MCP transport reclaim also failed: {cleanup}")))
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.retirement.cancel();
        self.service.cancellation_token().cancel();
        self.capture_state.stop();
    }
}

type ReclaimFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// RMCP consumes its transport during negotiation and otherwise drops it on
/// errors. Return that exact transport to the owner instead, so startup
/// timeout/failure can await WorkerTransport::close rather than detach it.
struct ReclaimTransport<T: Transport<RoleClient>> {
    transport: Option<T>,
    return_to_owner: Option<tokio::sync::oneshot::Sender<T>>,
    closed_error: fn() -> T::Error,
}

impl<T: Transport<RoleClient> + 'static> ReclaimTransport<T> {
    fn new(transport: T, closed_error: fn() -> T::Error) -> (Self, ReclaimFuture) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let cleanup = Box::pin(async move {
            let mut transport: T = receiver.await.context("MCP transport ownership was lost")?;
            let result = transport
                .close()
                .await
                .context("MCP transport worker cleanup failed");
            drop(transport);
            result
        });
        (
            Self {
                transport: Some(transport),
                return_to_owner: Some(sender),
                closed_error,
            },
            cleanup,
        )
    }
}

impl<T: Transport<RoleClient>> Drop for ReclaimTransport<T> {
    fn drop(&mut self) {
        if let (Some(transport), Some(sender)) =
            (self.transport.take(), self.return_to_owner.take())
        {
            let _ = sender.send(transport);
        }
    }
}

impl<T: Transport<RoleClient> + 'static> Transport<RoleClient> for ReclaimTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let sending = self
            .transport
            .as_mut()
            .map(|transport| transport.send(item));
        let closed_error = self.closed_error;
        async move {
            let sending = sending.ok_or_else(closed_error)?;
            sending.await
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        match self.transport.as_mut() {
            Some(transport) => transport.receive().await,
            None => None,
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        if let Some(transport) = self.transport.as_mut() {
            transport.close().await?;
        }
        Ok(())
    }
}

fn with_cleanup_error(
    mut original: anyhow::Error,
    process: Result<()>,
    transport: Result<()>,
) -> anyhow::Error {
    if let Err(error) = process {
        original = original.context(format!("MCP process cleanup also failed: {error}"));
    }
    if let Err(error) = transport {
        original = original.context(format!("MCP transport cleanup also failed: {error}"));
    }
    original
}
