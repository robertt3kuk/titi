//! Network tools: `fetch` for one bounded GET, `web_search` for a
//! provider-agnostic search endpoint.
//!
//! Both sit at [`ApprovalTier::Network`]: reaching an outside host is not a
//! filesystem read, and a URL is a channel out of the machine. What comes
//! back is ordinary tool output and goes through the engine's redaction on
//! the way to the model; nothing here builds a second path around it.
//!
//! Both name the call in one row before anyone is asked to approve it, and
//! `fetch` refuses a cloud metadata host outright: that endpoint answers with
//! the machine's credentials rather than with a page. Nothing else about a
//! fetch changes — the scheme check, the timeout and the body cap stand.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use smol_str::SmolStr;
use thiserror::Error;
use titi_providers::ToolSpec;

use crate::fs::{DESCRIBE_MAX, arg_str, describe_line, err, ok};
use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// The one placeholder this crate puts on a value it will not repeat: a
/// secret never comes back out in the clear, it comes back as this.
pub(crate) const REDACTED: &str = "<redacted>";

/// Most of a response body the model ever sees. The rest is dropped: a page
/// is untrusted input and a megabyte of it is a megabyte of context gone.
pub const FETCH_BYTE_CAP: usize = 64 * 1024;

/// One deadline for connect, headers and body together.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Most redirects one `fetch` follows before it gives up.
///
/// The same bound the http client used to apply, moved into the tool: a
/// redirect is a *new URL*, and the metadata guard is decided from the URL, so
/// following one inside the client would walk past a check that only ever saw
/// the first.
pub const FETCH_REDIRECTS: u32 = 5;

/// Search endpoint, e.g. `https://api.search.example/res`.
pub const SEARCH_ENDPOINT_ENV: &str = "TITI_SEARCH_ENDPOINT";
/// Search credential. Never logged and never put in the URL.
pub const SEARCH_API_KEY_ENV: &str = "TITI_SEARCH_API_KEY";
/// Query parameter the endpoint expects, default `q`.
pub const SEARCH_QUERY_PARAM_ENV: &str = "TITI_SEARCH_QUERY_PARAM";
/// Header the key travels in, default `Authorization`.
pub const SEARCH_AUTH_HEADER_ENV: &str = "TITI_SEARCH_AUTH_HEADER";

#[derive(Debug, Error)]
pub enum WebError {
    #[error("missing {0}")]
    MissingArg(&'static str),
    #[error("{url} is not a url: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("{scheme} is refused; this tool speaks http and https only")]
    UnsupportedScheme { scheme: String },
    #[error("request failed: {message}")]
    Request { message: String },
    #[error("request timed out after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("{url} answered {status}")]
    Status { status: u16, url: String },
    #[error("body read failed: {message}")]
    Body { message: String },
    #[error(
        "no search provider configured; set TITI_SEARCH_ENDPOINT and TITI_SEARCH_API_KEY, or pass a SearchProvider"
    )]
    NoSearchProvider,
    #[error("http client unavailable: {message}")]
    Client { message: String },
    #[error("{reason}")]
    Refused { reason: String },
    #[error("gave up after {hops} redirects; the last one pointed at {url}")]
    TooManyRedirects { hops: u32, url: String },
}

/// Where `web_search` sends a query and how the key rides along. Held by
/// value so the caller can inject it (like `SensitivePolicy`), with
/// [`SearchProvider::from_env`] as the zero-wiring path.
#[derive(Clone)]
pub struct SearchProvider {
    endpoint: SmolStr,
    api_key: SmolStr,
    query_param: SmolStr,
    auth_header: SmolStr,
}

