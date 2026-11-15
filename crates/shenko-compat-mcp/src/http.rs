//! reqwest-backed HTTP client with bounded JSON bodies.
//!
//! The SDK owns negotiation and response correlation; this backend preserves
//! its auth/status/legacy/SSE semantics while refusing oversized bodies.
use std::{borrow::Cow, collections::HashMap, sync::Arc};

use bytes::Bytes;
use futures::{Stream, StreamExt as _, stream::BoxStream};
use http::{
    HeaderMap, HeaderName, HeaderValue, StatusCode,
    header::{ACCEPT, RETRY_AFTER, WWW_AUTHENTICATE},
};
use reqwest::header::CONTENT_TYPE;
use rmcp::{
    model::{ClientJsonRpcMessage, ErrorData, JsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        common::http_header::{EVENT_STREAM_MIME_TYPE, HEADER_SESSION_ID, JSON_MIME_TYPE},
        streamable_http_client::{
            AuthRequiredError, HttpStatusError, InsufficientScopeError, StreamableHttpClient,
            StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use sse_stream::{Error as SseError, Sse, SseStream};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTrackerToken};

pub const MAX_JSON_BODY_BYTES: usize = 128 * 1024;
const MAX_JSON_BODY_CAP: usize = 1024 * 1024;
const SSE_DATA_CAP: usize = 16 * 1024 * 1024;

fn scope_from_header(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let start = lower.find("scope=")? + "scope=".len();
    let value = &header[start..];
    if let Some(value) = value.strip_prefix('"') {
        value.find('"').map(|end| value[..end].to_owned())
    } else {
        let end = value
            .find(|c| c == ',' || c == ';' || char::is_whitespace(c))
            .unwrap_or(value.len());
        (end > 0).then(|| value[..end].to_owned())
    }
}

fn custom_headers(
    mut builder: reqwest::RequestBuilder,
    headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, StreamableHttpError<reqwest::Error>> {
    for (name, value) in headers {
        let reserved = name.as_str().eq_ignore_ascii_case("accept")
            || name.as_str().eq_ignore_ascii_case(HEADER_SESSION_ID)
            || name.as_str().eq_ignore_ascii_case("last-event-id");
        if reserved {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

fn authorization_error(reason: &'static str) -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(reason))
}

fn auth_error(response: &reqwest::Response) -> Option<StreamableHttpError<reqwest::Error>> {
    let status = response.status();
    let header = || {
        response
            .headers()
            .get(WWW_AUTHENTICATE)?
            .to_str()
            .ok()
            .map(str::to_owned)
    };
    match status {
        StatusCode::UNAUTHORIZED => {
            header().map(|header| StreamableHttpError::AuthRequired(AuthRequiredError::new(header)))
        }
        StatusCode::FORBIDDEN => header().map(|header| {
            StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                header.clone(),
                scope_from_header(&header),
            ))
        }),
        _ => None,
    }
}

fn status_error(status: StatusCode, headers: &HeaderMap) -> Option<HttpStatusError> {
    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
        return None;
    }
    HttpStatusError::new(
        status.as_u16(),
        headers
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    )
    .into()
}

fn too_large(max_bytes: usize) -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!(
        "HTTP response body exceeds {max_bytes} bytes"
    )))
}

