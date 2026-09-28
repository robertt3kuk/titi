//! Loopback callback server and the manual-paste parser.
//!
//! omp's `callback-server.ts` is the reference: a `TcpListener` on the
//! descriptor's host/port, the descriptor's path reading
//! `code`/`state`/`error`/`error_description`, everything else 404, a free
//! port only when the rule allows it, and a 300-second wait.
//!
//! A descriptor names a host, not an address family: `localhost` has a
//! loopback in each family, the browser picks one of them, and it is not
//! knowable which. Every family the host names therefore listens on the same
//! port, and any of them answers the same single login.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::oauth::OAuthError;
use crate::oauth::encode;
use crate::oauth::provider::CallbackSpec;

/// How long the callback may take before the login is abandoned.
pub const CALLBACK_TIMEOUT_SECS: u64 = 300;

/// How long one socket wait lasts before the manual-paste path is polled
/// again; the TUI polls it once per rendered frame.
const POLL_SLICE: Duration = Duration::from_millis(100);

/// Largest request head accepted; a callback URL never comes close.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// How many ephemeral ports one fallback may try before giving up: each
/// attempt has to fit every loopback family on the same free port.
const EPHEMERAL_ATTEMPTS: usize = 5;

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>titi</title></head><body><h1>Authorization complete</h1>\
<p>You can close this tab and return to titi.</p></body></html>";

/// A bound loopback listener waiting for exactly one provider redirect.
#[derive(Debug)]
pub struct CallbackServer {
    /// One listener per loopback family the descriptor's host names. Any of
    /// them carries the same login; the first answer retires the rest.
    listeners: Vec<TcpListener>,
    local: SocketAddr,
    path: &'static str,
    state: String,
    redirect_uri: String,
}

impl CallbackServer {
    /// Binds `spec` and reports the redirect URI the browser must be sent to
    /// (the real ephemeral port when the descriptor allowed a fallback).
    pub async fn bind(spec: &CallbackSpec, state: &str) -> Result<Self, OAuthError> {
        // A pinned redirect URI names an exact URI the provider allowlists, so
        // neither an ephemeral port nor a fallback is ever permissible there.
        let may_fallback = spec.port_fallback && spec.redirect_uri.is_none();
        let addrs = callback_addrs(spec.host, spec.port).await?;
        let (listeners, local) = match bind_on(&addrs, spec.port).await? {
            Attempt::Bound { listeners, local } => (listeners, local),
            // A busy port is not this login's port. When the descriptor allows
            // it, the families move together: a family left behind on another
            // port would answer a redirect the browser was never sent.
            Attempt::Busy(busy) if may_fallback => {
                let mut last = busy;
                let mut moved = None;
                for _ in 0..EPHEMERAL_ATTEMPTS {
                    match bind_on(&addrs, 0).await? {
                        Attempt::Bound { listeners, local } => {
                            moved = Some((listeners, local));
                            break;
                        }
                        Attempt::Busy(busy) => last = busy,
                    }
                }
                moved.ok_or_else(|| OAuthError::Config {
                    message: format!(
                        "cannot bind the oauth callback ({last}); ephemeral fallback also failed"
                    ),
                })?
            }
            Attempt::Busy(busy) => {
                return Err(OAuthError::Config {
                    message: format!("oauth callback port {} is not available: {busy}", spec.port),
                });
            }
        };
        let redirect_uri = match spec.redirect_uri {
            Some(uri) => uri.to_owned(),
            None => format!("http://{}:{}{}", spec.host, local.port(), spec.path),
        };
        Ok(Self {
            listeners,
            local,
            path: spec.path,
            state: state.to_owned(),
            redirect_uri,
        })
    }