/// Hand-written so the key cannot reach a log through `{:?}`.
impl std::fmt::Debug for SearchProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchProvider")
            .field("endpoint", &self.endpoint)
            .field("query_param", &self.query_param)
            .field("auth_header", &self.auth_header)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl SearchProvider {
    pub fn new(endpoint: impl Into<SmolStr>, api_key: impl Into<SmolStr>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            query_param: "q".into(),
            auth_header: "Authorization".into(),
        }
    }

    pub fn with_query_param(mut self, param: impl Into<SmolStr>) -> Self {
        self.query_param = param.into();
        self
    }

    pub fn with_auth_header(mut self, header: impl Into<SmolStr>) -> Self {
        self.auth_header = header.into();
        self
    }

    /// `None` when either half is missing: a search tool with an endpoint and
    /// no key would just hand the provider a 401 on every turn.
    pub fn from_env() -> Option<Self> {
        let endpoint = env_value(SEARCH_ENDPOINT_ENV)?;
        let api_key = env_value(SEARCH_API_KEY_ENV)?;
        let mut provider = Self::new(endpoint, api_key);
        if let Some(param) = env_value(SEARCH_QUERY_PARAM_ENV) {
            provider = provider.with_query_param(param);
        }
        if let Some(header) = env_value(SEARCH_AUTH_HEADER_ENV) {
            provider = provider.with_auth_header(header);
        }
        Some(provider)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Bearer only for `Authorization`; providers with their own header
    /// (`X-Subscription-Token`, `X-API-Key`) want the bare key.
    fn auth_value(&self) -> String {
        if self.auth_header.eq_ignore_ascii_case("authorization") {
            format!("Bearer {}", self.api_key)
        } else {
            self.api_key.to_string()
        }
    }
}

fn env_value(key: &str) -> Option<SmolStr> {
    let value = std::env::var(key).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| SmolStr::from(value))
}

/// One GET, bounded body, http(s) only.
pub struct FetchTool {
    client: reqwest::Client,
    cap: usize,
}

impl FetchTool {
    pub fn new() -> Result<Self, WebError> {
        Ok(Self {
            client: http_client()?,
            cap: FETCH_BYTE_CAP,
        })
    }

    /// One GET, following redirects by hand so every hop is judged.
    ///
    /// The metadata refusal is decided from the URL, and a redirect hands back
    /// a URL the model never wrote. Following one inside the http client would
    /// mean the check saw only the first URL, which is the whole guard: a page
    /// that answers `302 Location: http://169.254.169.254/…` would put the
    /// instance's credentials in the model's context. So each hop is parsed,
    /// scheme-checked and refused-or-allowed exactly as the first URL is, and
    /// the chain is bounded by [`FETCH_REDIRECTS`].
    async fn fetch(&self, args: Value) -> Result<String, WebError> {
        let raw = arg_str(&args, "url").ok_or(WebError::MissingArg("url"))?;
        let mut url = http_url(raw.trim())?;
        let mut hops = 0;
        loop {
            // The same refusal the engine asks for before approval, asked
            // again here: `invoke` is reachable on its own, and a metadata
            // service must not be reached through it either.
            if let Some(reason) = metadata_refusal(&url) {
                return Err(WebError::Refused { reason });
            }
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(request_error)?;
            let status = response.status();
            if !status.is_redirection() {
                if !status.is_success() {
                    return Err(WebError::Status {
                        status: status.as_u16(),
                        url: url.to_string(),
                    });
                }
                return read_capped(response, self.cap).await;
            }
            // A redirection with nowhere to go is answered as what it is: a
            // status the tool cannot use, not a body.
            let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                return Err(WebError::Status {
                    status: status.as_u16(),
                    url: url.to_string(),
                });
            };
            if hops >= FETCH_REDIRECTS {
                return Err(WebError::TooManyRedirects {
                    hops: FETCH_REDIRECTS,
                    url: url.to_string(),
                });
            }
            let location = String::from_utf8_lossy(location.as_bytes()).into_owned();
            // Relative locations resolve against the URL that answered, which
            // is what every browser does and what a server means by `/next`.
            let next = url.join(&location).map_err(|error| WebError::InvalidUrl {
                url: location.clone(),
                reason: error.to_string(),
            })?;
            url = http_url(next.as_str())?;
            hops += 1;
        }
    }
}

