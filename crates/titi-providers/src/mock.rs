//! Network-free test doubles: scripted [`HttpFetch`] responses and a
//! [`MockTransport`] emitting canned [`StreamEvent`] sequences.

use futures::future::BoxFuture;
use std::sync::Mutex;

use crate::http::{HttpFetch, HttpRequest, HttpResponse};
use crate::stream::StreamEvent;
use crate::transport::{
    ApiKind, EventStream, RequestCtx, Transport, TransportError, WatchdogConfig, WireRequest,
};

/// Scripted response for one fetch call.
#[derive(Debug, Clone)]
pub struct MockFetchResponse {
    pub status: u16,
    /// SSE/text chunks streamed in order.
    pub chunks: Vec<String>,
    /// Headers the response carries. A `retry-after` on a 429 is the reason
    /// this exists.
    pub headers: Vec<(smol_str::SmolStr, smol_str::SmolStr)>,
}

impl MockFetchResponse {
    pub fn sse(chunks: Vec<String>) -> Self {
        Self {
            status: 200,
            chunks,
            headers: Vec::new(),
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn with_header(
        mut self,
        name: impl Into<smol_str::SmolStr>,
        value: impl Into<smol_str::SmolStr>,
    ) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Deterministic [`HttpFetch`]: pops scripted responses in order, records
/// every request for assertions.
#[derive(Default)]
pub struct MockFetch {
    responses: Mutex<Vec<Result<MockFetchResponse, TransportError>>>,
    pub requests: Mutex<Vec<HttpRequest>>,
}

impl MockFetch {
    pub fn new(responses: Vec<Result<MockFetchResponse, TransportError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn sse(chunks: Vec<String>) -> Self {
        Self::new(vec![Ok(MockFetchResponse::sse(chunks))])
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn last_body(&self) -> Option<serde_json::Value> {
        let requests = self.requests.lock().ok()?;
        let last = requests.last()?;
        let body = last.body.as_ref()?;
        serde_json::from_slice(body).ok()
    }
}

impl HttpFetch for MockFetch {
    fn fetch<'a>(
        &'a self,
        req: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            if let Ok(mut q) = self.requests.lock() {
                q.push(req);
            }
            let next = self.responses.lock().ok().and_then(|mut r| {
                if r.is_empty() {
                    None
                } else {
                    Some(r.remove(0))
                }
            });
            let Some(resp) = next else {
                return Err(TransportError::Fatal {
                    status: None,
                    message: "mock exhausted: no scripted responses".into(),
                    context_too_long: false,
                });
            };
            let resp = resp?;
            let status = resp.status;
            let chunks = resp.chunks.into_iter().map(|c| Ok(c.into_bytes()));
            let mut headers: Vec<(smol_str::SmolStr, smol_str::SmolStr)> =
                vec![("content-type".into(), "text/event-stream".into())];
            headers.extend(resp.headers);
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(futures::stream::iter(chunks)),
            })
        })
    }
}

/// Events for a scripted transport turn.
#[derive(Debug, Clone)]
pub enum MockBody {
    Events(Vec<StreamEvent>),
    Err(TransportError),
}

/// Canned [`Transport`]: hands out scripted bodies per call, counts calls
/// (empty-completion retry tests).
#[derive(Default)]
pub struct MockTransport {
    bodies: Mutex<Vec<MockBody>>,
    requests: Mutex<Vec<WireRequest>>,
    pub calls: std::sync::atomic::AtomicUsize,
}

impl MockTransport {
    pub fn new(bodies: Vec<MockBody>) -> Self {
        Self {
            bodies: Mutex::new(bodies),
            requests: Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<WireRequest> {
        self.requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Transport for MockTransport {
    fn api(&self) -> ApiKind {
        ApiKind::OpenAiCompletions
    }

    fn watchdog(&self) -> WatchdogConfig {
        WatchdogConfig {
            first_event_timeout: std::time::Duration::from_millis(100),
            idle_timeout: std::time::Duration::from_millis(100),
            ..WatchdogConfig::default()
        }
    }

    async fn stream(
        &self,
        req: WireRequest,
        _ctx: RequestCtx,
    ) -> Result<EventStream, TransportError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(req);
        }
        let next = self.bodies.lock().ok().and_then(|mut b| {
            if b.is_empty() {
                None
            } else {
                Some(b.remove(0))
            }
        });
        match next {
            None => Err(TransportError::Fatal {
                status: None,
                message: "mock exhausted".into(),
                context_too_long: false,
            }),
            Some(MockBody::Err(e)) => Err(e),
            Some(MockBody::Events(events)) => Ok(Box::pin(futures::stream::iter(events))),
        }
    }
}
