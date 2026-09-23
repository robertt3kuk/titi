//! Local hub broker: JSON lines over a per-user Unix socket.
//!
//! One broker per agent directory listens on `<agent_dir>/hub.sock`. Agents
//! connect, `Join` under an id, and then exchange directed or broadcast
//! messages; presence (who is connected) is tracked by the broker and pushed
//! to the peers as `Joined` / `Left`.
//!
//! The socket is the trust boundary. It is local-only and created with mode
//! `0600`, so only the user running the agent can talk to it, and the broker
//! validates `from` against the id the connection joined under, so one client
//! cannot impersonate another.
//!
//! Spec: `docs/research/agents-hub-security/README.md` (hub messaging).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Socket file name inside the agent directory.
pub const SOCKET_NAME: &str = "hub.sock";
/// Owner-only socket: the hub is a local IPC channel, never shared.
const SOCKET_MODE: u32 = 0o600;
/// Same posture for an agent directory the broker has to create itself.
const AGENT_DIR_MODE: u32 = 0o700;
/// Longest accepted wire frame; a peer that exceeds it is disconnected.
const MAX_FRAME_BYTES: u64 = 1 << 20;
/// Longest accepted agent id.
const MAX_AGENT_ID: usize = 128;
/// Accept-loop poll interval, the granularity of `HubBroker::shutdown`.
const ACCEPT_POLL: Duration = Duration::from_millis(5);
/// How long `HubClient::connect` waits for the broker to acknowledge `Join`.
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors surfaced by the hub.
#[derive(Debug)]
pub enum HubError {
    Io(io::Error),
    Json(serde_json::Error),
    /// Another live broker already owns the socket.
    AlreadyRunning(PathBuf),
    /// The broker refused the operation (duplicate id, unknown peer, ...).
    Rejected(String),
    /// The socket closed under us.
    Disconnected,
    /// Nothing arrived before the deadline.
    Timeout,
    /// An agent id or a routing target failed validation.
    InvalidId(String),
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubError::Io(e) => write!(f, "hub io: {e}"),
            HubError::Json(e) => write!(f, "hub json: {e}"),
            HubError::AlreadyRunning(path) => {
                write!(f, "hub already running at {}", path.display())
            }
            HubError::Rejected(reason) => write!(f, "hub rejected: {reason}"),
            HubError::Disconnected => write!(f, "hub disconnected"),
            HubError::Timeout => write!(f, "hub timed out"),
            HubError::InvalidId(id) => write!(f, "invalid agent id: {id:?}"),
        }
    }
}

impl std::error::Error for HubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HubError::Io(e) => Some(e),
            HubError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for HubError {
    fn from(error: io::Error) -> Self {
        HubError::Io(error)
    }
}

impl From<serde_json::Error> for HubError {
    fn from(error: serde_json::Error) -> Self {
        HubError::Json(error)
    }
}

/// A frame a client sends to the broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HubRequest {
    /// Register this connection under `agent_id` and take a roster snapshot.
    Join { agent_id: String },
    /// Unregister and close this connection.
    Leave { agent_id: String },
    /// Deliver `message` to the peer registered as `to`.
    Send {
        from: String,
        to: String,
        message: String,
    },
    /// Deliver `message` to every peer except the sender.
    Broadcast { from: String, message: String },
}

/// A frame the broker pushes to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HubEvent {
    /// Answer to `Join`: the roster as it stands, including the joiner.
    Presence { agents: Vec<String> },
    /// Another peer joined.
    Joined { agent_id: String },
    /// Another peer left or dropped its connection.
    Left { agent_id: String },
    /// A delivered message; `to` is `None` for a broadcast.
    Message {
        from: String,
        to: Option<String>,
        message: String,
    },
    /// The previous request could not be served.
    Error { reason: String },
}

