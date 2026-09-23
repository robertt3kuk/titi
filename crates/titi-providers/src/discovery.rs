//! Model discovery: `GET <base_url>/models` on an OpenAI-compatible server,
//! with the reason a listing failed kept instead of thrown away.
//!
//! An empty model list is the one answer a user cannot act on: a rejected key,
//! a typo'd base URL and a server that is simply down all look identical once
//! the status is dropped. [`list_models`] therefore separates the three cases
//! a surface has to phrase differently — the key was refused (401/403), the
//! server answered something else, the server was not reachable at all.
//!
//! Spec: `docs/research/providers-streaming/README.md`.

use futures::StreamExt;
use smol_str::SmolStr;

use crate::http::{HttpFetch, HttpRequest};
use crate::transport::TransportError;

/// Most bytes read from a model listing before it is given up on. A listing
/// is untrusted input and is buffered whole to be parsed.
pub const MAX_DISCOVERY_BODY: usize = 256 * 1024;

/// Why a provider's model list did not arrive.
///
/// Every variant names the provider, because a surface listing four providers
/// has to say *which* one refused. No variant carries the credential: these
/// strings reach the screen and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// HTTP 401. The key is missing, expired, or not a key this provider knows.
    Unauthorized { provider: SmolStr, status: u16 },
    /// HTTP 403. The key authenticates but may not list models — wrong scope,
    /// wrong plan, or an org/region the key does not cover.
    Forbidden { provider: SmolStr, status: u16 },
    /// Any other non-2xx: the provider answered, but not with a listing.
    Status { provider: SmolStr, status: u16 },
    /// The request never produced a response: DNS, refused connection, TLS.
    Unreachable { provider: SmolStr, message: SmolStr },
    /// A 2xx body that is not an OpenAI model listing, or one too large to
    /// read. Not an auth problem, and not the server being down.
    Malformed { provider: SmolStr, reason: SmolStr },
}

impl DiscoveryError {
    pub fn provider(&self) -> &str {
        match self {
            Self::Unauthorized { provider, .. }
            | Self::Forbidden { provider, .. }
            | Self::Status { provider, .. }
            | Self::Unreachable { provider, .. }
            | Self::Malformed { provider, .. } => provider.as_str(),
        }
    }

    /// The HTTP status, when the provider answered with one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Unauthorized { status, .. }
            | Self::Forbidden { status, .. }
            | Self::Status { status, .. } => Some(*status),
            Self::Unreachable { .. } | Self::Malformed { .. } => None,
        }
    }

    /// Whether the credential is what has to change. A surface shows these
    /// unprompted; the rest belong next to the provider that went quiet.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Unauthorized { .. } | Self::Forbidden { .. })
    }
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { provider, status } => write!(
                f,
                "authentication failed for provider {provider} (HTTP {status}): \
                 key rejected — check the key with `titi --set-key`"
            ),
            Self::Forbidden { provider, status } => write!(
                f,
                "access forbidden for provider {provider} (HTTP {status}): \
                 the key is valid but may not list models — check its scope or plan, \
                 or replace it with `titi --set-key`"
            ),
            Self::Status { provider, status } => write!(
                f,
                "provider {provider} answered HTTP {status} when listing models"
            ),
            Self::Unreachable { provider, message } => write!(
                f,
                "provider {provider} could not be reached when listing models: {message}"
            ),
            Self::Malformed { provider, reason } => write!(
                f,
                "provider {provider} returned an unusable model list: {reason}"
            ),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// Model ids an OpenAI-compatible server lists at `GET <base_url>/models`.
