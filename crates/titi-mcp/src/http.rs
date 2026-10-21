//! One Streamable HTTP connection to an MCP server.
//!
//! Transport: Model Context Protocol revision **2025-11-25**, "Streamable HTTP"
//! (<https://modelcontextprotocol.io/specification/2025-11-25/basic/transports>).
//!
//! * Every JSON-RPC message the client sends is its own `POST` to the one MCP
//!   endpoint, with `Accept: application/json, text/event-stream`. A request is
//!   answered either by one `application/json` body or by a
//!   `text/event-stream` that ends with the response; both are read. A
//!   notification or a reply to a server request is answered `202` with no
//!   body.
//! * The `MCP-Session-Id` the `initialize` response carries is sent on every
//!   later request, and so is `MCP-Protocol-Version` once the revision is
//!   agreed. A `404` on a request that carried a session id means the session
//!   ended: the connection reports itself closed and the manager starts a new
//!   one (a new `initialize`, no session id).
//! * On shutdown the session is ended with `DELETE` (a `405` is fine: the
//!   server may not allow it).
//!
//! Not implemented, on purpose:
//!
//! * The optional `GET` stream for server-initiated messages. A server can
//!   still send notifications and requests on the stream a `POST` opens, and
//!   those are handled (`tools/list_changed`, `ping`); it just cannot speak
//!   between requests.
//! * Resuming a stream (`Last-Event-ID`, `retry`): a stream that ends before
//!   the response fails the request.
//! * Sign-in: HTTP `401` is [`McpError::NeedsAuth`].
//!
//! Limits are the stdio client's: a message (a JSON body, or one event's
//! data) over [`MAX_LINE_BYTES`] fails the request, the whole exchange is
//! bounded by the server's timeout, and redirects are not followed — a
//! redirect would carry the request, headers included, somewhere the
//! definition never named.
//!
//! Nothing is logged or put in an error that could hold a header value, and
//! the URL in an error is stripped of its query.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::client::{ClientEvent, MAX_LINE_BYTES, McpError, plain};

/// How to reach one HTTP server. Header values are already expanded: this
/// layer sends exactly what it is given and nothing else.
#[derive(Clone)]
pub struct HttpLaunch {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Bound on each request, the handshake included.
    pub timeout: Duration,
}

impl std::fmt::Debug for HttpLaunch {
    /// Header values are secrets: only their names are printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpLaunch")
            .field("url", &display_url(&self.url))
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Most a stream may carry in all, however many events it is cut into: a
/// stream that never answers is also bounded by the timeout, this bounds the
/// memory it can make the client read before that.
const MAX_STREAM_BYTES: usize = 4 * MAX_LINE_BYTES;

/// Server requests answered on one stream before the rest are ignored.
const MAX_STREAM_REPLIES: usize = 32;

/// Longest session id kept. The spec bounds the alphabet, not the length.
const MAX_SESSION_ID: usize = 512;

/// How long the session `DELETE` may take at shutdown.
const DELETE_WAIT: Duration = Duration::from_secs(2);

fn display_url(url: &str) -> String {
    match url.find(['?', '#']) {
        Some(at) => format!("{}?…", &url[..at]),
        None => url.to_owned(),
    }
}

fn client_for(timeout: Duration) -> Result<reqwest::Client, McpError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .build()
        .map_err(|error| McpError::Protocol(format!("no HTTP client: {}", error.without_url())))
}

pub(crate) struct HttpConn {
    http: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
    next_id: AtomicU64,
    timeout: Duration,
    closed: Mutex<Option<String>>,
    events: UnboundedSender<ClientEvent>,
    tools_pending: AtomicBool,
    leaving: AtomicBool,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cancels an unanswered request when its future is dropped or times out, as
/// the stdio client does: the server is told to stop, in the background.
struct Cancel {
    conn: Arc<HttpConn>,
    id: u64,
    armed: bool,
}

impl Drop for Cancel {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let conn = Arc::clone(&self.conn);
        let id = self.id;
        runtime.spawn(async move {
            let _ = conn
                .notify(
                    "notifications/cancelled",
                    json!({ "requestId": id, "reason": "titi stopped waiting" }),
                )
                .await;
        });
    }
}

