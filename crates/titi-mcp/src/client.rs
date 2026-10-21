//! One stdio connection to an MCP server.
//!
//! Protocol: Model Context Protocol revision **2025-11-25**
//! (<https://modelcontextprotocol.io/specification/2025-11-25>), JSON-RPC 2.0
//! over the server's standard streams, one JSON message per line. The client
//! runs the `initialize` / `notifications/initialized` handshake, lists and
//! calls tools, answers the server's `ping`, tells the server when it gives up
//! on a request (`notifications/cancelled`) and reports
//! `notifications/tools/list_changed`. It declares no client capabilities
//! (no roots, sampling or elicitation), so a server has nothing to ask of it
//! and any other request it sends is refused with "method not found".
//!
//! A server that answers `initialize` with an older revision it supports
//! (2025-06-18, 2025-03-26, 2024-11-05) is accepted: the tool messages are the
//! same. The stateless 2026-07-28 revision, which has no handshake, is not
//! implemented.
//!
//! The server is a child process in its own process group. Its standard input
//! belongs to a writer thread and its standard output to a reader thread, so
//! no async task ever blocks on a pipe, and a server that stops reading cannot
//! stall a turn. Standard error goes to a log file.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// The revision this client asks for.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Revisions a server may answer with and still be served.
pub const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Pages of `tools/list` read before the list is cut: a server that never
/// stops paging must not hold the session.
const MAX_PAGES: usize = 20;

/// Longest line read from a server. A line is one JSON-RPC message; a stream
/// that never ends one would otherwise grow without bound.
pub const MAX_LINE_BYTES: usize = 8 << 20;

/// Messages waiting to be written to a server. A server that never reads its
/// input fills this, and what does not fit is dropped (replies to its pings)
/// or fails the request.
const OUT_QUEUE: usize = 256;

/// Longest a server's stderr log grows before it starts over.
pub const STDERR_LOG_CAP: u64 = 1 << 20;

/// How long a server gets to leave after its input is closed, and after
/// `SIGTERM`, before the next, harder step.
const GRACE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    #[error("could not start {command}: {reason}")]
    Spawn { command: String, reason: String },
    /// The server's output ended: it exited, or closed its end.
    #[error("{0}")]
    Closed(String),
    #[error("{method} got no answer in {secs}s")]
    Timeout { method: String, secs: u64 },
    /// The server answered with a JSON-RPC error.
    #[error("error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("bad answer from the server: {0}")]
    Protocol(String),
    #[error(
        "the server speaks protocol {server}, which this client does not (it supports {PROTOCOL_VERSION} and older)"
    )]
    UnsupportedVersion { server: String },
}

/// What the reader thread tells whoever owns the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// `notifications/tools/list_changed`.
    ToolsChanged,
    /// The server's output ended without this side asking for it: a crash.
    Exited(String),
}

/// A tool as `tools/list` describes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// What `tools/call` answered: the text the model is told, and whether the
/// server flagged it as the tool's own failure (`isError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutput {
    pub text: String,
    pub is_error: bool,
}

/// How to start one server.
#[derive(Debug, Clone)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
    /// The whole environment the server starts with: nothing else is
    /// inherited.
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    /// Where standard error goes. `None` discards it.
    pub stderr_log: Option<PathBuf>,
    /// Bound on each request, the handshake included.
    pub timeout: Duration,
}

enum Out {
    Line(String),
    /// An answer to something the server asked. Counted apart, so a server
    /// that floods requests cannot fill the queue the client's own messages
    /// need.
    Reply(String),
    Close,
}

/// Answers to the server's requests that may wait to be written.
const MAX_QUEUED_REPLIES: usize = 32;