/// Rejects ids that would break framing, routing, or the roster display.
fn validate_id(id: &str) -> Result<String, HubError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_AGENT_ID {
        return Err(HubError::InvalidId(id.to_owned()));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(HubError::InvalidId(id.to_owned()));
    }
    Ok(trimmed.to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Reads one newline-delimited frame, refusing oversized ones.
fn read_frame(reader: &mut BufReader<UnixStream>, buf: &mut String) -> io::Result<usize> {
    buf.clear();
    let read = reader.by_ref().take(MAX_FRAME_BYTES).read_line(buf)?;
    if read as u64 == MAX_FRAME_BYTES && !buf.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hub frame exceeds the size limit",
        ));
    }
    Ok(read)
}

fn write_line<T: Serialize>(stream: &Mutex<UnixStream>, frame: &T) -> Result<(), HubError> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    let mut guard = lock(stream);
    guard.write_all(line.as_bytes())?;
    guard.flush()?;
    Ok(())
}

/// One connected socket. `agent_id` stays `None` until the client joins.
struct Client {
    agent_id: Option<String>,
    writer: Arc<Mutex<UnixStream>>,
}

#[derive(Default)]
struct Registry {
    clients: HashMap<u64, Client>,
}

impl Registry {
    fn roster(&self) -> Vec<String> {
        let mut agents: Vec<String> = self
            .clients
            .values()
            .filter_map(|client| client.agent_id.clone())
            .collect();
        agents.sort();
        agents
    }

    fn is_taken(&self, agent_id: &str) -> bool {
        self.clients
            .values()
            .any(|client| client.agent_id.as_deref() == Some(agent_id))
    }

    fn writer_for(&self, agent_id: &str) -> Option<Arc<Mutex<UnixStream>>> {
        self.clients
            .values()
            .find(|client| client.agent_id.as_deref() == Some(agent_id))
            .map(|client| Arc::clone(&client.writer))
    }

    /// Every joined peer except `except`, for presence and broadcast fan-out.
    fn peers_except(&self, except: u64) -> Vec<Arc<Mutex<UnixStream>>> {
        self.clients
            .iter()
            .filter(|(id, client)| **id != except && client.agent_id.is_some())
            .map(|(_, client)| Arc::clone(&client.writer))
            .collect()
    }
}

struct State {
    running: AtomicBool,
    next_conn: AtomicU64,
    registry: Mutex<Registry>,
}

impl State {
    fn fan_out(&self, except: u64, event: &HubEvent) {
        // Snapshot first: writing to a socket must never happen under the
        // registry lock, a slow peer would stall every other connection.
        let targets = lock(&self.registry).peers_except(except);
        for writer in targets {
            // A peer that died between the snapshot and the write is dropped
            // by its own reader thread; losing its copy is not an error here.
            let _ = write_line(&writer, event);
        }
    }
}

/// The broker: owns the listening socket and the connected-client registry.
pub struct HubBroker {
    path: PathBuf,
    state: Arc<State>,
    accept: Option<JoinHandle<()>>,
}

impl fmt::Debug for HubBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubBroker")
            .field("socket", &self.path)
            .field("agents", &self.agents())
            .finish()
    }
}