    /// The redirect URI to hand to the provider (and the user).
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The address actually bound; the ephemeral port lives here.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Waits for the code, alternating a short socket wait with a poll of the
    /// manual-paste path so a TUI can service both without blocking.
    pub async fn wait_for_code(
        &mut self,
        mut manual: impl FnMut() -> Option<String>,
    ) -> Result<String, OAuthError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(CALLBACK_TIMEOUT_SECS);
        loop {
            if let Some(input) = manual()
                && let Some(code) = self.manual_code(&input)
            {
                return Ok(code);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(OAuthError::Timeout {
                    seconds: CALLBACK_TIMEOUT_SECS,
                });
            }
            let slice = POLL_SLICE.min(deadline - now);
            // Whichever family the browser picked, one login is being served:
            // the first listener with a connection answers it.
            let accepted = match self.listeners.as_mut_slice() {
                [one] => tokio::select! {
                    biased;
                    accepted = one.accept() => Some(accepted),
                    () = tokio::time::sleep(slice) => None,
                },
                [one, two] => tokio::select! {
                    biased;
                    accepted = one.accept() => Some(accepted),
                    accepted = two.accept() => Some(accepted),
                    () = tokio::time::sleep(slice) => None,
                },
                // `bind` leaves one listener per family of the host, and a
                // server that answered holds none.
                [] => {
                    return Err(OAuthError::Server {
                        message: "the oauth callback is already spent".to_owned(),
                    });
                }
                _ => {
                    return Err(OAuthError::Server {
                        message: "more oauth callback listeners than loopback families".to_owned(),
                    });
                }
            };
            let Some(accepted) = accepted else { continue };
            let (mut stream, _) = accepted.map_err(|e| OAuthError::Server {
                message: format!("oauth callback accept failed: {e}"),
            })?;
            // A connection that stalls must not outlive the deadline either.
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(
                remaining,
                handle_connection(&mut stream, self.path, &self.state),
            )
            .await
            {
                Ok(Ok(Some(code))) => {
                    // The login is this one redirect: the family that did not
                    // answer stops with it.
                    self.retire();
                    return Ok(code);
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    self.retire();
                    return Err(error);
                }
                Err(_) => {
                    return Err(OAuthError::Timeout {
                        seconds: CALLBACK_TIMEOUT_SECS,
                    });
                }
            }
        }
    }

    /// Closes every listener: this server carried its one redirect.
    fn retire(&mut self) {
        self.listeners.clear();
    }

    /// Interprets pasted text: a bare code, `code#state`, a query string or a
    /// redirect URL. Text carrying a different `state` is not the answer to
    /// this login and is ignored so the caller can prompt again.
    fn manual_code(&self, input: &str) -> Option<String> {
        let (code, state) = parse_callback_input(input);
        let code = code?;
        match state {
            Some(state) if state != self.state => None,
            _ => Some(code),
        }
    }
}

/// What one attempt at binding every address on a single port produced.
enum Attempt {
    Bound {
        listeners: Vec<TcpListener>,
        local: SocketAddr,
    },
    /// The port is taken; the caller decides between a fallback and a
    /// configuration error.
    Busy(io::Error),
}

/// The addresses `host` names, in resolution order and without repeats.
///
/// A callback serves one address per family — what a browser can pick between
/// for a name like `localhost`. Anything wider would need a multiplexer this
/// server does not have, so it is refused by name rather than half-listened
/// to.
async fn callback_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, OAuthError> {
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| OAuthError::Config {
            message: format!("cannot resolve the oauth callback host {host}: {error}"),
        })?;
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for addr in resolved {
        if !addrs.iter().any(|seen| seen.ip() == addr.ip()) {
            addrs.push(addr);
        }
    }
    if addrs.is_empty() {
        return Err(OAuthError::Config {
            message: format!("oauth callback host {host} resolves to no address"),
        });
    }
    if addrs.len() > 2 {
        return Err(OAuthError::Config {
            message: format!(
                "oauth callback host {host} resolves to {} addresses; a callback serves one per family",
                addrs.len()
            ),
        });
    }
    Ok(addrs)
}