#[derive(Default)]
struct State {
    pending: HashMap<u64, oneshot::Sender<Result<Value, McpError>>>,
    /// Why the stream ended, once it has. A request made after that fails at
    /// once instead of waiting for a timeout.
    closed: Option<String>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    state: Arc<Mutex<State>>,
    out: SyncSender<Out>,
    child: Arc<Mutex<Child>>,
    pid: u32,
    next_id: AtomicU64,
    timeout: Duration,
    /// Set when this side ends the connection, so the reader does not report
    /// it as a crash.
    leaving: Arc<AtomicBool>,
    /// A `list_changed` has been reported and the list not yet read again.
    tools_pending: Arc<AtomicBool>,
}

/// A live connection. Dropping the last handle stops the server and the
/// process group it leads.
pub struct Client {
    inner: Arc<Inner>,
    /// What the server said about itself, for a panel.
    pub server_name: String,
    /// The revision the server answered with.
    pub version: String,
    /// Whether the server declared the `tools` capability.
    pub has_tools: bool,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.server_name)
            .field("version", &self.version)
            .finish()
    }
}

impl Client {
    /// Starts the server and runs the handshake. Errors from the handshake
    /// leave no process behind.
    pub async fn connect(
        launch: &Launch,
        events: UnboundedSender<ClientEvent>,
    ) -> Result<Self, McpError> {
        let inner = Arc::new(spawn(launch, events)?);
        let init = Handshake::run(&inner).await?;
        Ok(Self {
            inner,
            server_name: init.server_name,
            version: init.version,
            has_tools: init.has_tools,
        })
    }

    /// `tools/list`, following the cursor.
    pub async fn list_tools(&self) -> Result<Vec<ToolInfo>, McpError> {
        // Cleared first: a change announced while the pages are read is a new
        // event, not one this list already covers.
        self.inner.tools_pending.store(false, Ordering::SeqCst);
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self.inner.request("tools/list", params, true).await?;
            let listed = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol("tools/list has no `tools` list".to_owned()))?;
            for tool in listed {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.push(ToolInfo {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    // As the server sent it: `vet` decides whether it may be
                    // offered. A tool with no schema at all takes no arguments.
                    input_schema: match tool.get("inputSchema") {
                        None | Some(Value::Null) => json!({ "type": "object" }),
                        Some(schema) => schema.clone(),
                    },
                });
            }
            // More than the list may keep (`vet`) is not worth collecting.
            if tools.len() > crate::vet::MAX_TOOLS * 4 {
                break;
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// `tools/call`. A tool that ran and failed is `Ok` with `is_error`; a
    /// request the server refused, or never answered, is `Err`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallOutput, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.inner.request("tools/call", params, true).await?;
        Ok(CallOutput {
            text: render_content(&result),
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// `ping`: whether the server still answers.
    pub async fn ping(&self) -> Result<(), McpError> {
        self.inner
            .request("ping", json!({}), true)
            .await
            .map(|_| ())
    }

    /// Whether the server's output is still open.
    pub fn is_alive(&self) -> bool {
        locked(&self.inner.state).closed.is_none()
    }

    /// Stops the server now: closes its input, gives it a moment, then
    /// `SIGTERM` and `SIGKILL` to its whole process group. Blocks for at most
    /// a few hundred milliseconds; dropping the client does the same.
    pub fn shutdown(&self) {
        self.inner.stop();
    }
}

struct Handshake {
    server_name: String,
    version: String,
    has_tools: bool,
}

impl Handshake {
    async fn run(inner: &Inner) -> Result<Self, McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "titi", "version": env!("CARGO_PKG_VERSION") },
        });
        // `initialize` is the one request the protocol forbids cancelling.
        let result = inner.request("initialize", params, false).await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Protocol("initialize has no protocolVersion".to_owned()))?;
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(McpError::UnsupportedVersion {
                server: version.to_owned(),
            });
        }
        inner.notify("notifications/initialized", json!({}))?;
        Ok(Self {
            server_name: result
                .pointer("/serverInfo/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            version: version.to_owned(),
            has_tools: result.pointer("/capabilities/tools").is_some(),
        })
    }
}