impl HubBroker {
    /// Binds `<agent_dir>/hub.sock` and starts serving.
    ///
    /// A leftover socket from a crashed broker is replaced; a socket that
    /// still answers means a live broker, and that is an error.
    pub fn bind(agent_dir: &Path) -> Result<Self, HubError> {
        if !agent_dir.exists() {
            fs::create_dir_all(agent_dir)?;
            fs::set_permissions(agent_dir, fs::Permissions::from_mode(AGENT_DIR_MODE))?;
        }
        let path = agent_dir.join(SOCKET_NAME);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    return Err(HubError::AlreadyRunning(path));
                }
                fs::remove_file(&path)?;
                UnixListener::bind(&path)?
            }
            Err(error) => return Err(HubError::Io(error)),
        };
        // Narrow the window between bind and chmod as much as a Unix socket
        // allows: no accept loop runs yet, so nobody is served meanwhile.
        fs::set_permissions(&path, fs::Permissions::from_mode(SOCKET_MODE))?;
        listener.set_nonblocking(true)?;

        let state = Arc::new(State {
            running: AtomicBool::new(true),
            next_conn: AtomicU64::new(1),
            registry: Mutex::new(Registry::default()),
        });
        let accept_state = Arc::clone(&state);
        let accept = thread::spawn(move || accept_loop(listener, accept_state));
        Ok(Self {
            path,
            state,
            accept: Some(accept),
        })
    }

    /// Path of the listening socket.
    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Ids of the currently joined agents, sorted.
    pub fn agents(&self) -> Vec<String> {
        lock(&self.state.registry).roster()
    }

    /// Stops the accept loop, drops every connection, removes the socket.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if !self.state.running.swap(false, Ordering::SeqCst) {
            return;
        }
        // Shutting the sockets down unblocks the connection threads, which
        // are parked in a blocking read.
        let live: Vec<Arc<Mutex<UnixStream>>> = lock(&self.state.registry)
            .clients
            .values()
            .map(|client| Arc::clone(&client.writer))
            .collect();
        for writer in live {
            let _ = lock(&writer).shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        lock(&self.state.registry).clients.clear();
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for HubBroker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Joins the local hub, starting the broker if this is the first session in.
///
/// First-joiner-hosts: sessions on one machine share one broker per agent
/// directory, but nothing hosts it until someone wants the hub. The first
/// `/join` binds the broker in-process and connects to it; every later
/// session finds the live socket and connects as a plain client.
///
/// `Some(broker)` means this call started the broker: the caller **must**
/// keep the handle alive for as long as it wants the hub to exist, and
/// dropping it removes the socket and disconnects the peers. `None` means a
/// peer was already hosting.
///
/// A join that races another first-joiner is resolved without an error: the
/// session that loses the bind ([`HubError::AlreadyRunning`]) connects to the
/// winner instead of reporting a failure the user did not cause.
pub fn join_or_host(
    agent_dir: &Path,
    agent_id: &str,
) -> Result<(HubClient, Option<HubBroker>), HubError> {
    match HubClient::connect(agent_dir, agent_id) {
        Ok(client) => Ok((client, None)),
        // No socket, or a dead one nobody is serving: this session hosts.
        Err(HubError::Io(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            match HubBroker::bind(agent_dir) {
                Ok(broker) => {
                    let client = HubClient::connect(agent_dir, agent_id)?;
                    Ok((client, Some(broker)))
                }
                // Another session bound first in the gap between our failed
                // connect and our bind: connect to it, host nothing.
                Err(HubError::AlreadyRunning(_)) => {
                    let client = HubClient::connect(agent_dir, agent_id)?;
                    Ok((client, None))
                }
                Err(other) => Err(other),
            }
        }
        Err(other) => Err(other),
    }
}

fn accept_loop(listener: UnixListener, state: Arc<State>) {
    let mut connections: Vec<JoinHandle<()>> = Vec::new();
    while state.running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // BSD sockets inherit O_NONBLOCK from the listener, and the
                // connection threads want blocking reads.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let conn = state.next_conn.fetch_add(1, Ordering::SeqCst);
                let conn_state = Arc::clone(&state);
                connections.push(thread::spawn(move || serve(conn, stream, conn_state)));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Idle moment: drop the handles of connections that ended, so
                // a long-lived broker does not accumulate them.
                connections.retain(|handle| !handle.is_finished());
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
    for handle in connections {
        let _ = handle.join();
    }
}

fn serve(conn: u64, stream: UnixStream, state: Arc<State>) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(write_half));
    lock(&state.registry).clients.insert(
        conn,
        Client {
            agent_id: None,
            writer: Arc::clone(&writer),
        },
    );

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while state.running.load(Ordering::SeqCst) {
        match read_frame(&mut reader, &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let frame = line.trim();
        if frame.is_empty() {
            continue;
        }
        match serde_json::from_str::<HubRequest>(frame) {
            Ok(request) => {
                if !handle_request(conn, &state, &writer, request) {
                    break;
                }
            }
            Err(error) => {
                let event = HubEvent::Error {
                    reason: format!("malformed frame: {error}"),
                };
                if write_line(&writer, &event).is_err() {
                    break;
                }
            }
        }
    }

    let departed = lock(&state.registry)
        .clients
        .remove(&conn)
        .and_then(|client| client.agent_id);
    if let Some(agent_id) = departed {
        state.fan_out(conn, &HubEvent::Left { agent_id });
    }
}

/// Serves one request. Returns `false` when the connection must close.
fn handle_request(
    conn: u64,
    state: &Arc<State>,
    writer: &Arc<Mutex<UnixStream>>,
    request: HubRequest,
) -> bool {
    let reject = |reason: String| {
        write_line(writer, &HubEvent::Error { reason }).is_ok() // keep serving
    };
    match request {
        HubRequest::Join { agent_id } => {
            let agent_id = match validate_id(&agent_id) {
                Ok(id) => id,
                Err(error) => return reject(error.to_string()),
            };
            let roster = {
                let mut registry = lock(&state.registry);
                if registry.is_taken(&agent_id) {
                    drop(registry);
                    return reject(format!("agent id already joined: {agent_id}"));
                }
                let Some(client) = registry.clients.get_mut(&conn) else {
                    return false;
                };
                client.agent_id = Some(agent_id.clone());
                registry.roster()
            };
            state.fan_out(
                conn,
                &HubEvent::Joined {
                    agent_id: agent_id.clone(),
                },
            );
            write_line(writer, &HubEvent::Presence { agents: roster }).is_ok()
        }
        HubRequest::Leave { agent_id } => {
            // The connection tears down in `serve`, which announces the exit.
            let joined = joined_id(state, conn);
            if joined.as_deref() != Some(agent_id.trim()) {
                return reject("leave: id does not match this connection".to_owned());
            }
            false
        }
        HubRequest::Send { from, to, message } => {
            if let Err(reason) = check_sender(state, conn, &from) {
                return reject(reason);
            }
            let to = match validate_id(&to) {
                Ok(id) => id,
                Err(error) => return reject(error.to_string()),
            };
            let Some(target) = lock(&state.registry).writer_for(&to) else {
                return reject(format!("unknown agent: {to}"));
            };
            let event = HubEvent::Message {
                from,
                to: Some(to),
                message,
            };
            let _ = write_line(&target, &event);
            true
        }
        HubRequest::Broadcast { from, message } => {
            if let Err(reason) = check_sender(state, conn, &from) {
                return reject(reason);
            }
            state.fan_out(
                conn,
                &HubEvent::Message {
                    from,
                    to: None,
                    message,
                },
            );
            true
        }
    }
}

fn joined_id(state: &Arc<State>, conn: u64) -> Option<String> {
    lock(&state.registry)
        .clients
        .get(&conn)
        .and_then(|client| client.agent_id.clone())
}

/// A client may only speak as the id it joined under: `from` is not a claim
/// the broker takes on trust.
fn check_sender(state: &Arc<State>, conn: u64, from: &str) -> Result<(), String> {
    match joined_id(state, conn) {
        None => Err("join before sending".to_owned()),
        Some(joined) if joined == from.trim() => Ok(()),
        Some(joined) => Err(format!("from must be {joined}")),
    }
}

/// A connected agent: writes requests on the calling thread, receives events
/// on a reader thread so nothing blocks the caller's loop.
pub struct HubClient {
    agent_id: String,
    writer: Mutex<UnixStream>,
    events: Receiver<HubEvent>,
    /// Events that arrived before the join was acknowledged; drained first so
    /// the caller still sees the broker's order.
    pending: Mutex<VecDeque<HubEvent>>,
    reader: Option<JoinHandle<()>>,
    peers: Vec<String>,
}

impl fmt::Debug for HubClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubClient")
            .field("agent_id", &self.agent_id)
            .field("peers", &self.peers)
            .finish()
    }
}