#[async_trait]
impl ToolHandler for FetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "fetch".into(),
                description: "GET an http(s) URL and return the response body".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "url": { "type": "string" } },
                    "required": ["url"]
                }),
            },
            approval: ApprovalTier::Network,
        }
    }

    fn describe(&self, args: &serde_json::Value) -> Option<String> {
        let raw = arg_str(args, "url")?;
        let url = http_url(raw.trim()).ok()?;
        Some(format!(
            "fetch {}",
            describe_line(&masked_url(&url), DESCRIBE_MAX)
        ))
    }

    /// Asked before anyone is asked to approve the call: a metadata service
    /// hands the instance's credentials to whoever can reach it, so a call
    /// there is answered instead of put to the person.
    ///
    /// This judges the URL the model wrote, which is all an approval can name;
    /// a redirect to a metadata host is caught at the hop, in [`Self::fetch`],
    /// where the redirect's own URL is in hand.
    fn refusal(&self, args: &serde_json::Value) -> Option<String> {
        let raw = arg_str(args, "url")?;
        let url = http_url(raw.trim()).ok()?;
        metadata_refusal(&url)
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match self.fetch(args).await {
            Ok(body) => ok(body),
            Err(error) => err(error.to_string()),
        }
    }
}

/// Search through whatever endpoint the user configured. The provider's
/// answer is passed through untouched: titi does not know its JSON shape and
/// guessing one would break every provider but the one guessed for.
pub struct WebSearchTool {
    client: reqwest::Client,
    provider: Option<SearchProvider>,
    cap: usize,
}

impl WebSearchTool {
    pub fn new(provider: Option<SearchProvider>) -> Result<Self, WebError> {
        Ok(Self {
            client: http_client()?,
            provider,
            cap: FETCH_BYTE_CAP,
        })
    }

    async fn search(&self, args: Value) -> Result<String, WebError> {
        let query = arg_str(&args, "query").ok_or(WebError::MissingArg("query"))?;
        let query = query.trim();
        if query.is_empty() {
            return Err(WebError::MissingArg("query"));
        }
        let provider = self.provider.as_ref().ok_or(WebError::NoSearchProvider)?;
        let mut url = http_url(provider.endpoint())?;
        url.query_pairs_mut()
            .append_pair(&provider.query_param, query);
        let response = self
            .client
            .get(url.clone())
            .header(provider.auth_header.as_str(), provider.auth_value())
            .header("accept", "application/json")
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(WebError::Status {
                status,
                url: url.to_string(),
            });
        }
        read_capped(response, self.cap).await
    }

    /// Belt and braces: a transport message can quote what it was handed.
    fn scrub(&self, message: String) -> String {
        match &self.provider {
            Some(provider) if !provider.api_key.is_empty() => {
                redact_occurrences(message, provider.api_key.as_str())
            }
            _ => message,
        }
    }
}

#[async_trait]
impl ToolHandler for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "web_search".into(),
                description: "Search the web through the configured search provider".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }),
            },
            approval: ApprovalTier::Network,
        }
    }

    fn describe(&self, args: &serde_json::Value) -> Option<String> {
        let query = arg_str(args, "query")?;
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        Some(format!(
            "web_search \"{}\"",
            describe_line(query, DESCRIBE_MAX)
        ))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match self.search(args).await {
            Ok(body) => ok(body),
            Err(error) => err(self.scrub(error.to_string())),
        }
    }
}

/// The network tools. Empty when the TLS client cannot be built: that is a
/// broken platform, and a registry short two tools beats a session that
/// cannot start.
pub fn web_tools(provider: Option<SearchProvider>) -> Vec<Box<dyn ToolHandler>> {
    let mut tools: Vec<Box<dyn ToolHandler>> = Vec::new();
    if let Ok(fetch) = FetchTool::new() {
        tools.push(Box::new(fetch));
    }
    if let Ok(search) = WebSearchTool::new(provider) {
        tools.push(Box::new(search));
    }
    tools
}

