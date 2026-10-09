//! Abstract HTTP layer so every transport and test runs over an injectable
//! byte-stream source; `reqwest` lives only behind [`HttpFetch`].

use futures::StreamExt;
use futures::future::BoxFuture;
use smol_str::SmolStr;

use crate::transport::{TransportError, WatchdogConfig};

/// A fully-shaped HTTP request ready to send.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: SmolStr,
    pub url: SmolStr,
    pub headers: Vec<(SmolStr, SmolStr)>,
    pub body: Option<Vec<u8>>,
}

/// Chunk of a response body.
pub type BodyChunk = Vec<u8>;

/// Minimal response surface: status, headers and a byte stream.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(SmolStr, SmolStr)>,
    /// Chunked response body (SSE frames arrive incrementally).
    pub body: std::pin::Pin<Box<dyn futures::Stream<Item = Result<BodyChunk, String>> + Send>>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Transport-agnostic HTTP sender. The only seam where real networking
/// happens; tests substitute [`crate::mock::MockFetch`].
pub trait HttpFetch: Send + Sync {
    fn fetch<'a>(&'a self, req: HttpRequest)
    -> BoxFuture<'a, Result<HttpResponse, TransportError>>;
}

/// Production [`HttpFetch`] backed by `reqwest` with rustls.
pub struct ReqwestFetch {
    client: reqwest::Client,
}

impl ReqwestFetch {
    /// The production client, bounded by the default watchdog timings.
    pub fn new() -> Result<Self, TransportError> {
        Self::with_watchdog(&crate::transport::WatchdogConfig::default())
    }

    /// A client bounded by the same watchdog the stream uses: a connection
    /// that never completes fails at `first_event_timeout`, and a body that
    /// stops mid-read fails at `idle_timeout`. The stream pump applies its own
    /// typed verdict on top; this is the layer that keeps a black-holed
    /// socket from holding the request at all.
    pub fn with_watchdog(watchdog: &WatchdogConfig) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .connect_timeout(watchdog.first_event_timeout)
            .read_timeout(watchdog.idle_timeout)
            .build()
            .map_err(|e| TransportError::Fatal {
                status: None,
                message: format!("client build failed: {e}").into(),
                context_too_long: false,
            })?;
        Ok(Self { client })
    }
}

impl HttpFetch for ReqwestFetch {
    fn fetch<'a>(
        &'a self,
        req: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| {
                TransportError::Fatal {
                    status: None,
                    message: format!("bad method: {e}").into(),
                    context_too_long: false,
                }
            })?;
            let mut builder = self.client.request(method, req.url.as_str());
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            if let Some(body) = req.body {
                builder = builder.body(body);
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| TransportError::Retryable {
                    status: None,
                    message: format!("request failed: {e}").into(),
                    retry_after: None,
                })?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    let v = v.to_str().map(str::to_owned).unwrap_or_default();
                    (SmolStr::from(k.as_str()), SmolStr::from(v))
                })
                .collect();
            let body = resp.bytes_stream().map(|chunk| {
                chunk
                    .map(|b| b.to_vec())
                    .map_err(|e| format!("body read failed: {e}"))
            });
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(body),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: Box::pin(futures::stream::empty()),
        };
        assert_eq!(resp.header("content-type"), Some("text/event-stream"));
        assert_eq!(resp.header("CONTENT-TYPE"), Some("text/event-stream"));
        assert_eq!(resp.header("missing"), None);
    }
}
