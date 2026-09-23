//! Network tools: `fetch` for one bounded GET, `web_search` for a
//! provider-agnostic search endpoint.
//!
//! Both sit at [`ApprovalTier::Network`]: reaching an outside host is not a
//! filesystem read, and a URL is a channel out of the machine. What comes
//! back is ordinary tool output and goes through the engine's redaction on
//! the way to the model; nothing here builds a second path around it.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use smol_str::SmolStr;
use thiserror::Error;
use titi_providers::ToolSpec;

use crate::fs::{arg_str, err, ok};
use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// Most of a response body the model ever sees. The rest is dropped: a page
/// is untrusted input and a megabyte of it is a megabyte of context gone.
pub const FETCH_BYTE_CAP: usize = 64 * 1024;

/// One deadline for connect, headers and body together.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

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

    async fn fetch(&self, args: Value) -> Result<String, WebError> {
        let raw = arg_str(&args, "url").ok_or(WebError::MissingArg("url"))?;
        let url = http_url(raw.trim())?;
        let response = self
            .client
            .get(url.clone())
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
                message.replace(provider.api_key.as_str(), "<redacted>")
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
        .redirect(reqwest::redirect::Policy::limited(5))
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
            let listener = TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
            let port = listener.local_addr().expect("bound address").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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