///
/// `api_key` is sent as a bearer token and never appears in a returned error.
/// Ids come back as the server wrote them, trimmed and de-duplicated; naming,
/// filtering and per-server caps are the caller's policy.
pub async fn list_models(
    provider: &str,
    base_url: &str,
    api_key: Option<&str>,
    fetch: &dyn HttpFetch,
) -> Result<Vec<SmolStr>, DiscoveryError> {
    let mut headers = vec![(SmolStr::new("accept"), SmolStr::new("application/json"))];
    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        headers.push((
            SmolStr::new("authorization"),
            format!("Bearer {key}").into(),
        ));
    }
    let request = HttpRequest {
        method: "GET".into(),
        url: format!("{}/models", base_url.trim_end_matches('/')).into(),
        headers,
        body: None,
    };

    let response = fetch
        .fetch(request)
        .await
        .map_err(|error| unreachable_error(provider, &error))?;
    match response.status {
        401 => {
            return Err(DiscoveryError::Unauthorized {
                provider: provider.into(),
                status: response.status,
            });
        }
        403 => {
            return Err(DiscoveryError::Forbidden {
                provider: provider.into(),
                status: response.status,
            });
        }
        status if !(200..300).contains(&status) => {
            return Err(DiscoveryError::Status {
                provider: provider.into(),
                status,
            });
        }
        _ => {}
    }

    let mut body = Vec::new();
    let mut chunks = response.body;
    while let Some(chunk) = chunks.next().await {
        let bytes = chunk.map_err(|reason| DiscoveryError::Malformed {
            provider: provider.into(),
            reason: format!("body stream failed: {reason}").into(),
        })?;
        if body.len() + bytes.len() > MAX_DISCOVERY_BODY {
            return Err(DiscoveryError::Malformed {
                provider: provider.into(),
                reason: format!("listing is larger than {MAX_DISCOVERY_BODY} bytes").into(),
            });
        }
        body.extend_from_slice(&bytes);
    }

    let listing: serde_json::Value =
        serde_json::from_slice(&body).map_err(|error| DiscoveryError::Malformed {
            provider: provider.into(),
            reason: format!("listing is not json: {error}").into(),
        })?;
    let entries = listing
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| DiscoveryError::Malformed {
            provider: provider.into(),
            reason: "listing has no `data` array".into(),
        })?;

    let mut ids: Vec<SmolStr> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if !ids.iter().any(|seen| seen == id) {
            ids.push(id.into());
        }
    }
    Ok(ids)
}