impl HubClient {
    /// Connects to `<agent_dir>/hub.sock` and joins as `agent_id`.
    ///
    /// Returns once the broker has acknowledged the join, so the roster in
    /// [`HubClient::peers`] and the registration are both already in place.
    pub fn connect(agent_dir: &Path, agent_id: &str) -> Result<Self, HubError> {
        let agent_id = validate_id(agent_id)?;
        let stream = UnixStream::connect(agent_dir.join(SOCKET_NAME))?;
        let read_half = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            loop {
                match read_frame(&mut reader, &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let frame = line.trim();
                if frame.is_empty() {
                    continue;
                }
                match serde_json::from_str::<HubEvent>(frame) {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        });

        let mut client = Self {
            agent_id: agent_id.clone(),
            writer: Mutex::new(stream),
            events: rx,
            pending: Mutex::new(VecDeque::new()),
            reader: Some(reader),
            peers: Vec::new(),
        };
        client.request(&HubRequest::Join { agent_id })?;
        // A peer joining at the same moment is announced to us before our own
        // acknowledgement arrives, so keep reading until the ack shows up. The
        // events read on the way are held aside, never fed back into the queue
        // this loop reads from, and handed to the caller in arrival order.
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let mut early = VecDeque::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(HubError::Timeout);
            }
            match client.recv_timeout(left)? {
                HubEvent::Presence { agents } => {
                    client.peers = agents;
                    *lock(&client.pending) = early;
                    return Ok(client);
                }
                HubEvent::Error { reason } => return Err(HubError::Rejected(reason)),
                other => early.push_back(other),
            }
        }
    }

    /// The id this client joined under.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The roster as it stood when the join was acknowledged.
    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    /// Sends `message` to the peer registered as `to`.
    pub fn send(&self, to: &str, message: &str) -> Result<(), HubError> {
        let to = validate_id(to)?;
        self.request(&HubRequest::Send {
            from: self.agent_id.clone(),
            to,
            message: message.to_owned(),
        })
    }

    /// Sends `message` to every other joined peer.
    pub fn broadcast(&self, message: &str) -> Result<(), HubError> {
        self.request(&HubRequest::Broadcast {
            from: self.agent_id.clone(),
            message: message.to_owned(),
        })
    }

    /// Takes a queued event without blocking. `None` means nothing pending.
    pub fn try_recv(&self) -> Result<Option<HubEvent>, HubError> {
        if let Some(event) = lock(&self.pending).pop_front() {
            return Ok(Some(event));
        }
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(HubError::Disconnected),
        }
    }

    /// Waits up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<HubEvent, HubError> {
        if let Some(event) = lock(&self.pending).pop_front() {
            return Ok(event);
        }
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(event),
            Err(RecvTimeoutError::Timeout) => Err(HubError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(HubError::Disconnected),
        }
    }

    fn request(&self, request: &HubRequest) -> Result<(), HubError> {
        write_line(&self.writer, request)
    }
}