impl HttpConn {
    pub(crate) fn new(
        launch: &HttpLaunch,
        events: UnboundedSender<ClientEvent>,
    ) -> Result<Self, McpError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &launch.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                McpError::Protocol(format!("the header name {name:?} is not valid"))
            })?;
            let mut value = HeaderValue::from_str(value).map_err(|_| {
                McpError::Protocol(format!("the value of header {name} is not valid"))
            })?;
            value.set_sensitive(true);
            headers.append(name, value);
        }
        Ok(Self {
            http: client_for(launch.timeout)?,
            url: launch.url.clone(),
            headers,
            session: Mutex::new(None),
            version: Mutex::new(None),
            next_id: AtomicU64::new(1),
            timeout: launch.timeout,
            closed: Mutex::new(None),
            events,
            tools_pending: AtomicBool::new(false),
            leaving: AtomicBool::new(false),
        })
    }

    pub(crate) fn negotiated(&self, version: &str) {
        *locked(&self.version) = Some(version.to_owned());
    }

    pub(crate) fn clear_tools_pending(&self) {
        self.tools_pending.store(false, Ordering::SeqCst);
    }

    pub(crate) fn is_alive(&self) -> bool {
        locked(&self.closed).is_none()
    }

    /// The headers every request carries: the definition's own, then the
    /// protocol's, which win.
    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(version) = locked(&self.version).as_deref()
            && let Ok(value) = HeaderValue::from_str(version)
        {
            headers.insert("mcp-protocol-version", value);
        }
        if let Some(session) = locked(&self.session).as_deref()
            && let Ok(value) = HeaderValue::from_str(session)
        {
            headers.insert("mcp-session-id", value);
        }
        headers
    }

    fn close(&self, reason: &str) {
        let first = {
            let mut closed = locked(&self.closed);
            let first = closed.is_none();
            if first {
                *closed = Some(reason.to_owned());
            }
            first
        };
        if first && !self.leaving.load(Ordering::SeqCst) {
            let _ = self.events.send(ClientEvent::Exited(reason.to_owned()));
        }
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, McpError> {
        let response = self
            .http
            .post(&self.url)
            .headers(self.request_headers())
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| {
                let error = error.without_url();
                McpError::Closed(format!(
                    "could not reach {}: {}",
                    display_url(&self.url),
                    plain(&error.to_string())
                ))
            })?;
        let status = response.status();
        if status.is_redirection() {
            return Err(McpError::Protocol(format!(
                "the server redirected (HTTP {}); titi does not follow redirects",
                status.as_u16()
            )));
        }
        if status.as_u16() == 401 {
            return Err(McpError::NeedsAuth);
        }
        if status.as_u16() == 404 && locked(&self.session).is_some() {
            let reason = "the session ended (HTTP 404)";
            self.close(reason);
            return Err(McpError::Closed(reason.to_owned()));
        }
        if !status.is_success() {
            return Err(McpError::Protocol(format!(
                "the server answered HTTP {}",
                status.as_u16()
            )));
        }
        Ok(response)
    }

    /// A notification, or a reply to something the server asked: one `POST`
    /// the server accepts with no body.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.post(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
            .map(|_| ())
    }

    pub(crate) async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        cancellable: bool,
    ) -> Result<Value, McpError> {
        if let Some(reason) = locked(&self.closed).clone() {
            return Err(McpError::Closed(reason));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut cancel = Cancel {
            conn: Arc::clone(self),
            id,
            armed: cancellable,
        };
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let waited = tokio::time::timeout(self.timeout, self.exchange(id, method, &body)).await;
        match waited {
            Ok(answer) => {
                cancel.armed = false;
                answer
            }
            Err(_) => Err(McpError::Timeout {
                method: method.to_owned(),
                secs: self.timeout.as_secs(),
            }),
        }
    }

    async fn exchange(&self, id: u64, method: &str, body: &Value) -> Result<Value, McpError> {
        let response = self.post(body).await?;
        if method == "initialize"
            && let Some(session) = response.headers().get("mcp-session-id")
        {
            let session = session
                .to_str()
                .ok()
                .filter(|text| {
                    !text.is_empty()
                        && text.len() <= MAX_SESSION_ID
                        && text.bytes().all(|b| (0x21..=0x7e).contains(&b))
                })
                .ok_or_else(|| {
                    McpError::Protocol(
                        "the server's MCP-Session-Id is not visible ASCII".to_owned(),
                    )
                })?;
            *locked(&self.session) = Some(session.to_owned());
        }
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        match kind.as_str() {
            "application/json" => self.read_json(response, id).await,
            "text/event-stream" => self.read_stream(response, id).await,
            other => Err(McpError::Protocol(format!(
                "the server answered with the content type {:?}, not JSON or an event stream",
                plain(other)
            ))),
        }
    }

    async fn read_json(&self, mut response: reqwest::Response, id: u64) -> Result<Value, McpError> {
        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            && length > MAX_LINE_BYTES
        {
            return Err(too_big());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(read_failed)? {
            if bytes.len() + chunk.len() > MAX_LINE_BYTES {
                return Err(too_big());
            }
            bytes.extend_from_slice(&chunk);
        }
        let message: Value = serde_json::from_slice(&bytes)
            .map_err(|_| McpError::Protocol("the response body is not JSON".to_owned()))?;
        // A batch is not what a single request gets, but the answer in one is
        // still the answer.
        let found = match message {
            Value::Array(items) => items.into_iter().find(|item| is_response_to(item, id)),
            item => Some(item).filter(|item| is_response_to(item, id)),
        };
        match found {
            Some(message) => settle(&message),
            None => Err(McpError::Protocol(
                "the response does not answer the request".to_owned(),
            )),
        }
    }

    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        id: u64,
    ) -> Result<Value, McpError> {
        let mut parser = SseParser::default();
        let mut total = 0usize;
        let mut replies = 0usize;
        loop {
            let chunk = response.chunk().await.map_err(read_failed)?;
            let events = match &chunk {
                Some(chunk) => {
                    total += chunk.len();
                    if total > MAX_STREAM_BYTES {
                        return Err(too_big());
                    }
                    parser.push(chunk)?
                }
                None => parser.finish()?,
            };
            for data in events {
                let message: Value = serde_json::from_str(&data).map_err(|_| {
                    McpError::Protocol("an event of the stream is not JSON".to_owned())
                })?;
                if is_response_to(&message, id) {
                    return settle(&message);
                }
                self.on_stream_message(&message, &mut replies).await;
            }
            if chunk.is_none() {
                return Err(McpError::Protocol(
                    "the stream ended without answering the request".to_owned(),
                ));
            }
        }
    }

    /// What the server said on a stream besides the answer: it asked
    /// something, or announced something.
    async fn on_stream_message(&self, message: &Value, replies: &mut usize) {
        let id = message.get("id");
        let method = message.get("method").and_then(Value::as_str);
        match (id, method) {
            (Some(id), Some(method)) if *replies < MAX_STREAM_REPLIES => {
                *replies += 1;
                let reply = if method == "ping" {
                    json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                } else {
                    // No capability was declared: any other request is one the
                    // server had no business sending.
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "Method not found" },
                    })
                };
                let _ = self.post(&reply).await;
            }
            (None, Some("notifications/tools/list_changed")) => {
                if !self.tools_pending.swap(true, Ordering::SeqCst) {
                    let _ = self.events.send(ClientEvent::ToolsChanged);
                }
            }
            _ => {}
        }
    }

    /// Ends the session: `DELETE` with the session id, when there is one.
    /// Blocks for at most [`DELETE_WAIT`]; a thread of its own does the work
    /// because this runs from `Drop`, on whatever thread — inside or outside a
    /// runtime — drops the client.
    pub(crate) fn stop(&self) {
        if self.leaving.swap(true, Ordering::SeqCst) {
            return;
        }
        *locked(&self.closed) = Some("the connection was closed".to_owned());
        let Some(session) = locked(&self.session).take() else {
            return;
        };
        let mut headers = self.request_headers();
        if let Ok(value) = HeaderValue::from_str(&session) {
            headers.insert("mcp-session-id", value);
        }
        let url = self.url.clone();
        let (done, finished) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("mcp-http-delete".to_owned())
            .spawn(move || {
                // A client and a runtime of its own: the connection pool of
                // the session's client belongs to a runtime that may be the
                // one waiting for this thread.
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    && let Ok(client) = client_for(DELETE_WAIT)
                {
                    runtime.block_on(async {
                        let _ = tokio::time::timeout(
                            DELETE_WAIT,
                            client.delete(&url).headers(headers).send(),
                        )
                        .await;
                    });
                }
                let _ = done.send(());
            });
        if spawned.is_ok() {
            let _ = finished.recv_timeout(DELETE_WAIT);
        }
    }
}