impl Inner {
    fn send(&self, message: &Value) -> Result<(), McpError> {
        use std::sync::mpsc::TrySendError;
        self.out
            .try_send(Out::Line(message.to_string()))
            .map_err(|error| match error {
                TrySendError::Full(_) => {
                    McpError::Closed("the server is not reading its input".to_owned())
                }
                TrySendError::Disconnected(_) => {
                    McpError::Closed("the server's input is closed".to_owned())
                }
            })
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancellable: bool,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = locked(&self.state);
            if let Some(reason) = &state.closed {
                return Err(McpError::Closed(reason.clone()));
            }
            state.pending.insert(id, tx);
        }
        // From here, leaving this function without an answer — the timeout
        // below, or the caller dropping the future when a turn is cancelled —
        // forgets the request and tells the server to stop working on it.
        let mut guard = Pending {
            inner: self,
            id,
            cancellable,
            answered: false,
        };
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let waited = tokio::time::timeout(self.timeout, rx).await;
        match waited {
            Ok(Ok(answer)) => {
                guard.answered = true;
                answer
            }
            Ok(Err(_)) => {
                guard.answered = true;
                Err(McpError::Closed("the connection was dropped".to_owned()))
            }
            Err(_) => Err(McpError::Timeout {
                method: method.to_owned(),
                secs: self.timeout.as_secs(),
            }),
        }
    }

    /// Close the input, then the group: see [`Client::shutdown`].
    fn stop(&self) {
        if self.leaving.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.out.try_send(Out::Close);
        let exited = |within: Duration| {
            let deadline = Instant::now() + within;
            while Instant::now() < deadline {
                if matches!(locked(&self.child).try_wait(), Ok(Some(_))) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        if !exited(GRACE) {
            signal_group(self.pid, "TERM");
            if !exited(GRACE) {
                signal_group(self.pid, "KILL");
            }
        }
        // Whatever the server left in its group — a helper it started — goes
        // with it, even when the server itself left politely.
        signal_group(self.pid, "KILL");
        let mut child = locked(&self.child);
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Pending<'a> {
    inner: &'a Inner,
    id: u64,
    cancellable: bool,
    answered: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        locked(&self.inner.state).pending.remove(&self.id);
        if self.cancellable {
            let _ = self.inner.notify(
                "notifications/cancelled",
                json!({ "requestId": self.id, "reason": "titi stopped waiting" }),
            );
        }
    }
}

fn spawn(launch: &Launch, events: UnboundedSender<ClientEvent>) -> Result<Inner, McpError> {
    let mut command = Command::new(&launch.command);
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if launch.stderr_log.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    // Its own process group, so a stop reaches what the server started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|error| McpError::Spawn {
        command: launch.command.clone(),
        reason: error.to_string(),
    })?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err(McpError::Spawn {
            command: launch.command.clone(),
            reason: "no standard streams".to_owned(),
        });
    };
    if let (Some(stderr), Some(path)) = (child.stderr.take(), launch.stderr_log.clone()) {
        std::thread::spawn(move || pump_stderr(stderr, &path));
    }
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    let state = Arc::new(Mutex::new(State::default()));
    let leaving = Arc::new(AtomicBool::new(false));
    let tools_pending = Arc::new(AtomicBool::new(false));
    let (out, lines) = sync_channel::<Out>(OUT_QUEUE);

    let replies = Arc::new(AtomicUsize::new(0));
    let queued_replies = Arc::clone(&replies);
    std::thread::spawn(move || {
        while let Ok(item) = lines.recv() {
            let line = match item {
                Out::Line(line) => line,
                Out::Reply(line) => {
                    queued_replies.fetch_sub(1, Ordering::SeqCst);
                    line
                }
                Out::Close => break,
            };
            if writeln!(stdin, "{line}")
                .and_then(|()| stdin.flush())
                .is_err()
            {
                break;
            }
        }
    });

    let reader = Reader {
        state: Arc::clone(&state),
        out: out.clone(),
        child: Arc::clone(&child),
        pid,
        replies,
        leaving: Arc::clone(&leaving),
        tools_pending: Arc::clone(&tools_pending),
        events,
    };
    std::thread::spawn(move || reader.run(stdout));

    Ok(Inner {
        state,
        out,
        child,
        pid,
        next_id: AtomicU64::new(1),
        timeout: launch.timeout,
        leaving,
        tools_pending,
    })
}