async fn bounded_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    if response
        .content_length()
        .is_some_and(|bytes| bytes > max_bytes as u64)
    {
        return Err(too_large(max_bytes));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(StreamableHttpError::Client)?;
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            return Err(too_large(max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn json_rpc_error(body: &[u8]) -> Option<ServerJsonRpcMessage> {
    serde_json::from_slice::<ServerJsonRpcMessage>(body)
        .ok()
        .filter(|message| matches!(message, JsonRpcMessage::Error(_)))
}

fn legacy_discover(
    message: &ClientJsonRpcMessage,
    session_attached: bool,
    status: StatusCode,
    body: &[u8],
) -> Option<StreamableHttpPostResponse> {
    if session_attached
        || !status.is_client_error()
        || matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    {
        return None;
    }
    let JsonRpcMessage::Request(request) = message else {
        return None;
    };
    if !matches!(
        request.request,
        rmcp::model::ClientRequest::DiscoverRequest(_)
    ) {
        return None;
    }
    let error = match serde_json::from_slice::<ServerJsonRpcMessage>(body) {
        Ok(JsonRpcMessage::Error(error)) => error.error,
        _ => ErrorData::invalid_request(
            format!(
                "server/discover rejected with HTTP {status}: {}",
                String::from_utf8_lossy(body)
            ),
            None,
        ),
    };
    Some(StreamableHttpPostResponse::Json(
        JsonRpcMessage::error(error, Some(request.id.clone())),
        None,
    ))
}

fn body_preview(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    if body.len() <= 256 {
        return body.into_owned();
    }
    let mut end = 256;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

fn bounded_sse<S, E>(stream: S, max_event_size: usize) -> BoxStream<'static, Result<Sse, SseError>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    // sse-stream has no public raw-event bound. This adapter counts the joined
    // non-comment data lines plus parser-inserted separators and fails the body
    // stream before one event can exceed the transport's raw limit.
    struct Limit {
        max: usize,
        retained: usize,
        line: usize,
        comment: bool,
        cr: bool,
        failed: bool,
    }
    impl Limit {
        fn observe(&mut self, chunk: &[u8]) -> bool {
            for &byte in chunk {
                if self.cr {
                    self.cr = false;
                    if byte == b'\n' {
                        continue;
                    }
                }
                match byte {
                    b'\r' => {
                        self.finish_line();
                        self.cr = true;
                    }
                    b'\n' => self.finish_line(),
                    _ => {
                        if self.line == 0 {
                            self.comment = byte == b':';
                        }
                        self.line = self.line.saturating_add(1);
                    }
                }
                if self.retained.saturating_add(self.line) > self.max {
                    return false;
                }
            }
            true
        }
        fn finish_line(&mut self) {
            if self.line == 0 {
                self.retained = 0;
            } else if !self.comment {
                self.retained = self.retained.saturating_add(self.line).saturating_add(1);
            }
            self.line = 0;
            self.comment = false;
        }
    }
    let stream = futures::stream::unfold(
        (
            stream.boxed(),
            Limit {
                max: max_event_size.min(SSE_DATA_CAP),
                retained: 0,
                line: 0,
                comment: false,
                cr: false,
                failed: false,
            },
        ),
        |(mut stream, mut limiter)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) if !limiter.failed && limiter.observe(&chunk) => {
                        return Some((Ok(chunk), (stream, limiter)));
                    }
                    Some(Ok(_)) => {
                        limiter.failed = true;
                        return Some((
                            Err(SseError::Body(
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "SSE event exceeds configured bound",
                                )
                                .into(),
                            )),
                            (stream, limiter),
                        ));
                    }
                    Some(Err(_)) => {
                        limiter.failed = true;
                        continue;
                    }
                    None => return None,
                }
            }
        },
    );
    SseStream::from_bytes_stream(stream).boxed()
}

#[derive(Clone)]
pub struct BoundedHttpClient {
    client: reqwest::Client,
    authorizer: Option<Arc<dyn crate::protocol::HttpRequestAuthorizer>>,
    retirement: CancellationToken,
    lifetime: Option<TaskTrackerToken>,
}

impl BoundedHttpClient {
    pub fn new(
        client: reqwest::Client,
        authorizer: Option<Arc<dyn crate::protocol::HttpRequestAuthorizer>>,
    ) -> Self {
        Self {
            client,
            authorizer,
            retirement: CancellationToken::new(),
            lifetime: None,
        }
    }

    pub fn with_retirement(
        mut self,
        retirement: CancellationToken,
        lifetime: TaskTrackerToken,
    ) -> Self {
        self.retirement = retirement;
        self.lifetime = Some(lifetime);
        self
    }