impl Drop for HttpConn {
    fn drop(&mut self) {
        self.stop();
    }
}

fn too_big() -> McpError {
    McpError::Protocol(format!(
        "the server's message is larger than {} MiB",
        MAX_LINE_BYTES >> 20
    ))
}

fn read_failed(error: reqwest::Error) -> McpError {
    McpError::Closed(format!(
        "the connection broke while reading: {}",
        plain(&error.without_url().to_string())
    ))
}

fn is_response_to(message: &Value, id: u64) -> bool {
    message.get("id").and_then(Value::as_u64) == Some(id)
        && message.get("method").is_none()
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// The `result` of a JSON-RPC response, or its error.
fn settle(message: &Value) -> Result<Value, McpError> {
    match (message.get("result"), message.get("error")) {
        (_, Some(error)) => Err(McpError::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: plain(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message"),
            ),
        }),
        (Some(result), None) => Ok(result.clone()),
        (None, None) => Err(McpError::Protocol(
            "a response with neither result nor error".to_owned(),
        )),
    }
}

/// Server-Sent Events, per the WHATWG stream interpretation: lines end in
/// `\n`, `\r` or `\r\n`; `data:` lines of an event are joined with `\n`; an
/// empty line ends the event; `:` lines, `id`, `event` and `retry` are read
/// and ignored.
#[derive(Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    after_cr: bool,
}