impl Drop for HubClient {
    fn drop(&mut self) {
        let _ = self.request(&HubRequest::Leave {
            agent_id: self.agent_id.clone(),
        });
        let _ = lock(&self.writer).shutdown(Shutdown::Both);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn broker() -> (tempfile::TempDir, HubBroker) {
        let dir = tempfile::tempdir().expect("a temp agent dir");
        let broker = HubBroker::bind(dir.path()).expect("the broker binds");
        (dir, broker)
    }

    /// Waits for the first event matching `pick`, skipping presence noise.
    fn wait_for<T>(client: &HubClient, pick: impl Fn(&HubEvent) -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            let event = client.recv_timeout(WAIT).expect("an event before the wait");
            if let Some(found) = pick(&event) {
                return found;
            }
        }
        panic!("no matching event within {WAIT:?}");
    }

    /// Several agents joining at once: the roster snapshot each one gets plus
    /// the announcements that follow must add up to the full set, however the
    /// joins interleave.
    #[test]
    fn concurrent_joins_all_land_with_a_complete_view() {
        use std::collections::HashSet;

        let (dir, _broker) = broker();
        let ids = ["a", "b", "c", "d"];
        let threads: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let path = dir.path().to_path_buf();
                thread::spawn(move || {
                    let client = HubClient::connect(&path, id).expect("the join is acknowledged");
                    let mut seen: HashSet<String> = client.peers().iter().cloned().collect();
                    let deadline = Instant::now() + WAIT;
                    while seen.len() < 4 && Instant::now() < deadline {
                        match client.recv_timeout(WAIT) {
                            Ok(HubEvent::Joined { agent_id }) => {
                                seen.insert(agent_id);
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    (id, seen)
                })
            })
            .collect();

        for thread in threads {
            let (id, seen) = thread.join().expect("the client thread finishes");
            assert_eq!(seen.len(), ids.len(), "{id} lost a peer: {seen:?}");
        }
    }

    #[test]
    fn join_tracks_presence_on_both_sides() {
        let (dir, broker) = broker();
        let alice = HubClient::connect(dir.path(), "alice").expect("alice joins");
        assert_eq!(alice.peers(), ["alice".to_owned()]);

        let bob = HubClient::connect(dir.path(), "bob").expect("bob joins");
        assert_eq!(bob.peers(), ["alice".to_owned(), "bob".to_owned()]);
        assert_eq!(broker.agents(), vec!["alice".to_owned(), "bob".to_owned()]);

        let joined = wait_for(&alice, |event| match event {
            HubEvent::Joined { agent_id } => Some(agent_id.clone()),
            _ => None,
        });
        assert_eq!(joined, "bob");

        drop(bob);
        let left = wait_for(&alice, |event| match event {
            HubEvent::Left { agent_id } => Some(agent_id.clone()),
            _ => None,
        });
        assert_eq!(left, "bob");
        assert_eq!(broker.agents(), vec!["alice".to_owned()]);
    }

    #[test]
    fn send_routes_only_to_the_named_peer() {
        let (dir, _broker) = broker();
        let alice = HubClient::connect(dir.path(), "alice").expect("alice joins");
        let bob = HubClient::connect(dir.path(), "bob").expect("bob joins");
        let carol = HubClient::connect(dir.path(), "carol").expect("carol joins");

        alice
            .send("bob", "rebase first")
            .expect("the send is accepted");

        let delivered = wait_for(&bob, |event| match event {
            HubEvent::Message { from, to, message } => {
                Some((from.clone(), to.clone(), message.clone()))
            }
            _ => None,
        });
        assert_eq!(
            delivered,
            (
                "alice".to_owned(),
                Some("bob".to_owned()),
                "rebase first".to_owned()
            )
        );

        // Carol only ever sees presence traffic, never a directed message.
        while let Some(event) = carol.try_recv().expect("carol stays connected") {
            assert!(
                !matches!(event, HubEvent::Message { .. }),
                "directed message leaked to a third peer: {event:?}"
            );
        }
    }

    #[test]
    fn broadcast_reaches_every_peer_but_the_sender() {
        let (dir, _broker) = broker();
        let alice = HubClient::connect(dir.path(), "alice").expect("alice joins");
        let bob = HubClient::connect(dir.path(), "bob").expect("bob joins");
        let carol = HubClient::connect(dir.path(), "carol").expect("carol joins");

        alice
            .broadcast("ci is red")
            .expect("the broadcast is accepted");

        for peer in [&bob, &carol] {
            let delivered = wait_for(peer, |event| match event {
                HubEvent::Message { from, to, message } => {
                    Some((from.clone(), to.clone(), message.clone()))
                }
                _ => None,
            });
            assert_eq!(
                delivered,
                ("alice".to_owned(), None, "ci is red".to_owned())
            );
        }

        while let Some(event) = alice.try_recv().expect("alice stays connected") {
            assert!(
                !matches!(event, HubEvent::Message { .. }),
                "the sender got its own broadcast back: {event:?}"
            );
        }
    }

    #[test]
    fn socket_is_owner_only() {
        let (_dir, broker) = broker();
        let mode = fs::metadata(broker.socket_path())
            .expect("the socket exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, SOCKET_MODE, "hub.sock must be 0600");
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let (dir, _broker) = broker();
        let _alice = HubClient::connect(dir.path(), "alice").expect("alice joins");
        let error = HubClient::connect(dir.path(), "alice").expect_err("the second alice is out");
        assert!(
            matches!(&error, HubError::Rejected(reason) if reason.contains("already joined")),
            "{error}"
        );
    }

    #[test]
    fn send_to_unknown_peer_reports_an_error() {
        let (dir, _broker) = broker();
        let alice = HubClient::connect(dir.path(), "alice").expect("alice joins");
        alice.send("ghost", "hello").expect("the frame is written");
        let reason = wait_for(&alice, |event| match event {
            HubEvent::Error { reason } => Some(reason.clone()),
            _ => None,
        });
        assert!(reason.contains("unknown agent: ghost"), "{reason}");
    }

    #[test]
    fn a_client_cannot_speak_as_another_agent() {
        let (dir, _broker) = broker();
        let alice = HubClient::connect(dir.path(), "alice").expect("alice joins");
        let bob = HubClient::connect(dir.path(), "bob").expect("bob joins");

        // Bypass `HubClient::send`, which always stamps the real id.
        alice
            .request(&HubRequest::Send {
                from: "bob".to_owned(),
                to: "bob".to_owned(),
                message: "spoofed".to_owned(),
            })
            .expect("the frame is written");

        let reason = wait_for(&alice, |event| match event {
            HubEvent::Error { reason } => Some(reason.clone()),
            _ => None,
        });
        assert_eq!(reason, "from must be alice");
        assert!(
            bob.try_recv().expect("bob stays connected").is_none(),
            "the spoofed message must not be delivered"
        );
    }

    #[test]
    fn empty_agent_id_is_refused_before_connecting() {
        let (dir, _broker) = broker();
        let error = HubClient::connect(dir.path(), "  ").expect_err("an empty id is invalid");
        assert!(matches!(error, HubError::InvalidId(_)), "{error}");
    }

    #[test]
    fn a_stale_socket_is_replaced_but_a_live_one_is_not() {
        let dir = tempfile::tempdir().expect("a temp agent dir");
        let first = HubBroker::bind(dir.path()).expect("the first broker binds");
        let error = HubBroker::bind(dir.path()).expect_err("a live socket is not stolen");
        assert!(matches!(error, HubError::AlreadyRunning(_)), "{error}");

        first.shutdown();
        // A crash leaves the file behind; the next broker must reclaim it.
        fs::write(dir.path().join(SOCKET_NAME), "").expect("a leftover file");
        let _reclaimed = HubBroker::bind(dir.path()).expect("a stale socket is replaced");
    }

    /// Waits until `client`'s roster holds every id in `want`, folding in the
    /// `Joined` announcements that arrive after the initial snapshot.
    fn roster_reaches(client: &HubClient, want: &[&str]) -> Vec<String> {
        use std::collections::BTreeSet;
        let mut seen: BTreeSet<String> = client.peers().iter().cloned().collect();
        let deadline = Instant::now() + WAIT;
        while !want.iter().all(|id| seen.contains(*id)) && Instant::now() < deadline {
            match client.recv_timeout(WAIT) {
                Ok(HubEvent::Joined { agent_id }) => {
                    seen.insert(agent_id);
                }
                Ok(HubEvent::Presence { agents }) => seen.extend(agents),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        seen.into_iter().collect()
    }

    /// First-joiner-hosts: the first `join_or_host` with no socket starts the
    /// broker and joins it; a second session finds that broker and joins as a
    /// plain client; dropping the host tears the socket down under the peer.
    #[test]
    fn join_or_host_starts_a_broker_then_a_second_session_joins_it() {
        let dir = tempfile::tempdir().expect("a temp agent dir");

        let (main, broker) = join_or_host(dir.path(), "main").expect("the first session hosts");
        let broker = broker.expect("the first joiner starts the broker");
        assert_eq!(main.peers(), ["main"]);

        let (scout, none) = join_or_host(dir.path(), "scout").expect("the second session joins");
        assert!(none.is_none(), "the second joiner must not host a broker");

        assert_eq!(roster_reaches(&main, &["main", "scout"]), ["main", "scout"]);
        assert_eq!(
            roster_reaches(&scout, &["main", "scout"]),
            ["main", "scout"]
        );

        // The host leaves: dropping its client and broker removes the socket,
        // and the peer's reader ends rather than hanging.
        drop(main);
        drop(broker);
        let disconnected = {
            let deadline = Instant::now() + WAIT;
            let mut ended = false;
            while Instant::now() < deadline {
                match scout.recv_timeout(WAIT) {
                    Ok(_) => {}
                    Err(_) => {
                        ended = true;
                        break;
                    }
                }
            }
            ended
        };
        assert!(disconnected, "the peer must see the host's broker go away");
    }
}