    fn response_stream<S, E>(
        &self,
        stream: S,
        max_event_size: usize,
    ) -> BoxStream<'static, Result<Sse, SseError>>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        let lifetime = self.lifetime.clone();
        bounded_sse(stream, max_event_size)
            .take_until(self.retirement.clone().cancelled_owned())
            .inspect(move |_| {
                let _ = &lifetime;
            })
            .boxed()
    }

    fn max_json_bytes(&self) -> usize {
        self.authorizer
            .as_ref()
            .map_or(MAX_JSON_BODY_BYTES, |authorizer| {
                authorizer.max_json_bytes().clamp(1, MAX_JSON_BODY_CAP)
            })
    }

    async fn execute(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StreamableHttpError<reqwest::Error>> {
        let mut request = builder.build()?;
        let url = request.url().clone();
        let method = request.method().clone();
        let mut client = self.client.clone();
        if let Some(authorizer) = &self.authorizer {
            if let Some(prepared) = authorizer
                .prepare_client(&url)
                .await
                .map_err(|_| authorization_error("HTTP request client preparation failed"))?
            {
                client = prepared;
            }
            // JSON-RPC HTTP bodies are materialized. Keep the frozen payload
            // invariant across a credential provider that may only add headers.
            let body = match request.body() {
                Some(body) => Some(
                    body.as_bytes()
                        .ok_or_else(|| {
                            authorization_error("HTTP authorization requires a materialized body")
                        })?
                        .to_vec(),
                ),
                None => None,
            };
            let protocol_headers = [
                "accept",
                "content-type",
                "mcp-session-id",
                "mcp-protocol-version",
                "last-event-id",
            ]
            .map(|name| {
                (
                    name,
                    request
                        .headers()
                        .get_all(name)
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            });
            authorizer
                .authorize(&mut request)
                .await
                .map_err(|_| authorization_error("HTTP request authorization failed"))?;
            if protocol_headers
                .iter()
                .any(|(name, before)| request.headers().get_all(*name).iter().ne(before.iter()))
            {
                return Err(authorization_error(
                    "HTTP authorization changed a protocol-owned header",
                ));
            }
            if request.url() != &url
                || request.method() != method
                || request.body().and_then(reqwest::Body::as_bytes) != body.as_deref()
            {
                return Err(authorization_error(
                    "HTTP authorization changed the frozen method, destination or payload",
                ));
            }
        }
        let response = client.execute(request).await?;
        if let Some(authorizer) = &self.authorizer {
            authorizer
                .observe_response(&url, response.status().as_u16(), response.headers())
                .map_err(|_| {
                    authorization_error(
                        "HTTP response authorization metadata could not be retained",
                    )
                })?;
        }
        Ok(response)
    }
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            headers,
            SSE_DATA_CAP,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let exchange = async {
            let mut request = self
                .client
                .post(uri.as_ref())
                .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
            if let Some(auth) = auth_header {
                request = request.bearer_auth(auth);
            }
            request = custom_headers(request, headers)?;
            let session_attached = session_id.is_some();
            if let Some(session) = session_id {
                request = request.header(HEADER_SESSION_ID, session.as_ref());
            }
            let response = self.execute(request.json(&message)).await?;
            if let Some(error) = auth_error(&response) {
                return Err(error);
            }
            let status = response.status();
            if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
                return Ok(StreamableHttpPostResponse::Accepted);
            }
            if status == StatusCode::NOT_FOUND && session_attached {
                return Err(StreamableHttpError::SessionExpired);
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
            let is_json = content_type
                .as_deref()
                .is_some_and(|value| value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()));
            let mut http_error = status_error(status, response.headers());
            if (!is_json || status == StatusCode::TOO_MANY_REQUESTS)
                && let Some(error) = http_error.take()
            {
                return Err(error.into());
            }
            let response_session = response
                .headers()
                .get(HEADER_SESSION_ID)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if status.is_success()
                && response.content_length() == Some(0)
                && matches!(
                    message,
                    JsonRpcMessage::Notification(_)
                        | JsonRpcMessage::Response(_)
                        | JsonRpcMessage::Error(_)
                )
            {
                return Ok(StreamableHttpPostResponse::Accepted);
            }
            if !status.is_success() {
                let body = bounded_body(response, self.max_json_bytes()).await?;
                if let Some(response) = legacy_discover(&message, session_attached, status, &body) {
                    return Ok(response);
                }
                let auth_failure =
                    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
                if !auth_failure
                    && is_json
                    && let Some(message) = json_rpc_error(&body)
                {
                    return Ok(StreamableHttpPostResponse::Json(message, response_session));
                }
                if let Some(error) = http_error {
                    return Err(error.into());
                }
                return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                    format!("HTTP {status}: {}", body_preview(&body)),
                )));
            }
            match content_type.as_deref() {
                Some(value)
                    if value
                        .as_bytes()
                        .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) =>
                {
                    Ok(StreamableHttpPostResponse::Sse(
                        self.response_stream(response.bytes_stream(), max_sse_event_size),
                        response_session,
                    ))
                }
                Some(value) if value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
                    let body = bounded_body(response, self.max_json_bytes()).await?;
                    match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                        Ok(message) => {
                            Ok(StreamableHttpPostResponse::Json(message, response_session))
                        }
                        Err(error)
                            if matches!(
                                message,
                                JsonRpcMessage::Notification(_)
                                    | JsonRpcMessage::Response(_)
                                    | JsonRpcMessage::Error(_)
                            ) =>
                        {
                            tracing::warn!(
                                "could not parse bounded JSON response as JSON-RPC, treating as accepted: {error}"
                            );
                            Ok(StreamableHttpPostResponse::Accepted)
                        }
                        Err(error) => Err(StreamableHttpError::UnexpectedServerResponse(
                            Cow::Owned(format!(
                                "could not parse JSON response as ServerJsonRpcMessage: {error}: {}",
                                body_preview(&body)
                            )),
                        )),
                    }
                }
                _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
            }
        };
        tokio::select! { biased;
            _ = self.retirement.cancelled() => Err(authorization_error("HTTP transport retired")),
            result = exchange => result,
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .delete(uri.as_ref())
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(auth) = auth_header {
            request = request.bearer_auth(auth);
        }
        request = custom_headers(request, headers)?;
        let response = self.execute(request).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        let _ = response.error_for_status()?;
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            headers,
            SSE_DATA_CAP,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let exchange = async {
            let mut request = self
                .client
                .get(uri.as_ref())
                .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
            if let Some(session) = session_id {
                request = request.header(HEADER_SESSION_ID, session.as_ref());
            }
            if let Some(last) = last_event_id {
                request = request.header("last-event-id", last);
            }
            if let Some(auth) = auth_header {
                request = request.bearer_auth(auth);
            }
            request = custom_headers(request, headers)?;
            let response = self.execute(request).await?;
            if response.status() == StatusCode::METHOD_NOT_ALLOWED {
                return Err(StreamableHttpError::ServerDoesNotSupportSse);
            }
            if let Some(error) = auth_error(&response) {
                return Err(error);
            }
            if let Some(error) = status_error(response.status(), response.headers()) {
                return Err(error.into());
            }
            let response = response.error_for_status()?;
            let content_type = response.headers().get(CONTENT_TYPE);
            match content_type {
                Some(value)
                    if value
                        .as_bytes()
                        .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                        || value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) =>
                {
                    Ok(self.response_stream(response.bytes_stream(), max_sse_event_size))
                }
                _ => Err(StreamableHttpError::UnexpectedContentType(
                    content_type
                        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned()),
                )),
            }
        };
        tokio::select! { biased;
            _ = self.retirement.cancelled() => Err(authorization_error("HTTP transport retired")),
            result = exchange => result,
        }
    }
}