fn http_client() -> Result<reqwest::Client, WebError> {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        // `fetch` follows redirects itself, one hop at a time, so that every
        // URL it reaches is judged before a request is built for it: the
        // client's own policy follows a `Location` it never shows anyone, and
        // a redirect to a metadata service would land there unchecked.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| WebError::Client {
            message: error.to_string(),
        })
}

fn http_url(raw: &str) -> Result<reqwest::Url, WebError> {
    let url = reqwest::Url::parse(raw).map_err(|error| WebError::InvalidUrl {
        url: raw.to_owned(),
        reason: error.to_string(),
    })?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(WebError::UnsupportedScheme {
            scheme: format!("{scheme}://"),
        }),
    }
}

fn request_error(error: reqwest::Error) -> WebError {
    if error.is_timeout() {
        return WebError::Timeout {
            seconds: FETCH_TIMEOUT.as_secs(),
        };
    }
    WebError::Request {
        message: error.to_string(),
    }
}

/// Replaces every occurrence of `secret` in `text` with [`REDACTED`].
/// `scrub` and the URL printer share this one replacement, so the crate has a
/// single mask and not two that can drift.
fn redact_occurrences(text: String, secret: &str) -> String {
    if secret.is_empty() {
        return text;
    }
    text.replace(secret, REDACTED)
}

/// The URL an approval prompt shows: scheme, host, port and path as written,
/// and the query reduced to its keys. A query *value* is where a credential
/// rides (`?access_token=…`) and cannot be vouched for from the outside, so
/// it is never repeated — the key survives, the value becomes [`REDACTED`].
/// Userinfo (`https://user:pass@host/`) goes with it: it is the same kind of
/// value and nothing about it belongs on a one-row screen.
fn masked_url(url: &reqwest::Url) -> String {
    let mut out = String::new();
    out.push_str(url.scheme());
    out.push_str("://");
    if let Some(host) = url.host_str() {
        out.push_str(host);
    }
    if let Some(port) = url.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(url.path());
    for (index, (key, _)) in url.query_pairs().enumerate() {
        out.push(if index == 0 { '?' } else { '&' });
        out.push_str(key.as_ref());
        out.push('=');
        out.push_str(REDACTED);
    }
    out
}

/// Hosts that answer with the cloud instance's credentials rather than with a
/// page. Only names confirmed by their vendor's own metadata documentation go
/// here — a wrong entry would refuse an ordinary host.
const METADATA_HOSTS: &[&str] = &[
    // Google Compute Engine (the alias `metadata.goog` is not listed here).
    "metadata.google.internal",
    // Tencent Cloud CVM.
    "metadata.tencentyun.com",
];

/// Alibaba Cloud's metadata service, which has one fixed address and no name.
const ALIBABA_METADATA: Ipv4Addr = Ipv4Addr::new(100, 100, 100, 200);

/// The refusal a cloud metadata endpoint earns, or `None` for every other
/// host. It is decided from the URL alone, so the engine can hand the person
/// this sentence instead of an approval, and `fetch` can refuse the same call
/// before it builds a request — no socket opens either way.
///
/// The whole link-local block (`169.254.0.0/16`, `fe80::/10`) is refused, not
/// just the two addresses AWS and Google happen to document: nothing on
/// link-local is a public host, while loopback (`127.0.0.0/8`, `::1`) stays
/// allowed because titi's own smoke servers and local model backends live
/// there — a link-local address reaches out to the network your machine is
/// attached to, loopback never leaves it.
fn metadata_refusal(url: &reqwest::Url) -> Option<String> {
    let raw = url.host_str()?;
    // `host_str` keeps the brackets around an IPv6 literal and the trailing
    // dot a name may carry; both name the same host, so both are stripped
    // before the host is judged.
    let host = raw
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(raw);
    let host = host.trim_end_matches('.');

    if let Some(name) = METADATA_HOSTS
        .iter()
        .find(|name| host.eq_ignore_ascii_case(name))
    {
        return Some(format!(
            "refusing {name}: it is a cloud metadata service, which answers with the machine's credentials"
        ));
    }
    let Ok(address) = host.parse::<IpAddr>() else {
        return None;
    };
    if address == IpAddr::V4(ALIBABA_METADATA) {
        return Some(format!(
            "refusing {host}: it is the Alibaba Cloud metadata service, which answers with the machine's credentials"
        ));
    }
    if is_link_local(address) {
        return Some(format!(
            "refusing {host}: link-local addresses (169.254.0.0/16, fe80::/10) are the cloud metadata service, which answers with the machine's credentials"
        ));
    }
    None
}