/// A transport failure is the provider being unreachable — except when the
/// transport already read the status, which is where a gateway's 401 shows up
/// as a fatal error rather than a response.
fn unreachable_error(provider: &str, error: &TransportError) -> DiscoveryError {
    match error {
        TransportError::Retryable {
            status: Some(status),
            ..
        }
        | TransportError::Fatal {
            status: Some(status),
            ..
        } => match status {
            401 => DiscoveryError::Unauthorized {
                provider: provider.into(),
                status: *status,
            },
            403 => DiscoveryError::Forbidden {
                provider: provider.into(),
                status: *status,
            },
            _ => DiscoveryError::Status {
                provider: provider.into(),
                status: *status,
            },
        },
        other => DiscoveryError::Unreachable {
            provider: provider.into(),
            message: other.to_string().into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockFetch, MockFetchResponse};

    const BASE: &str = "https://api.example.invalid/v1";

    fn listing(body: &str) -> MockFetch {
        MockFetch::new(vec![Ok(MockFetchResponse::sse(vec![body.to_owned()]))])
    }

    fn status_only(status: u16) -> MockFetch {
        MockFetch::new(vec![Ok(MockFetchResponse::sse(vec![
            r#"{"error":"nope"}"#.to_owned(),
        ])
        .with_status(status))])
    }

    #[tokio::test]
    async fn a_listing_comes_back_as_model_ids() {
        let fetch =
            listing(r#"{"object":"list","data":[{"id":"gpt-test"},{"id":" claude-test "}]}"#);
        let ids = list_models("openai", BASE, Some("sk-test"), &fetch)
            .await
            .unwrap_or_else(|error| panic!("listing: {error}"));
        assert_eq!(ids, ["gpt-test", "claude-test"]);

        let requests = fetch.requests.lock().expect("requests");
        assert_eq!(requests[0].method.as_str(), "GET");
        assert_eq!(
            requests[0].url.as_str(),
            "https://api.example.invalid/v1/models"
        );
        let auth = requests[0]
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .expect("auth header");
        assert_eq!(auth.1, "Bearer sk-test");
    }

    #[tokio::test]
    async fn a_keyless_provider_sends_no_authorization() {
        let fetch = listing(r#"{"object":"list","data":[{"id":"qwen3:8b"}]}"#);
        let ids = list_models("ollama", "http://127.0.0.1:11434/v1", None, &fetch)
            .await
            .unwrap_or_else(|error| panic!("listing: {error}"));
        assert_eq!(ids, ["qwen3:8b"]);
        let requests = fetch.requests.lock().expect("requests");
        assert!(
            !requests[0]
                .headers
                .iter()
                .any(|(name, _)| name == "authorization"),
            "{:?}",
            requests[0].headers
        );
    }

    #[tokio::test]
    async fn a_rejected_key_is_an_authentication_error_naming_the_provider() {
        let error = list_models("openai", BASE, Some("sk-test"), &status_only(401))
            .await
            .expect_err("401 must not look like an empty catalog");
        assert_eq!(
            error,
            DiscoveryError::Unauthorized {
                provider: "openai".into(),
                status: 401
            }
        );
        assert!(error.is_auth());
        assert_eq!(error.provider(), "openai");
        assert_eq!(error.status(), Some(401));
        let rendered = error.to_string();
        assert!(rendered.contains("authentication failed"), "{rendered}");
        assert!(rendered.contains("openai"), "{rendered}");
        assert!(rendered.contains("401"), "{rendered}");
        assert!(rendered.contains("titi --set-key"), "{rendered}");
        // The key is a secret even in a message the user asked for.
        assert!(!rendered.contains("sk-test"), "{rendered}");
    }

    #[tokio::test]
    async fn a_forbidden_key_is_its_own_error() {
        let error = list_models("openrouter", BASE, Some("sk-test"), &status_only(403))
            .await
            .expect_err("403 must not look like an empty catalog");
        assert_eq!(
            error,
            DiscoveryError::Forbidden {
                provider: "openrouter".into(),
                status: 403
            }
        );
        assert!(error.is_auth());
        let rendered = error.to_string();
        assert!(rendered.contains("access forbidden"), "{rendered}");
        assert!(rendered.contains("openrouter"), "{rendered}");
        assert!(rendered.contains("403"), "{rendered}");
        assert!(!rendered.contains("sk-test"), "{rendered}");
    }

    #[tokio::test]
    async fn another_status_is_not_an_auth_error() {
        let error = list_models("groq", BASE, Some("sk-test"), &status_only(500))
            .await
            .expect_err("500 is still a failed listing");
        assert_eq!(
            error,
            DiscoveryError::Status {
                provider: "groq".into(),
                status: 500
            }
        );
        assert!(!error.is_auth());
        let rendered = error.to_string();
        assert!(rendered.contains("HTTP 500"), "{rendered}");
        assert!(!rendered.contains("--set-key"), "{rendered}");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_neither_auth_nor_status() {
        let fetch = MockFetch::new(vec![Err(TransportError::Retryable {
            status: None,
            message: "request failed: connection refused".into(),
        })]);
        let error = list_models("ollama", "http://127.0.0.1:11434/v1", None, &fetch)
            .await
            .expect_err("a refused connection is a failure, not an empty catalog");
        assert!(
            matches!(error, DiscoveryError::Unreachable { .. }),
            "{error}"
        );
        assert!(!error.is_auth());
        assert_eq!(error.status(), None);
        assert!(error.to_string().contains("connection refused"), "{error}");
    }

    /// Some gateways fail the request before the response is handed back; the
    /// status is inside the transport error and still means "fix the key".
    #[tokio::test]
    async fn a_transport_error_carrying_401_is_still_an_auth_error() {
        let fetch = MockFetch::new(vec![Err(TransportError::Fatal {
            status: Some(401),
            message: "unauthorized".into(),
        })]);
        let error = list_models("openai", BASE, Some("sk-test"), &fetch)
            .await
            .expect_err("401 must survive the transport error");
        assert_eq!(
            error,
            DiscoveryError::Unauthorized {
                provider: "openai".into(),
                status: 401
            }
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_listing_is_malformed_not_auth() {
        let fetch = listing(r#"{"object":"list"}"#);
        let error = list_models("openai", BASE, Some("sk-test"), &fetch)
            .await
            .expect_err("a listing without `data` is unusable");
        assert!(matches!(error, DiscoveryError::Malformed { .. }), "{error}");
        assert!(!error.is_auth());
        assert!(error.to_string().contains("no `data` array"), "{error}");
    }

    #[tokio::test]
    async fn an_oversized_listing_is_refused() {
        let flood = format!(
            r#"{{"object":"list","data":[{{"id":"{}"}}]}}"#,
            "a".repeat(MAX_DISCOVERY_BODY + 1)
        );
        let error = list_models("openai", BASE, Some("sk-test"), &listing(&flood))
            .await
            .expect_err("an unbounded body must not be buffered");
        assert!(matches!(error, DiscoveryError::Malformed { .. }), "{error}");
        assert!(error.to_string().contains("larger than"), "{error}");
    }
}
