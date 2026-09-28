//! Loopback callback server and the manual-paste parser.
//!
//! omp's `callback-server.ts` is the reference: a `TcpListener` on the
//! descriptor's host/port, the descriptor's path reading
//! `code`/`state`/`error`/`error_description`, everything else 404, a free
//! port only when the rule allows it, and a 300-second wait.

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

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>titi</title></head><body><h1>Authorization complete</h1>\
<p>You can close this tab and return to titi.</p></body></html>";

/// A bound loopback listener waiting for exactly one provider redirect.
#[derive(Debug)]
pub struct CallbackServer {
    listener: TcpListener,
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
        let listener = match TcpListener::bind((spec.host, spec.port)).await {
            Ok(listener) => listener,
            Err(busy) if may_fallback && spec.port != 0 => {
                TcpListener::bind((spec.host, 0)).await.map_err(|e| {
                    OAuthError::Config {
                        message: format!(
                            "cannot bind the oauth callback ({busy}); ephemeral fallback also failed: {e}"
                        ),
                    }
                })?
            }
            Err(busy) => {
                return Err(OAuthError::Config {
                    message: format!("oauth callback port {} is not available: {busy}", spec.port),
                });
            }
        };
        let local = listener.local_addr().map_err(|e| OAuthError::Server {
            message: format!("cannot read the oauth callback address: {e}"),
        })?;
        let redirect_uri = match spec.redirect_uri {
            Some(uri) => uri.to_owned(),
            None => format!("http://{}:{}{}", spec.host, local.port(), spec.path),
        };
        Ok(Self {
            listener,
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
            let accepted = tokio::select! {
                biased;
                accepted = self.listener.accept() => Some(accepted),
                () = tokio::time::sleep(slice) => None,
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
                Ok(Ok(Some(code))) => return Ok(code),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    return Err(OAuthError::Timeout {
                        seconds: CALLBACK_TIMEOUT_SECS,
                    });
                }
            }
        }
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

    fn test_spec(port: u16, port_fallback: bool) -> CallbackSpec {
        CallbackSpec {
            host: "127.0.0.1",
            port,
            path: "/callback",
            redirect_uri: None,
            port_fallback,
        }
    }

    async fn get(addr: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("read");
        String::from_utf8_lossy(&response).into_owned()
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
