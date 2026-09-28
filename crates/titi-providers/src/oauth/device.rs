//! The Codex device-code flow: a headless alternative to the loopback
//! callback, used when port 1455 is unavailable or the browser callback is
//! blocked.
//!
//! Endpoints and timings are ported from omp's
//! `@oh-my-pi/pi-ai` `src/registry/oauth/openai-codex.ts` (MIT, Stencil Labs,
//! Inc.).

use std::time::Duration;

use serde_json::{Value, json};

use crate::http::{HttpFetch, HttpRequest};
use crate::oauth::{OAuthError, OAuthProvider, OAuthTokens, OAuthUi};

const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

const DEVICE_REQUEST_TIMEOUT_SECS: u64 = 15;
/// Upper bound on polling so a stuck authorization cannot hang a login.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Added to the server's interval so the first poll is never early.
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// RFC 8628 `slow_down`: the client must back off by five seconds.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Signs in through the device grant. Publishes the user code through
/// [`OAuthUi::on_auth`] and polls until the user authorizes or the flow times
/// out.
pub async fn device_login(
    provider: &OAuthProvider,
    fetch: &dyn HttpFetch,
    ui: &dyn OAuthUi,
) -> Result<OAuthTokens, OAuthError> {
    if !provider.supports_device {
        return Err(OAuthError::Config {
            message: format!("{} does not support the device flow", provider.id),
        });
    }
    ui.on_progress("Initiating device authorization…");
    let (status, text) = post_json(
        DEVICE_USERCODE_URL,
        &json!({ "client_id": provider.client_id }),
        fetch,
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(device_status_error(status));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| OAuthError::protocol("device authorization returned invalid JSON"))?;
    let device_auth_id = string_field(&value, "device_auth_id")
        .ok_or_else(|| OAuthError::protocol("device authorization is missing device_auth_id"))?;
    let user_code = string_field(&value, "user_code")
        .ok_or_else(|| OAuthError::protocol("device authorization is missing user_code"))?;
    let mut interval = poll_interval(value.get("interval"));

    ui.on_auth(DEVICE_AUTH_URL, &format!("Enter code: {user_code}"));
    ui.on_progress(&format!(
        "Waiting for browser authorization (code: {user_code})…"
    ));

    let deadline = tokio::time::Instant::now() + DEVICE_TIMEOUT;
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(OAuthError::Timeout {
                seconds: DEVICE_TIMEOUT.as_secs(),
            });
        }
        let (status, text) = post_json(
            DEVICE_TOKEN_URL,
            &json!({ "device_auth_id": device_auth_id, "user_code": user_code }),
            fetch,
        )
        .await?;
        match status {
            200..=299 => {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|_| OAuthError::protocol("device token response is not JSON"))?;
                let code = string_field(&value, "authorization_code").ok_or_else(|| {
                    OAuthError::protocol("device token response is missing authorization_code")
                })?;
                let verifier = string_field(&value, "code_verifier").ok_or_else(|| {
                    OAuthError::protocol("device token response is missing code_verifier")
                })?;
                ui.on_progress("Exchanging the authorization code…");
                return super::exchange_code(
                    provider,
                    fetch,
                    &code,
                    DEVICE_REDIRECT_URI,
                    &verifier,
                    "",
                )
                .await;
            }
            // The user has not finished in the browser yet.
            403 | 404 => continue,
            _ => {
                let detail = super::describe_error(&text);
                if detail.contains("slow_down") {
                    interval += SLOW_DOWN_STEP;
                    continue;
                }
                if detail.contains("access_denied") {
                    return Err(OAuthError::Denied {
                        message: if detail.is_empty() {
                            "access_denied".to_owned()
                        } else {
                            detail
                        },
                    });
                }
                return Err(device_status_error(status));
            }
        }
    }
}