impl SseParser {
    /// Feeds bytes; returns the `data` of every event they completed.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, McpError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut events)?;
                }
                b'\n' => self.end_line(&mut events)?,
                _ => {
                    if self.line.len() >= MAX_LINE_BYTES {
                        return Err(too_big());
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    /// The stream ended: an event that never got its empty line is still
    /// delivered, as a server closing right after its last `data:` would
    /// expect.
    pub(crate) fn finish(&mut self) -> Result<Vec<String>, McpError> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<String>) -> Result<(), McpError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(at) => {
                let value = &line[at + 1..];
                (&line[..at], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (&line[..], &line[line.len()..]),
        };
        if field == b"data" {
            if self.data.len() + value.len() + 1 > MAX_LINE_BYTES {
                return Err(too_big());
            }
            self.data.extend_from_slice(value);
            self.data.push(b'\n');
            self.has_data = true;
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        if !self.has_data {
            return;
        }
        self.has_data = false;
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        // The priming event the spec suggests has an id and no data: nothing
        // to deliver.
        if !data.is_empty() {
            events.push(String::from_utf8_lossy(&data).into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(parser.push(chunk).unwrap());
        }
        out.extend(parser.finish().unwrap());
        out
    }

    #[test]
    fn events_are_split_on_empty_lines_and_data_lines_are_joined() {
        assert_eq!(
            feed(&[b"data: one\n\ndata: {\"a\":\ndata: 1}\n\n"]),
            ["one", "{\"a\":\n1}"]
        );
    }

    #[test]
    fn crlf_cr_and_chunk_boundaries_do_not_change_the_events() {
        let whole = b"id: 1\r\nevent: message\r\ndata: x\r\n\r\ndata: y\r\r";
        assert_eq!(feed(&[whole]), ["x", "y"]);
        let cut: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(feed(&cut), ["x", "y"]);
    }

    #[test]
    fn comments_priming_events_and_unknown_fields_deliver_nothing() {
        assert_eq!(
            feed(&[b": keep-alive\n\nid: 7\ndata:\n\nretry: 100\n\nfoo: bar\n\n"]),
            Vec::<String>::new()
        );
        // One space after the colon is dropped, no more; no colon is an empty value.
        assert_eq!(feed(&[b"data:  two spaces\n\ndata\n\n"]), [" two spaces"]);
    }

    #[test]
    fn an_event_without_its_empty_line_is_delivered_when_the_stream_ends() {
        assert_eq!(feed(&[b"data: tail"]), ["tail"]);
    }

    #[test]
    fn a_line_or_event_over_the_cap_is_refused() {
        let mut parser = SseParser::default();
        let long = vec![b'a'; MAX_LINE_BYTES + 1];
        assert!(parser.push(&long).is_err());
        let mut parser = SseParser::default();
        let line = format!("data: {}\n", "b".repeat(MAX_LINE_BYTES / 2));
        assert!(parser.push(line.as_bytes()).is_ok());
        assert!(
            parser.push(line.as_bytes()).is_err(),
            "two lines of one event add up"
        );
    }

    #[test]
    fn debug_never_prints_a_header_value_or_a_query() {
        let launch = HttpLaunch {
            url: "https://x.invalid/mcp?key=hunter2".to_owned(),
            headers: vec![("Authorization".to_owned(), "Bearer hunter2".to_owned())],
            timeout: Duration::from_secs(1),
        };
        let text = format!("{launch:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("Authorization"));
    }
}