/// Binds every address on one port.
///
/// The first address that binds decides the port, so the families that follow
/// land beside it: a request for port `0` means one free port for all of them
/// rather than a port each.
async fn bind_on(addrs: &[SocketAddr], port: u16) -> Result<Attempt, OAuthError> {
    let mut listeners: Vec<TcpListener> = Vec::with_capacity(addrs.len());
    let mut local: Option<SocketAddr> = None;
    let mut chosen = port;
    for addr in addrs {
        let target = SocketAddr::new(addr.ip(), chosen);
        match TcpListener::bind(target).await {
            Ok(listener) => {
                let bound = listener.local_addr().map_err(|error| OAuthError::Server {
                    message: format!("cannot read the oauth callback address: {error}"),
                })?;
                if local.is_none() {
                    local = Some(bound);
                    chosen = bound.port();
                }
                listeners.push(listener);
            }
            // A busy port belongs to the caller's fallback rule.
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                return Ok(Attempt::Busy(error));
            }
            // A machine with only one loopback family, or a name that also
            // answers with an address this host does not have, is ordinary:
            // the families that do bind carry the redirect. No surface hears
            // about it — an absent IPv6 is not a fault to warn about.
            Err(error) if address_this_host_does_not_have(&error) => continue,
            Err(error) => {
                return Err(OAuthError::Config {
                    message: format!("cannot bind the oauth callback on {target}: {error}"),
                });
            }
        }
    }
    let Some(local) = local else {
        return Err(OAuthError::Config {
            message: format!("no oauth callback address of {addrs:?} could be bound"),
        });
    };
    Ok(Attempt::Bound { listeners, local })
}

/// An address this machine does not have — no IPv6, or a name answering with
/// a foreign address — which the callback skips. Everything else is a real
/// failure, not a family to step over.
fn address_this_host_does_not_have(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
    )
}

async fn handle_connection(
    stream: &mut TcpStream,
    path: &str,
    state: &str,
) -> Result<Option<String>, OAuthError> {
    let head = read_head(stream).await?;
    let Some(target) = request_target(&head) else {
        write_response(stream, 400, "Bad Request", &error_page("Malformed request")).await?;
        return Ok(None);
    };
    let (request_path, query) = match target.split_once('?') {
        Some((request_path, query)) => (request_path, query),
        None => (target.as_str(), ""),
    };
    if request_path != path {
        write_response(
            stream,
            404,
            "Not Found",
            &error_page("Unknown callback path"),
        )
        .await?;
        return Ok(None);
    }
    let params = encode::parse_query(query);
    if let Some(error) = param(&params, "error") {
        let description = param(&params, "error_description").unwrap_or_default();
        let message = if description.is_empty() {
            error
        } else {
            format!("{error}: {description}")
        };
        write_response(stream, 500, "Internal Server Error", &error_page(&message)).await?;
        return Err(OAuthError::Denied { message });
    }
    let Some(code) = param(&params, "code") else {
        write_response(
            stream,
            500,
            "Internal Server Error",
            &error_page("The provider did not return an authorization code"),
        )
        .await?;
        return Err(OAuthError::MissingCode);
    };
    if param(&params, "state").as_deref() != Some(state) {
        write_response(
            stream,
            500,
            "Internal Server Error",
            &error_page("State mismatch - possible CSRF attack"),
        )
        .await?;
        return Err(OAuthError::StateMismatch);
    }
    write_response(stream, 200, "OK", SUCCESS_PAGE).await?;
    Ok(Some(code))
}

async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, OAuthError> {
    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() >= MAX_HEAD_BYTES {
            return Ok(buffer);
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| OAuthError::Server {
                message: format!("oauth callback read failed: {e}"),
            })?;
        if read == 0 {
            return Ok(buffer);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn request_target(head: &[u8]) -> Option<String> {
    let head = std::str::from_utf8(head).ok()?;
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    parts.next().map(str::to_owned)
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), OAuthError> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| OAuthError::Server {
            message: format!("oauth callback write failed: {e}"),
        })?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn error_page(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>titi</title></head>\
         <body><h1>Authorization failed</h1><p>{}</p><p>You can close this tab.</p></body></html>",
        escape_html(message)
    )
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

fn param(params: &[(String, String)], key: &str) -> Option<String> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
}