/// Copies a server's standard error into its log, and starts the file over
/// when it passes [`STDERR_LOG_CAP`]: the log is for the last failure, and a
/// server that never stops talking must not fill the disk.
fn pump_stderr(mut stream: impl Read, path: &Path) {
    let Some(file) = open_log(path) else {
        // No log to write: still drain the pipe, or the server blocks on it.
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
        return;
    };
    let mut file = file;
    let mut written = file.metadata().map_or(0, |meta| meta.len());
    let mut chunk = [0u8; 8192];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        if written + read as u64 > STDERR_LOG_CAP {
            if file.set_len(0).is_err() {
                break;
            }
            written = 0;
        }
        if file.write_all(&chunk[..read]).is_err() {
            break;
        }
        written += read as u64;
    }
    let _ = std::io::copy(&mut stream, &mut std::io::sink());
}

fn open_log(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).ok()
}

enum Line {
    Message,
    TooLong,
    End,
}

/// One line into `buf`, newline excluded, never holding more than
/// [`MAX_LINE_BYTES`] of it.
fn read_capped(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Line> {
    buf.clear();
    loop {
        let (done, used) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(if buf.is_empty() {
                    Line::End
                } else {
                    Line::Message
                });
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => {
                    buf.extend_from_slice(&available[..end]);
                    (true, end + 1)
                }
                None => {
                    buf.extend_from_slice(available);
                    (false, available.len())
                }
            }
        };
        reader.consume(used);
        if buf.len() > MAX_LINE_BYTES {
            return Ok(Line::TooLong);
        }
        if done {
            return Ok(Line::Message);
        }
    }
}

struct Reader {
    state: Arc<Mutex<State>>,
    out: SyncSender<Out>,
    child: Arc<Mutex<Child>>,
    pid: u32,
    replies: Arc<AtomicUsize>,
    leaving: Arc<AtomicBool>,
    tools_pending: Arc<AtomicBool>,
    events: UnboundedSender<ClientEvent>,
}

impl Reader {
    fn run(self, stdout: std::process::ChildStdout) {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut dropped = None;
        loop {
            match read_capped(&mut reader, &mut line) {
                Ok(Line::Message) => {}
                Ok(Line::TooLong) => {
                    dropped = Some(format!(
                        "the server sent a line over {} MiB; connection dropped",
                        MAX_LINE_BYTES >> 20
                    ));
                    break;
                }
                Ok(Line::End) | Err(_) => break,
            }
            // A server may print something that is not a message; that is its
            // log, not a protocol error.
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            self.dispatch(&message);
        }
        let reason = match dropped {
            Some(reason) => {
                // It is still talking: stop it, with whatever it started.
                signal_group(self.pid, "KILL");
                let _ = locked(&self.child).kill();
                reason
            }
            None => self.exit_reason(),
        };
        {
            let mut state = locked(&self.state);
            state.closed = Some(reason.clone());
            for (_, waiter) in state.pending.drain() {
                let _ = waiter.send(Err(McpError::Closed(reason.clone())));
            }
        }
        let _ = self.out.try_send(Out::Close);
        if !self.leaving.load(Ordering::SeqCst) {
            let _ = self.events.send(ClientEvent::Exited(reason));
        }
    }