fn is_link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

/// Reads at most `cap` bytes and stops: the point of the cap is to not pull a
/// huge body into memory, so a body that overruns it is abandoned mid-stream
/// rather than drained.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<String, WebError> {
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.map_err(|error| WebError::Body {
        message: error.to_string(),
    })? {
        let room = cap - body.len();
        if room < chunk.len() {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let mut text = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        text.push_str(&format!("\n\n[titi: truncated at {cap} bytes]"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A loopback server that answers every connection with one canned
    /// response and keeps what it was asked. No test here touches a network.
    struct MockServer {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        fn start(status_line: &str, body: &str) -> Self {
            Self::start_with(status_line, &[], body)
        }

        fn start_with(status_line: &str, headers: &[(&str, &str)], body: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
            let port = listener.local_addr().expect("bound address").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let extra: String = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: text/plain\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let mut buf = [0_u8; 8192];
                    let read = stream.read(&mut buf).unwrap_or(0);
                    if let Ok(mut seen) = seen.lock() {
                        seen.push(String::from_utf8_lossy(&buf[..read]).into_owned());
                    }
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self { port, requests }
        }

        /// Answers every connection with a `302` pointing at `to`.
        fn redirecting(to: &str) -> Self {
            Self::start_with("302 Found", &[("location", to)], "")
        }

        /// Answers every connection with a `302` pointing back at itself, so a
        /// chain of any length can be asked for.
        fn redirecting_loop(path: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
            let port = listener.local_addr().expect("bound address").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let response = format!(
                "HTTP/1.1 302 Found\r\ncontent-type: text/plain\r\nlocation: http://127.0.0.1:{port}{path}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let mut buf = [0_u8; 8192];
                    let read = stream.read(&mut buf).unwrap_or(0);
                    if let Ok(mut seen) = seen.lock() {
                        seen.push(String::from_utf8_lossy(&buf[..read]).into_owned());
                    }
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self { port, requests }
        }

        fn hits(&self) -> usize {
            self.requests.lock().map(|seen| seen.len()).unwrap_or(0)
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn last_request(&self) -> String {
            self.requests
                .lock()
                .ok()
                .and_then(|seen| seen.last().cloned())
                .unwrap_or_default()
        }
    }

    fn fetch_tool() -> FetchTool {
        FetchTool::new().expect("a tls client builds")
    }

    #[tokio::test]
    async fn fetch_returns_the_body() {
        let server = MockServer::start("200 OK", "hello from example.invalid");
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/page") }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "hello from example.invalid");
    }

    /// A redirect to a metadata host is refused at the hop, before a request
    /// is built for it: the guard is decided from the URL, and a `Location`
    /// is a URL the model never wrote.
    #[tokio::test]
    async fn fetch_refuses_a_redirect_to_a_metadata_host() {
        let server = MockServer::redirecting("http://169.254.169.254/latest/meta-data/");
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/page") }))
            .await;
        assert!(result.is_error, "the redirect must not be followed");
        assert!(
            result.output.contains("169.254.169.254"),
            "the refusal names the host it refused: {}",
            result.output
        );
        assert!(
            result.output.contains("metadata service"),
            "and says what that host is: {}",
            result.output
        );
        assert_eq!(
            server.hits(),
            1,
            "the redirector was asked once; nothing followed the Location"
        );
    }

    /// A redirect to another URL is followed and that URL's body is the answer.
    #[tokio::test]
    async fn fetch_follows_a_redirect_to_another_url() {
        let target = MockServer::start("200 OK", "second page");
        let server = MockServer::redirecting(&target.url("/moved"));
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/page") }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "second page");
        assert_eq!(server.hits(), 1);
        assert_eq!(target.hits(), 1, "the hop was requested");
    }

    /// A relative `Location` resolves against the URL that answered, as it
    /// does in a browser.
    #[tokio::test]
    async fn fetch_resolves_a_relative_redirect() {
        let server = MockServer::redirecting("/elsewhere");
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/page") }))
            .await;
        // The server answers every path with the same 302, so this is the
        // chain limit rather than a body -- what matters is that the second
        // request went to the resolved path on the same host.
        assert!(result.is_error, "{}", result.output);
        assert!(
            result.output.contains("gave up after 5 redirects"),
            "{}",
            result.output
        );
        assert_eq!(server.hits(), 6, "the first request plus five hops");
    }

    /// The chain is bounded, and the bound is reported rather than hidden.
    #[tokio::test]
    async fn fetch_gives_up_after_five_redirects() {
        let server = MockServer::redirecting_loop("/again");
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/again") }))
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("gave up after 5 redirects"),
            "{}",
            result.output
        );
        assert_eq!(server.hits(), 6, "the first request plus five hops");
    }

    /// A hop to a scheme this tool does not speak is refused like the first
    /// URL is, not handed to the client to fail on.
    #[tokio::test]
    async fn fetch_refuses_a_redirect_to_another_scheme() {
        let server = MockServer::redirecting("file:///etc/passwd");
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/page") }))
            .await;
        assert!(result.is_error, "{}", result.output);
        assert!(
            result.output.contains("file://"),
            "the refusal names the scheme: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn fetch_reports_a_non_2xx_as_an_error() {
        let server = MockServer::start("404 Not Found", "nope");
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/missing") }))
            .await;
        assert!(result.is_error, "404 is an error, not a body");
        assert!(result.output.contains("404"), "{}", result.output);
    }

    #[tokio::test]
    async fn fetch_truncates_an_oversized_body() {
        let body = "a".repeat(FETCH_BYTE_CAP + 4096);
        let server = MockServer::start("200 OK", &body);
        let result = fetch_tool()
            .invoke(serde_json::json!({ "url": server.url("/big") }))
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert!(
            result.output.starts_with(&"a".repeat(FETCH_BYTE_CAP)),
            "the first {FETCH_BYTE_CAP} bytes survive"
        );
        assert!(
            result.output.contains("truncated"),
            "the cut is announced: {}",
            &result.output[FETCH_BYTE_CAP..]
        );
        assert!(
            result.output.len() < FETCH_BYTE_CAP + 128,
            "nothing past the cap but the marker"
        );
    }

    #[tokio::test]
    async fn fetch_refuses_a_non_http_scheme() {
        for url in ["file:///etc/passwd", "ftp://example.invalid/x"] {
            let result = fetch_tool().invoke(serde_json::json!({ "url": url })).await;
            assert!(result.is_error, "{url} must be refused");
            assert!(
                result.output.contains("http and https only"),
                "{}",
                result.output
            );
        }
    }

    #[tokio::test]
    async fn fetch_without_a_url_is_an_error() {
        let result = fetch_tool().invoke(serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.output.contains("url"), "{}", result.output);
    }

    #[test]
    fn fetch_describe_names_the_url_and_masks_the_query() {
        let tool = fetch_tool();
        let detail = tool
            .describe(&serde_json::json!({
                "url": "https://api.example/s?q=x&token=sk-live-abcdefgh"
            }))
            .expect("a url describes itself");
        assert_eq!(
            detail,
            "fetch https://api.example/s?q=<redacted>&token=<redacted>"
        );
        assert!(!detail.contains("sk-live-abcdefgh"), "{detail}");

        // A url longer than the one row a description gets is cut, never
        // unmasked: the query is already gone before the bound applies.
        let long = tool
            .describe(&serde_json::json!({
                "url": format!("https://api.example/{}/page?token=sk-live-abcdefgh", "d".repeat(80))
            }))
            .expect("a url describes itself");
        assert!(long.ends_with('…'), "{long}");
        assert!(!long.contains("sk-live-abcdefgh"), "{long}");
    }

    #[test]
    fn fetch_describe_keeps_a_plain_url_and_drops_userinfo() {
        let tool = fetch_tool();
        assert_eq!(
            tool.describe(&serde_json::json!({ "url": "https://docs.rs/serde" })),
            Some("fetch https://docs.rs/serde".to_owned())
        );
        let detail = tool
            .describe(&serde_json::json!({ "url": "https://user:pass@example.invalid/private" }))
            .expect("a url describes itself");
        assert_eq!(detail, "fetch https://example.invalid/private");
        assert!(!detail.contains("pass"), "{detail}");
    }

    #[test]
    fn fetch_describe_without_a_http_url_says_nothing() {
        let tool = fetch_tool();
        assert_eq!(tool.describe(&serde_json::json!({})), None);
        assert_eq!(
            tool.describe(&serde_json::json!({ "url": "file:///etc/passwd" })),
            None,
            "a scheme fetch refuses needs no description either"
        );
    }

    /// A metadata host is answered, not fetched: the sentence names the host
    /// and the reason. `invoke` hands back exactly that sentence rather than
    /// a transport error, which is what "refused before the request was
    /// built" looks like from outside the process.
    #[tokio::test]
    async fn a_metadata_host_is_refused_by_name() {
        let tool = fetch_tool();
        for (url, named) in [
            (
                "http://169.254.169.254/latest/meta-data/",
                "169.254.169.254",
            ),
            ("http://169.254.170.2/v2/metadata", "169.254.170.2"),
            ("http://[fe80::1]/x", "fe80::1"),
            (
                "http://metadata.google.internal/computeMetadata/v1/",
                "metadata.google.internal",
            ),
            (
                "http://metadata.google.internal./computeMetadata/v1/",
                "metadata.google.internal",
            ),
            (
                "http://100.100.100.200/latest/meta-data/",
                "100.100.100.200",
            ),
            (
                "http://metadata.tencentyun.com/latest/meta-data/",
                "metadata.tencentyun.com",
            ),
        ] {
            let args = serde_json::json!({ "url": url });
            let refusal = tool.refusal(&args).expect("a metadata host is refused");
            assert!(refusal.contains(named), "{refusal}");
            assert!(refusal.contains("metadata service"), "{refusal}");
            let result = tool.invoke(args).await;
            assert!(result.is_error, "{url}");
            assert_eq!(
                result.output.as_str(),
                refusal,
                "invoke answers the same sentence, not a transport error"
            );
            assert!(
                !result.output.contains("request failed"),
                "no request was made: {}",
                result.output
            );
        }
    }

    /// The guard is narrow: loopback (where titi's own smoke servers and local
    /// model backends live) and ordinary hosts go through untouched, and an
    /// address in the path is not the address of the host.
    #[test]
    fn loopback_and_ordinary_hosts_are_allowed() {
        let tool = fetch_tool();
        for url in [
            "http://127.0.0.1:8080/status",
            "http://[::1]:11434/api/tags",
            "https://docs.rs/serde",
            "https://example.invalid/169.254.169.254",
        ] {
            assert_eq!(
                tool.refusal(&serde_json::json!({ "url": url })),
                None,
                "{url} was refused"
            );
        }
    }

    #[tokio::test]
    async fn web_search_without_a_provider_is_an_error() {
        let tool = WebSearchTool::new(None).expect("a tls client builds");
        let result = tool
            .invoke(serde_json::json!({ "query": "rust ownership" }))
            .await;
        assert!(result.is_error, "no provider means no call");
        assert!(
            result.output.contains("no search provider configured"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn web_search_sends_the_query_and_keeps_the_key_in_a_header() {
        let server = MockServer::start("200 OK", r#"{"results":[{"url":"https://a.invalid"}]}"#);
        let provider = SearchProvider::new(server.url("/search"), "sk-test");
        let tool = WebSearchTool::new(Some(provider)).expect("a tls client builds");

        let result = tool
            .invoke(serde_json::json!({ "query": "rust ownership" }))
            .await;

        assert!(!result.is_error, "{}", result.output);
        assert_eq!(
            result.output,
            r#"{"results":[{"url":"https://a.invalid"}]}"#
        );
        let request = server.last_request();
        assert!(
            request.contains("/search?q=rust+ownership")
                || request.contains("/search?q=rust%20ownership"),
            "{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer sk-test"),
            "the key rides in the header, not the query: {request}"
        );
        assert!(
            !result.output.contains("sk-test"),
            "the key never comes back out"
        );
    }

    #[tokio::test]
    async fn a_custom_header_carries_the_bare_key() {
        let server = MockServer::start("200 OK", "{}");
        let provider = SearchProvider::new(server.url("/res"), "sk-test")
            .with_auth_header("X-Subscription-Token")
            .with_query_param("query");
        let tool = WebSearchTool::new(Some(provider)).expect("a tls client builds");

        let result = tool.invoke(serde_json::json!({ "query": "titi" })).await;

        assert!(!result.is_error, "{}", result.output);
        let request = server.last_request().to_lowercase();
        assert!(
            request.contains("x-subscription-token: sk-test"),
            "{request}"
        );
        assert!(request.contains("?query=titi"), "{request}");
    }

    #[test]
    fn web_search_describe_quotes_and_bounds_the_query() {
        let tool = WebSearchTool::new(None).expect("a tls client builds");
        assert_eq!(
            tool.describe(&serde_json::json!({ "query": "  rust ownership  " })),
            Some("web_search \"rust ownership\"".to_owned()),
            "the query is quoted, trimmed, and nothing else rides along"
        );

        let detail = tool
            .describe(&serde_json::json!({ "query": "line\none" }))
            .expect("a query describes itself");
        assert_eq!(detail, "web_search \"line one\"");

        let long = "x".repeat(DESCRIBE_MAX + 25);
        let detail = tool
            .describe(&serde_json::json!({ "query": long }))
            .expect("a query describes itself");
        assert!(detail.contains('…'), "{detail}");
        assert!(detail.ends_with('"'), "{detail}");
        assert!(
            detail.chars().count() <= DESCRIBE_MAX + "web_search \"\"".len() + 1,
            "the line is bounded: {detail}"
        );

        assert_eq!(tool.describe(&serde_json::json!({ "query": "   " })), None);
        assert_eq!(tool.describe(&serde_json::json!({})), None);
    }

    #[tokio::test]
    async fn web_search_error_never_quotes_the_key() {
        let provider = SearchProvider::new("https://example.invalid/search", "sk-test");
        let tool = WebSearchTool::new(Some(provider)).expect("a tls client builds");
        assert_eq!(
            tool.scrub("upstream said sk-test is bad".to_owned()),
            "upstream said <redacted> is bad"
        );
    }

    #[test]
    fn the_key_stays_out_of_debug_output() {
        let provider = SearchProvider::new("https://example.invalid/search", "sk-test");
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("sk-test"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn the_network_tier_is_not_auto_approved_by_write_mode() {
        use crate::ApprovalMode;
        assert!(!ApprovalMode::Write.auto_approves(ApprovalTier::Network));
        assert!(ApprovalMode::Yolo.auto_approves(ApprovalTier::Network));
    }

    #[test]
    fn both_tools_register_at_network_tier() {
        let tools = web_tools(None);
        let names: Vec<String> = tools
            .iter()
            .map(|tool| tool.definition().spec.name.to_string())
            .collect();
        assert!(names.contains(&"fetch".to_owned()), "{names:?}");
        assert!(names.contains(&"web_search".to_owned()), "{names:?}");
        for tool in &tools {
            assert_eq!(tool.definition().approval, ApprovalTier::Network);
        }
    }
}