/// Splits pasted login text into `(code, state)`. Accepts a full redirect
/// URL, a raw query string, a bare code and `code#state`; text that carries
/// only an `error` yields no code, which the caller reads as "try again".
pub fn parse_callback_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Some((_, rest)) = value.split_once("://") {
        let query = rest.split_once('?').map(|(_, query)| query).unwrap_or("");
        let params = encode::parse_query(query);
        return (param(&params, "code"), param(&params, "state"));
    }
    if value.contains("code=") {
        let stripped = value.trim_start_matches(['?', '#']);
        let params = encode::parse_query(stripped);
        return (param(&params, "code"), param(&params, "state"));
    }
    match value.split_once('#') {
        Some((code, state)) => (non_empty(code), non_empty(state)),
        None => (non_empty(value), None),
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn test_spec(port: u16, port_fallback: bool) -> CallbackSpec {
        CallbackSpec {
            host: "127.0.0.1",
            port,
            path: "/callback",
            redirect_uri: None,
            port_fallback,
        }
    }

    /// The host both real descriptors name: a name with a loopback in each
    /// family, which is the shape a browser may reach either way.
    fn localhost_spec(port: u16, port_fallback: bool) -> CallbackSpec {
        CallbackSpec {
            host: "localhost",
            port,
            path: "/callback",
            redirect_uri: None,
            port_fallback,
        }
    }

    /// Whether this host can bind the IPv6 loopback at all. Every IPv6
    /// assertion is skipped where it cannot: one family is a normal machine,
    /// and the suite must not fail on one.
    fn ipv6_loopback_available() -> bool {
        std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
    }

    async fn try_get(addr: SocketAddr, target: &str) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(addr).await?;
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok(String::from_utf8_lossy(&response).into_owned())
    }

    async fn get(addr: SocketAddr, target: &str) -> String {
        try_get(addr, target).await.expect("callback request")
    }

    fn status_line(response: &str) -> String {
        response.lines().next().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn a_matching_callback_returns_the_code_and_answers_200() {
        let mut server = CallbackServer::bind(&test_spec(0, true), "st-1")
            .await
            .expect("bind");
        let addr = server.local_addr();
        assert_eq!(
            server.redirect_uri(),
            format!("http://127.0.0.1:{}/callback", addr.port())
        );
        let client =
            tokio::spawn(async move { get(addr, "/callback?code=sk-test&state=st-1").await });
        let code = server.wait_for_code(|| None).await.expect("code");
        assert_eq!(code, "sk-test");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 200"),
            "{response}"
        );
        assert!(response.contains("close this tab"), "{response}");
    }

    #[tokio::test]
    async fn a_mismatched_state_is_a_typed_failure() {
        let mut server = CallbackServer::bind(&test_spec(0, true), "st-1")
            .await
            .expect("bind");
        let addr = server.local_addr();
        let client =
            tokio::spawn(async move { get(addr, "/callback?code=sk-test&state=st-2").await });
        let error = server.wait_for_code(|| None).await.expect_err("refused");
        assert!(matches!(error, OAuthError::StateMismatch), "{error:?}");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 500"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn a_provider_error_is_refused_with_its_text() {
        let mut server = CallbackServer::bind(&test_spec(0, true), "st-1")
            .await
            .expect("bind");
        let addr = server.local_addr();
        let client = tokio::spawn(async move {
            get(
                addr,
                "/callback?error=access_denied&error_description=user%20said%20no",
            )
            .await
        });
        let error = server.wait_for_code(|| None).await.expect_err("denied");
        let OAuthError::Denied { message } = &error else {
            panic!("expected denial, got {error:?}");
        };
        assert!(message.contains("access_denied"), "{message}");
        assert!(message.contains("user said no"), "{message}");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 500"),
            "{response}"
        );
        assert!(response.contains("user said no"), "{response}");
    }

    #[tokio::test]
    async fn another_path_is_404_and_the_server_keeps_waiting() {
        let mut server = CallbackServer::bind(&test_spec(0, true), "st-1")
            .await
            .expect("bind");
        let addr = server.local_addr();
        let client = tokio::spawn(async move {
            let first = get(addr, "/favicon.ico").await;
            let second = get(addr, "/callback?code=sk-test&state=st-1").await;
            (first, second)
        });
        let code = server.wait_for_code(|| None).await.expect("code");
        assert_eq!(code, "sk-test");
        let (first, second) = client.await.expect("client");
        assert!(status_line(&first).starts_with("HTTP/1.1 404"), "{first}");
        assert!(status_line(&second).starts_with("HTTP/1.1 200"), "{second}");
    }

    #[tokio::test]
    async fn a_missing_code_is_refused() {
        let mut server = CallbackServer::bind(&test_spec(0, true), "st-1")
            .await
            .expect("bind");
        let addr = server.local_addr();
        let client = tokio::spawn(async move { get(addr, "/callback?state=st-1").await });
        let error = server.wait_for_code(|| None).await.expect_err("refused");
        assert!(matches!(error, OAuthError::MissingCode), "{error:?}");
        let _ = client.await;
    }

    #[tokio::test]
    async fn a_busy_port_without_fallback_is_a_configuration_error() {
        let busy = TcpListener::bind(("127.0.0.1", 0)).await.expect("busy");
        let port = busy.local_addr().expect("addr").port();
        let error = CallbackServer::bind(&test_spec(port, false), "st-1")
            .await
            .expect_err("must refuse");
        assert!(matches!(error, OAuthError::Config { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_busy_port_with_fallback_binds_an_ephemeral_one() {
        let busy = TcpListener::bind(("127.0.0.1", 0)).await.expect("busy");
        let port = busy.local_addr().expect("addr").port();
        let server = CallbackServer::bind(&test_spec(port, true), "st-1")
            .await
            .expect("fallback");
        assert_ne!(server.local_addr().port(), port);
    }

    /// A browser resolves the descriptor's host itself, so the redirect may
    /// arrive on either loopback; both answer the same login.
    #[tokio::test]
    async fn both_loopback_families_answer_a_matching_callback() {
        let mut server = CallbackServer::bind(&localhost_spec(0, true), "st-1")
            .await
            .expect("bind");
        let port = server.local_addr().port();
        assert_eq!(
            server.redirect_uri(),
            format!("http://localhost:{port}/callback")
        );
        let client = tokio::spawn(async move {
            get(
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                "/callback?code=sk-test-v4&state=st-1",
            )
            .await
        });
        let code = server.wait_for_code(|| None).await.expect("code");
        assert_eq!(code, "sk-test-v4");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 200"),
            "{response}"
        );

        if !ipv6_loopback_available() {
            return;
        }
        let mut server = CallbackServer::bind(&localhost_spec(0, true), "st-2")
            .await
            .expect("bind");
        let port = server.local_addr().port();
        let client = tokio::spawn(async move {
            get(
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
                "/callback?code=sk-test-v6&state=st-2",
            )
            .await
        });
        let code = server.wait_for_code(|| None).await.expect("code");
        assert_eq!(code, "sk-test-v6");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 200"),
            "{response}"
        );
    }

    /// A wrong state is the same typed failure wherever it arrives.
    #[tokio::test]
    async fn a_wrong_state_on_the_ipv6_listener_is_the_same_typed_failure() {
        if !ipv6_loopback_available() {
            return;
        }
        let mut server = CallbackServer::bind(&localhost_spec(0, true), "st-1")
            .await
            .expect("bind");
        let addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), server.local_addr().port());
        let client =
            tokio::spawn(async move { get(addr, "/callback?code=sk-test&state=st-2").await });
        let error = server.wait_for_code(|| None).await.expect_err("refused");
        assert!(matches!(error, OAuthError::StateMismatch), "{error:?}");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 500"),
            "{response}"
        );
    }

    /// A name may answer with an address this host does not have; the family
    /// that is here still completes the callback. This is the shape of a
    /// machine with no IPv6, where the other family is skipped in silence.
    #[tokio::test]
    async fn an_address_this_host_does_not_have_is_skipped() {
        let foreign = SocketAddr::new(IpAddr::from([203, 0, 113, 7]), 0);
        let here = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        let Attempt::Bound { listeners, local } = bind_on(&[foreign, here], 0).await.expect("bind")
        else {
            panic!("the address this host has should have bound");
        };
        assert_eq!(listeners.len(), 1, "only the address this host has listens");
        assert!(local.ip().is_loopback(), "{local}");
        let client =
            tokio::spawn(async move { get(local, "/callback?code=sk-test-here&state=st-1").await });
        let (mut stream, _) = listeners[0].accept().await.expect("accept");
        let code = handle_connection(&mut stream, "/callback", "st-1")
            .await
            .expect("handled");
        assert_eq!(code.as_deref(), Some("sk-test-here"));
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 200"),
            "{response}"
        );
    }

    /// The pinned Codex URI: a port busy on either family stays a
    /// configuration error, never an ephemeral port.
    #[tokio::test]
    async fn a_busy_loopback_port_without_fallback_is_a_configuration_error() {
        let busy = TcpListener::bind(("127.0.0.1", 0)).await.expect("busy");
        let port = busy.local_addr().expect("addr").port();
        let error = CallbackServer::bind(&localhost_spec(port, false), "st-1")
            .await
            .expect_err("must refuse");
        assert!(matches!(error, OAuthError::Config { .. }), "{error:?}");
    }

    /// The fallback moves every family to one free port: a family left on a
    /// port of its own would answer a redirect nobody was sent.
    #[tokio::test]
    async fn a_fallback_puts_every_loopback_family_on_one_port() {
        let busy = TcpListener::bind(("127.0.0.1", 0)).await.expect("busy");
        let busy_port = busy.local_addr().expect("addr").port();
        let mut server = CallbackServer::bind(&localhost_spec(busy_port, true), "st-1")
            .await
            .expect("fallback");
        let port = server.local_addr().port();
        assert_ne!(port, busy_port, "the fallback must leave the busy port");
        assert_eq!(
            server.redirect_uri(),
            format!("http://localhost:{port}/callback")
        );
        let ports: Vec<u16> = server
            .listeners
            .iter()
            .map(|listener| listener.local_addr().expect("addr").port())
            .collect();
        assert!(
            ports.iter().all(|seen| *seen == port),
            "one port for every family: {ports:?}"
        );
        let client = tokio::spawn(async move {
            get(
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                "/callback?code=sk-test-fallback&state=st-1",
            )
            .await
        });
        let code = server.wait_for_code(|| None).await.expect("code");
        assert_eq!(code, "sk-test-fallback");
        let response = client.await.expect("client");
        assert!(
            status_line(&response).starts_with("HTTP/1.1 200"),
            "{response}"
        );
    }

    /// One login, however many families listen: the redirect that arrived
    /// ends the server, so the family it did not arrive on stops with it.
    #[tokio::test]
    async fn the_listener_that_did_not_answer_stops() {
        let mut server = CallbackServer::bind(&localhost_spec(0, true), "st-1")
            .await
            .expect("bind");
        let port = server.local_addr().port();
        let client = tokio::spawn(async move {
            get(
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                "/callback?code=sk-test-first&state=st-1",
            )
            .await
        });
        let code = server.wait_for_code(|| None).await.expect("code");
        assert_eq!(code, "sk-test-first");
        let _ = client.await;
        if !ipv6_loopback_available() {
            return;
        }
        let late = tokio::time::timeout(
            Duration::from_secs(5),
            try_get(
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
                "/callback?code=sk-test-late&state=st-1",
            ),
        )
        .await;
        assert!(matches!(late, Ok(Err(_))), "still answered: {late:?}");
    }

    /// The manual path is polled beside both listeners, not instead of them.
    #[tokio::test]
    async fn the_manual_paste_path_still_wins_with_two_listeners() {
        let mut server = CallbackServer::bind(&localhost_spec(0, true), "st-1")
            .await
            .expect("bind");
        let mut polls = 0;
        let code = server
            .wait_for_code(|| {
                polls += 1;
                (polls >= 3).then(|| "sk-test-pasted".to_owned())
            })
            .await
            .expect("code");
        assert_eq!(code, "sk-test-pasted");
        assert!(polls >= 3);
    }

    #[test]
    fn callback_input_parses_every_pasted_shape() {
        assert_eq!(
            parse_callback_input("http://localhost:54545/callback?code=abc&state=xyz"),
            (Some("abc".to_owned()), Some("xyz".to_owned()))
        );
        assert_eq!(
            parse_callback_input("code=abc&state=xyz"),
            (Some("abc".to_owned()), Some("xyz".to_owned()))
        );
        assert_eq!(
            parse_callback_input("?code=abc&state=xyz"),
            (Some("abc".to_owned()), Some("xyz".to_owned()))
        );
        assert_eq!(
            parse_callback_input(" just-a-code "),
            (Some("just-a-code".to_owned()), None)
        );
        assert_eq!(
            parse_callback_input("abc#xyz"),
            (Some("abc".to_owned()), Some("xyz".to_owned()))
        );
        let (code, state) = parse_callback_input(
            "http://localhost:54545/callback?error=access_denied&error_description=nope",
        );
        assert_eq!((code, state), (None, None));
        assert_eq!(parse_callback_input("   "), (None, None));
    }
}