/// The server's `interval` (seconds, number or string) padded by the safety
/// margin, never shorter than a second.
fn poll_interval(value: Option<&Value>) -> Duration {
    let seconds = match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    // `from_secs_f64` panics on a negative, NaN or overflowing input, so the
    // value is normalised before it is used.
    let seconds = if seconds.is_finite() && seconds > 0.0 && seconds < 3600.0 {
        seconds
    } else {
        DEFAULT_POLL_INTERVAL.as_secs_f64()
    };
    (Duration::from_secs_f64(seconds) + POLL_SAFETY_MARGIN).max(MIN_POLL_INTERVAL)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn device_status_error(status: u16) -> OAuthError {
    match status {
        400 | 401 | 403 => OAuthError::TokenRejected {
            status,
            message: format!("HTTP {status}"),
        },
        _ => OAuthError::Transport {
            message: format!("device endpoint returned HTTP {status}"),
        },
    }
}

async fn post_json(
    url: &str,
    body: &Value,
    fetch: &dyn HttpFetch,
) -> Result<(u16, String), OAuthError> {
    let encoded = serde_json::to_vec(body)
        .map_err(|e| OAuthError::protocol(format!("cannot encode the device request: {e}")))?;
    let request = HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: Some(encoded),
    };
    let timeout = Duration::from_secs(DEVICE_REQUEST_TIMEOUT_SECS);
    let response = match tokio::time::timeout(timeout, fetch.fetch(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(super::map_transport_error(error)),
        Err(_) => {
            return Err(OAuthError::Timeout {
                seconds: DEVICE_REQUEST_TIMEOUT_SECS,
            });
        }
    };
    let status = response.status;
    let text = super::read_body(response).await?;
    Ok((status, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockFetch, MockFetchResponse};
    use crate::oauth::provider;
    use crate::transport::TransportError;
    use std::sync::Mutex;

    fn ok(body: Value) -> Result<MockFetchResponse, TransportError> {
        Ok(MockFetchResponse {
            status: 200,
            chunks: vec![body.to_string()],
        })
    }

    fn status(code: u16, body: Value) -> Result<MockFetchResponse, TransportError> {
        Ok(MockFetchResponse {
            status: code,
            chunks: vec![body.to_string()],
        })
    }

    #[derive(Default)]
    struct TestUi {
        auth: Mutex<Vec<(String, String)>>,
        progress: Mutex<Vec<String>>,
    }

    impl OAuthUi for TestUi {
        fn on_auth(&self, url: &str, instructions: &str) {
            self.auth
                .lock()
                .expect("auth")
                .push((url.to_owned(), instructions.to_owned()));
        }
        fn on_progress(&self, message: &str) {
            self.progress
                .lock()
                .expect("progress")
                .push(message.to_owned());
        }
        fn manual_code(&self) -> Option<String> {
            None
        }
    }

    fn codex() -> &'static OAuthProvider {
        provider::find("openai-codex").expect("codex")
    }

    #[test]
    fn the_poll_interval_is_padded_and_normalised() {
        assert_eq!(poll_interval(Some(&json!(1))), Duration::from_secs(4));
        assert_eq!(poll_interval(Some(&json!("2"))), Duration::from_secs(5));
        assert_eq!(poll_interval(Some(&json!(0))), Duration::from_secs(8));
        assert_eq!(poll_interval(Some(&json!(-3))), Duration::from_secs(8));
        assert_eq!(poll_interval(None), Duration::from_secs(8));
        assert!(Duration::from_secs(4) + SLOW_DOWN_STEP > Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn two_pending_polls_then_a_success_yield_credentials() {
        let fetch = MockFetch::new(vec![
            ok(json!({
                "device_auth_id": "dev-1",
                "user_code": "CODE-1",
                "interval": 1,
            })),
            status(403, json!({})),
            status(404, json!({})),
            ok(json!({
                "authorization_code": "ac-1",
                "code_verifier": "cv-1",
            })),
            ok(json!({
                "access_token": "sk-test-access",
                "refresh_token": "sk-test-refresh",
                "expires_in": 3600,
            })),
        ]);
        let ui = TestUi::default();
        let tokens = device_login(codex(), &fetch, &ui).await.expect("login");
        assert_eq!(tokens.access, "sk-test-access");
        assert_eq!(tokens.refresh.as_deref(), Some("sk-test-refresh"));

        let auth = ui.auth.lock().expect("auth");
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].0, "https://auth.openai.com/codex/device");
        assert_eq!(auth[0].1, "Enter code: CODE-1");

        // The exchange is a form POST with the device redirect URI.
        let exchange = fetch.requests.lock().expect("requests")[4].clone();
        assert_eq!(exchange.url, "https://auth.openai.com/oauth/token");
        let body = String::from_utf8(exchange.body.clone().expect("body")).expect("utf-8");
        assert!(body.contains("code=ac-1"), "{body}");
        assert!(body.contains("code_verifier=cv-1"), "{body}");
        assert!(
            body.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"),
            "{body}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_down_widens_the_polling_interval() {
        let fetch = MockFetch::new(vec![
            ok(json!({
                "device_auth_id": "dev-2",
                "user_code": "CODE-2",
                "interval": 1,
            })),
            status(400, json!({ "error": "slow_down" })),
            ok(json!({
                "authorization_code": "ac-2",
                "code_verifier": "cv-2",
            })),
            ok(json!({
                "access_token": "sk-test-access",
                "refresh_token": "sk-test-refresh",
                "expires_in": 3600,
            })),
        ]);
        let ui = TestUi::default();
        let start = tokio::time::Instant::now();
        let tokens = device_login(codex(), &fetch, &ui).await.expect("login");
        assert_eq!(tokens.access, "sk-test-access");
        // 4 s (first interval) + 9 s (widened) — without the back-off the two
        // sleeps would total 8 s.
        assert!(
            start.elapsed() >= Duration::from_secs(13),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_access_denied_poll_fails_the_login() {
        let fetch = MockFetch::new(vec![
            ok(json!({
                "device_auth_id": "dev-3",
                "user_code": "CODE-3",
                "interval": 1,
            })),
            status(400, json!({ "error": "access_denied" })),
        ]);
        let ui = TestUi::default();
        let error = device_login(codex(), &fetch, &ui)
            .await
            .expect_err("denied");
        assert!(matches!(error, OAuthError::Denied { .. }), "{error:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsupported_provider_is_refused_without_a_request() {
        let fetch = MockFetch::default();
        let ui = TestUi::default();
        let error = device_login(provider::find("anthropic").expect("anthropic"), &fetch, &ui)
            .await
            .expect_err("unsupported");
        assert!(matches!(error, OAuthError::Config { .. }), "{error:?}");
        assert_eq!(fetch.request_count(), 0);
    }
}