    fn dispatch(&self, message: &Value) {
        let id = message.get("id");
        let method = message.get("method").and_then(Value::as_str);
        match (id, method) {
            (Some(id), None) => self.answer(id, message),
            (Some(id), Some("ping")) => {
                self.reply(json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
            }
            // No capability was declared, so any other request is one the
            // server had no business sending.
            (Some(id), Some(_)) => {
                self.reply(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" },
                }));
            }
            // One event stands for any number of notices until the list has
            // been read again: a server that repeats it must not queue a
            // re-list for each.
            (None, Some("notifications/tools/list_changed")) => {
                if !self.tools_pending.swap(true, Ordering::SeqCst) {
                    let _ = self.events.send(ClientEvent::ToolsChanged);
                }
            }
            // Logging, progress and the rest are not acted on.
            (None, _) => {}
        }
    }

    /// Queues an answer to the server, unless too many are already waiting: a
    /// server that is not reading must not hold this thread or the queue.
    fn reply(&self, message: Value) {
        if self.replies.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUED_REPLIES
            || self.out.try_send(Out::Reply(message.to_string())).is_err()
        {
            self.replies.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn answer(&self, id: &Value, message: &Value) {
        let Some(id) = id.as_u64() else { return };
        let Some(waiter) = locked(&self.state).pending.remove(&id) else {
            // Answered after the caller gave up: nothing waits for it.
            return;
        };
        let answer = match (message.get("result"), message.get("error")) {
            (_, Some(error)) => Err(McpError::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
                    .to_owned(),
            }),
            (Some(result), None) => Ok(result.clone()),
            (None, None) => Err(McpError::Protocol(
                "a response with neither result nor error".to_owned(),
            )),
        };
        let _ = waiter.send(answer);
    }

    /// How the server ended, as far as the operating system will say.
    fn exit_reason(&self) -> String {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match locked(&self.child).try_wait() {
                Ok(Some(status)) => return format!("the server exited ({status})"),
                Ok(None) if Instant::now() < deadline => {}
                _ => return "the server closed its output".to_owned(),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Signals the group the server leads. Safe Rust has no `killpg`, and the
/// workspace forbids `unsafe`, so this asks `sh`, as `titi-tools` does for a
/// command.
#[cfg(unix)]
fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s {signal} -- -{pid} 2>/dev/null"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: &str) {}

/// The text of a `tools/call` result. Text blocks are kept; what a model
/// cannot be handed as text is named: `[image]`, `[audio]`, `[resource
/// <uri>]`. A result with no content but a `structuredContent` is that,
/// as JSON.
pub fn render_content(result: &Value) -> String {
    cap_result(render_blocks(result))
}

/// Characters of a rendered result kept, twice what the engine puts in a
/// message (`MAX_TOOL_OUTPUT`, which cuts to that and says so). The bound is
/// here so the masking pass over the text never sees more than this.
pub const MAX_RESULT_CHARS: usize = 80_000;

fn cap_result(text: String) -> String {
    let total = text.chars().count();
    if total <= MAX_RESULT_CHARS {
        return text;
    }
    let tail = MAX_RESULT_CHARS / 4;
    let head = MAX_RESULT_CHARS - tail;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(total - tail).collect();
    format!(
        "{start}\n… [{} characters left out by the MCP client: the server's answer was longer than {MAX_RESULT_CHARS}] …\n{end}",
        total - head - tail
    )
}

fn render_blocks(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in result
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let uri = |value: &Value| {
            value
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        parts.push(match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            Some("image") => "[image]".to_owned(),
            Some("audio") => "[audio]".to_owned(),
            Some("resource_link") => format!("[resource {}]", uri(block)),
            Some("resource") => format!(
                "[resource {}]",
                block.get("resource").map(uri).unwrap_or_default()
            ),
            Some(other) => format!("[{other}]"),
            None => continue,
        });
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        return structured.to_string();
    }
    parts.join("\n")
}
