//! OAuth login for subscription-backed providers (Claude Pro/Max, ChatGPT
//! Plus/Pro): authorization code + PKCE over a loopback callback, a parallel
//! manual-paste path, the Codex device flow, token refresh and the identity
//! slice each provider reports.
//!
//! Flow reference: omp (`@oh-my-pi/pi-ai`, MIT, Stencil Labs, Inc.), described
//! in `docs/research/providers-streaming/oauth-login.md`; the provider
//! constants live in [`provider`].

pub mod callback;
pub mod device;
pub mod provider;

pub(crate) mod encode;
mod pkce;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde_json::{Map, Value};
use smol_str::SmolStr;
use titi_secrets::store::{Credential, OAuthRecord};

use crate::creds::mask_secret;
use crate::http::{HttpFetch, HttpRequest, HttpResponse};
use crate::transport::TransportError;

pub use callback::parse_callback_input;
pub use provider::{Body, CallbackSpec, IdentitySource, OAuthProvider, TokenSpec, builtin, find};

/// Anthropic's post-exchange identity endpoint and the model its CLI reports.
const ANTHROPIC_BOOTSTRAP_URL: &str =
    "https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli&model=claude-opus-4-8";
const ANTHROPIC_BOOTSTRAP_TIMEOUT_SECS: u64 = 30;
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

/// Refuse an absurd response before it is buffered.
const MAX_TOKEN_BODY: usize = 256 * 1024;

/// Every way an OAuth login can fail. No variant carries token material.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("oauth configuration error: {message}")]
    Config { message: String },
    #[error("oauth entropy source failed: {message}")]
    Entropy { message: String },
    #[error("oauth callback server failed: {message}")]
    Server { message: String },
    #[error("oauth request timed out after {seconds} s")]
    Timeout { seconds: u64 },
    #[error("oauth authorization was denied: {message}")]
    Denied { message: String },
    #[error("oauth callback carried no authorization code")]
    MissingCode,
    #[error("oauth state mismatch - possible CSRF attack")]
    StateMismatch,
    /// The provider answered a token request with 400/401/403: the credential
    /// is dead and retrying cannot help.
    #[error("oauth token endpoint rejected the request: {message}")]
    TokenRejected { status: u16, message: String },
    /// The request never produced a verdict (network, DNS, 5xx).
    #[error("oauth transport failure: {message}")]
    Transport { message: String },
    #[error("oauth response was malformed: {message}")]
    Protocol { message: String },
}

impl OAuthError {
    /// Definitive failure: a caller may quarantine the stored row.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Config { .. }
                | Self::Denied { .. }
                | Self::MissingCode
                | Self::StateMismatch
                | Self::TokenRejected { .. }
                | Self::Protocol { .. }
        )
    }

    /// Transient failure: the stored row must be kept and retried later.
    pub fn is_transport(&self) -> bool {
        matches!(
            self,
            Self::Entropy { .. }
                | Self::Server { .. }
                | Self::Timeout { .. }
                | Self::Transport { .. }
        )
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}

/// The credential material one successful login (or refresh) yields.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthTokens {
    pub access: String,
    pub refresh: Option<String>,
    /// Unix seconds at which `access` stops being valid.
    pub expires_at: Option<i64>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access", &mask_secret(&self.access))
            .field("refresh", &self.refresh.as_deref().map(mask_secret))
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("org_id", &self.org_id)
            .field("org_name", &self.org_name)
            .finish()
    }
}

/// The surface a login drives: print the URL, show progress, poll for a
/// pasted code. `manual_code` must never block.
pub trait OAuthUi: Send + Sync {
    fn on_auth(&self, url: &str, instructions: &str);
    fn on_progress(&self, message: &str);
    fn manual_code(&self) -> Option<String>;
}

/// The part of a login that does not need the HTTP client: PKCE material, the
/// bound callback server and the authorize URL.
struct SessionCore {
    provider: OAuthProvider,
    verifier: String,
    state: String,
    server: callback::CallbackServer,
    url: String,
    instructions: String,
}

impl SessionCore {
    async fn start(provider: &OAuthProvider) -> Result<Self, OAuthError> {
        let verifier = pkce::verifier()?;
        let state = pkce::state()?;
        let server = callback::CallbackServer::bind(&provider.callback, &state).await?;
        let challenge = pkce::challenge(&verifier);
        let url = build_authorize_url(provider, server.redirect_uri(), &state, &challenge);
        Ok(Self {
            provider: *provider,
            verifier,
            state,
            server,
            url,
            instructions: provider.instructions.to_owned(),
        })
    }

    fn redirect_uri(&self) -> &str {
        self.server.redirect_uri()
    }

    async fn wait_for_code(
        &mut self,
        manual: impl FnMut() -> Option<String>,
    ) -> Result<String, OAuthError> {
        self.server.wait_for_code(manual).await
    }
}

/// One in-flight authorization: the URL to publish, the callback that
/// catches the redirect and the exchange that turns the code into tokens.
pub struct LoginSession {
    core: SessionCore,
    fetch: Arc<dyn HttpFetch>,
}

impl LoginSession {
    pub async fn start(
        provider: &OAuthProvider,
        fetch: Arc<dyn HttpFetch>,
    ) -> Result<Self, OAuthError> {
        Ok(Self {
            core: SessionCore::start(provider).await?,
            fetch,
        })
    }

    /// The authorization URL to open (or show) for the user.
    pub fn url(&self) -> &str {
        &self.core.url
    }

    pub fn instructions(&self) -> &str {
        &self.core.instructions
    }

    /// The redirect URI that was actually registered with the provider.
    pub fn redirect_uri(&self) -> &str {
        self.core.redirect_uri()
    }

    /// Waits for the code: the callback is polled first, then `manual`, so a
    /// TUI can drive it once per rendered frame.
    pub async fn wait_for_code(
        &mut self,
        manual: impl FnMut() -> Option<String>,
    ) -> Result<String, OAuthError> {
        self.core.wait_for_code(manual).await
    }

    pub async fn finish(self, code: String) -> Result<OAuthTokens, OAuthError> {
        exchange_code(
            &self.core.provider,
            self.fetch.as_ref(),
            &code,
            self.core.redirect_uri(),
            &self.core.verifier,
            &self.core.state,
        )
        .await
    }
}

/// Full authorization-code login against a provider: publishes the URL to
/// `ui`, waits for the callback or a pasted code, then exchanges it.
pub async fn login(
    provider: &OAuthProvider,
    fetch: &dyn HttpFetch,
    ui: &dyn OAuthUi,
) -> Result<OAuthTokens, OAuthError> {
    let mut session = SessionCore::start(provider).await?;
    ui.on_auth(&session.url, &session.instructions);
    ui.on_progress("Waiting for authorization…");
    let code = session.wait_for_code(|| ui.manual_code()).await?;
    ui.on_progress("Exchanging the authorization code…");
    exchange_code(
        &session.provider,
        fetch,
        &code,
        session.redirect_uri(),
        &session.verifier,
        &session.state,
    )
    .await
}

/// Headless login through the provider's device-code grant.
pub async fn login_device(
    provider: &OAuthProvider,
    fetch: &dyn HttpFetch,
    ui: &dyn OAuthUi,
) -> Result<OAuthTokens, OAuthError> {
    device::device_login(provider, fetch, ui).await
}

/// Exchanges the stored refresh token for a new access token. The provider is
/// told nothing about the organization the credential was scoped to, so any
/// org the response omits is taken from `current`.
pub async fn refresh(
    provider: &OAuthProvider,
    current: &OAuthTokens,
    fetch: &dyn HttpFetch,
) -> Result<OAuthTokens, OAuthError> {
    let Some(refresh_token) = current.refresh.as_deref().filter(|token| !token.is_empty()) else {
        return Err(OAuthError::Config {
            message: format!("{} has no refresh token stored", provider.id),
        });
    };
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", provider.client_id),
        ("refresh_token", refresh_token),
    ];
    let body = post_token(provider, fetch, &params, provider.token.refresh_headers).await?;
    let mut tokens = map_tokens(provider, &body, Some(current))?;
    apply_identity(
        provider,
        fetch,
        &body,
        &mut tokens,
        Phase::Refresh,
        Some(current),
    )
    .await;
    Ok(tokens)
}

/// Projects a stored row back into the flow's token type.
pub fn from_stored(credential: &Credential) -> OAuthTokens {
    OAuthTokens {
        access: credential.token.clone(),
        refresh: credential.refresh_token.clone(),
        expires_at: credential.expires_at,
        account_id: credential.account_id.clone(),
        email: credential.email.clone(),
        org_id: credential.org_id.clone(),
        org_name: credential.org_name.clone(),
    }
}

/// Projects freshly exchanged tokens onto the store's record shape.
pub fn to_record<'a>(
    tokens: &'a OAuthTokens,
    provider: &'a str,
    label: &'a str,
) -> OAuthRecord<'a> {
    OAuthRecord {
        provider,
        label,
        access: &tokens.access,
        refresh: tokens.refresh.as_deref(),
        expires_at: tokens.expires_at,
        account_id: tokens.account_id.as_deref(),
        email: tokens.email.as_deref(),
        org_id: tokens.org_id.as_deref(),
        org_name: tokens.org_name.as_deref(),
        // The record is produced by the login it describes; a refresh caller
        // replaces this with the timestamp already stored.
        authorized_at: Some(now_secs()),
    }
}

/// The standard authorize parameters plus the descriptor's extras, in the
/// order omp emits them.
fn build_authorize_url(
    provider: &OAuthProvider,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let scope = provider.scopes.join(" ");
    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", provider.client_id),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", &scope),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    params.extend_from_slice(provider.authorize_params);
    encode::query_append(provider.authorize_url, &params)
}

async fn exchange_code(
    provider: &OAuthProvider,
    fetch: &dyn HttpFetch,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    state: &str,
) -> Result<OAuthTokens, OAuthError> {
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("client_id", provider.client_id),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    // Descriptor params are templates: Anthropic echoes the CSRF state here.
    let resolved: Vec<(String, String)> = provider
        .token
        .params
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.replace("{state}", state)))
        .collect();
    for (key, value) in &resolved {
        params.push((key.as_str(), value.as_str()));
    }
    let body = post_token(provider, fetch, &params, &[]).await?;
    let mut tokens = map_tokens(provider, &body, None)?;
    apply_identity(provider, fetch, &body, &mut tokens, Phase::Login, None).await;
    Ok(tokens)
}

/// Fills the `{sdk}` slot a descriptor may leave in a header value: omp's
/// Anthropic rule writes the Claude Code SDK version into the token request's
/// user agent (`pi-catalog/src/compat/rules/auth/anthropic.kdl`).
fn resolve_header_value(value: &str) -> SmolStr {
    match value.split_once("{sdk}") {
        Some((head, tail)) => SmolStr::from(format!(
            "{head}{}{tail}",
            crate::wire::CLAUDE_CODE_SDK_VERSION
        )),
        None => SmolStr::from(value),
    }
}

/// One token-endpoint POST: method, encoding, timeout, status mapping and
/// JSON parse, shared by exchange, device exchange and refresh.
async fn post_token(
    provider: &OAuthProvider,
    fetch: &dyn HttpFetch,
    params: &[(&str, &str)],
    extra_headers: &[(&str, &str)],
) -> Result<Value, OAuthError> {
    let (content_type, body) = match provider.token.body {
        Body::Json => {
            let mut map = Map::new();
            for (key, value) in params {
                map.insert((*key).to_owned(), Value::String((*value).to_owned()));
            }
            let encoded = serde_json::to_vec(&Value::Object(map)).map_err(|e| {
                OAuthError::protocol(format!("cannot encode the token request: {e}"))
            })?;
            ("application/json", encoded)
        }
        Body::Form => (
            "application/x-www-form-urlencoded",
            encode::form_encode(params).into_bytes(),
        ),
    };
    let mut headers: Vec<(SmolStr, SmolStr)> = vec![("content-type".into(), content_type.into())];
    for (key, value) in extra_headers {
        headers.push(((*key).into(), resolve_header_value(value)));
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: provider.token.url.into(),
        headers,
        body: Some(body),
    };
    let timeout = Duration::from_secs(provider.token.timeout_secs);
    let response = match tokio::time::timeout(timeout, fetch.fetch(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(map_transport_error(error)),
        Err(_) => {
            return Err(OAuthError::Timeout {
                seconds: provider.token.timeout_secs,
            });
        }
    };
    let status = response.status;
    let text = read_body(response).await?;
    if !(200..300).contains(&status) {
        return Err(token_status_error(status, &text));
    }
    serde_json::from_str(&text).map_err(|_| {
        OAuthError::protocol(format!(
            "{} returned invalid JSON from the token endpoint",
            provider.id
        ))
    })
}

/// Maps a response onto tokens. `previous` supplies the refresh token a
/// rotating grant did not return and the org the credential is scoped to.
fn map_tokens(
    provider: &OAuthProvider,
    body: &Value,
    previous: Option<&OAuthTokens>,
) -> Result<OAuthTokens, OAuthError> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            OAuthError::protocol(format!(
                "{} token response is missing access_token",
                provider.id
            ))
        })?
        .to_owned();
    let expires_in = body
        .get("expires_in")
        .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|f| f as i64)));
    Ok(OAuthTokens {
        access,
        refresh: body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .or_else(|| previous.and_then(|tokens| tokens.refresh.clone())),
        expires_at: expires_in.map(|seconds| now_secs() + seconds - provider.refresh_skew_secs),
        account_id: None,
        email: None,
        org_id: previous.and_then(|tokens| tokens.org_id.clone()),
        org_name: previous.and_then(|tokens| tokens.org_name.clone()),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Login,
    Refresh,
}

/// Fills the identity slice. A failure here never fails the login: the
/// credential is valid with or without its account metadata.
async fn apply_identity(
    provider: &OAuthProvider,
    fetch: &dyn HttpFetch,
    body: &Value,
    tokens: &mut OAuthTokens,
    phase: Phase,
    previous: Option<&OAuthTokens>,
) {
    match provider.identity {
        IdentitySource::AnthropicBootstrap => {
            if let Some(account) = body.get("account") {
                fill(
                    &mut tokens.account_id,
                    account.get("uuid").and_then(Value::as_str),
                );
                fill(
                    &mut tokens.email,
                    account.get("email_address").and_then(Value::as_str),
                );
            }
            if let Some(org) = body.get("organization") {
                fill(&mut tokens.org_id, org.get("uuid").and_then(Value::as_str));
                fill(
                    &mut tokens.org_name,
                    org.get("name").and_then(Value::as_str),
                );
            }
            // The org is fixed at login; a refresh must not re-key the row.
            let org_satisfied = phase == Phase::Refresh || tokens.org_id.is_some();
            let identity_complete =
                tokens.account_id.is_some() && tokens.email.is_some() && org_satisfied;
            let bootstrap = if identity_complete {
                None
            } else {
                anthropic_bootstrap(&tokens.access, fetch).await.ok()
            };
            if let Some(identity) = bootstrap {
                fill(&mut tokens.account_id, identity.account_id.as_deref());
                fill(&mut tokens.email, identity.email.as_deref());
                if phase == Phase::Login {
                    fill(&mut tokens.org_id, identity.org_id.as_deref());
                    fill(&mut tokens.org_name, identity.org_name.as_deref());
                }
            }
        }
        IdentitySource::JwtClaims => {
            if let Some(claims) = jwt_identity(&tokens.access) {
                fill(&mut tokens.account_id, claims.account_id.as_deref());
                fill(&mut tokens.email, claims.email.as_deref());
                if phase == Phase::Login {
                    if let Some(account) = tokens.account_id.clone() {
                        tokens.org_id = Some(account);
                    }
                    fill(&mut tokens.org_name, claims.plan.as_deref());
                }
            }
        }
    }
    if phase == Phase::Refresh
        && let Some(previous) = previous
    {
        fill(&mut tokens.account_id, previous.account_id.as_deref());
        fill(&mut tokens.email, previous.email.as_deref());
        fill(&mut tokens.org_id, previous.org_id.as_deref());
        fill(&mut tokens.org_name, previous.org_name.as_deref());
    }
}

fn fill(target: &mut Option<String>, value: Option<&str>) {
    if target.is_none()
        && let Some(value) = value.filter(|value| !value.is_empty())
    {
        *target = Some(value.to_owned());
    }
}

struct BootstrapIdentity {
    account_id: Option<String>,
    email: Option<String>,
    org_id: Option<String>,
    org_name: Option<String>,
}

async fn anthropic_bootstrap(
    access_token: &str,
    fetch: &dyn HttpFetch,
) -> Result<BootstrapIdentity, OAuthError> {
    let request = HttpRequest {
        method: "GET".into(),
        url: ANTHROPIC_BOOTSTRAP_URL.into(),
        headers: vec![
            (
                "authorization".into(),
                format!("Bearer {access_token}").into(),
            ),
            ("accept".into(), "application/json, text/plain, */*".into()),
            ("anthropic-beta".into(), ANTHROPIC_BETA.into()),
        ],
        body: None,
    };
    let response = match tokio::time::timeout(
        Duration::from_secs(ANTHROPIC_BOOTSTRAP_TIMEOUT_SECS),
        fetch.fetch(request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(map_transport_error(error)),
        Err(_) => {
            return Err(OAuthError::Timeout {
                seconds: ANTHROPIC_BOOTSTRAP_TIMEOUT_SECS,
            });
        }
    };
    if !(200..300).contains(&response.status) {
        return Err(OAuthError::protocol(
            "anthropic bootstrap rejected the request",
        ));
    }
    let text = read_body(response).await?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| OAuthError::protocol("anthropic bootstrap returned invalid JSON"))?;
    let account = value.get("oauth_account");
    let field = |name: &str| {
        account
            .and_then(|account| account.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(BootstrapIdentity {
        account_id: field("account_uuid"),
        email: field("account_email"),
        org_id: field("organization_uuid"),
        org_name: field("organization_name"),
    })
}

struct JwtIdentity {
    account_id: Option<String>,
    email: Option<String>,
    plan: Option<String>,
}

/// Reads the ChatGPT identity claims out of the access token. The token is
/// never logged; only these three claims leave this function.
fn jwt_identity(access_token: &str) -> Option<JwtIdentity> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = encode::base64url_decode(payload.trim_end_matches('=')).ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let auth = value.get("https://api.openai.com/auth");
    let profile = value.get("https://api.openai.com/profile");
    let account_id = auth
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let email = profile
        .and_then(|profile| profile.get("email"))
        .and_then(Value::as_str)
        .map(|email| email.trim().to_lowercase())
        .filter(|email| !email.is_empty());
    let plan = auth
        .and_then(|auth| auth.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(|plan| plan.trim().to_lowercase())
        .filter(|plan| !plan.is_empty());
    Some(JwtIdentity {
        account_id,
        email,
        plan,
    })
}

async fn read_body(response: HttpResponse) -> Result<String, OAuthError> {
    let mut body = response.body;
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| OAuthError::Transport {
            message: format!("oauth response body failed: {e}"),
        })?;
        if bytes.len() + chunk.len() > MAX_TOKEN_BODY {
            return Err(OAuthError::protocol("oauth response body is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| OAuthError::protocol("oauth response body is not utf-8"))
}

fn map_transport_error(error: TransportError) -> OAuthError {
    match error {
        TransportError::Fatal {
            status: Some(status),
            message,
        } if matches!(status, 400 | 401 | 403) => OAuthError::TokenRejected {
            status,
            message: format!("HTTP {status}: {message}"),
        },
        other => OAuthError::Transport {
            message: other.to_string(),
        },
    }
}

/// 400/401/403 is the provider saying "this credential is dead"; anything
/// else is worth retrying. Only parsed error fields are quoted, never the raw
/// body, so a token echoed by a broken endpoint cannot leak into a log.
fn token_status_error(status: u16, body: &str) -> OAuthError {
    let detail = describe_error(body);
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    let message = format!("HTTP {status}{suffix}");
    match status {
        400 | 401 | 403 => OAuthError::TokenRejected { status, message },
        _ => OAuthError::Transport { message },
    }
}

fn describe_error(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let code = field("error").or_else(|| field("code"));
    let description = field("error_description").or_else(|| field("message"));
    match (code, description) {
        (Some(code), Some(description)) if code != description => format!("{code}: {description}"),
        (Some(code), _) => code,
        (None, Some(description)) => description,
        (None, None) => String::new(),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockFetch, MockFetchResponse};
    use std::sync::Mutex;

    fn describe(id: &str) -> &'static OAuthProvider {
        provider::find(id).expect("builtin provider")
    }

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

    fn request_body(fetch: &MockFetch, index: usize) -> Vec<u8> {
        fetch.requests.lock().expect("requests")[index]
            .body
            .clone()
            .expect("body")
    }

    fn jwt(claims: Value) -> String {
        let payload = encode::base64url_encode(claims.to_string().as_bytes());
        format!("header.{payload}.signature")
    }

    #[derive(Default)]
    struct TestUi {
        auth: Mutex<Vec<(String, String)>>,
        progress: Mutex<Vec<String>>,
        codes: Mutex<Vec<String>>,
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
            let mut codes = self.codes.lock().expect("codes");
            if codes.is_empty() {
                None
            } else {
                Some(codes.remove(0))
            }
        }
    }

    #[test]
    fn the_authorization_url_carries_each_expected_parameter_once() {
        let provider = describe("anthropic");
        let url = build_authorize_url(provider, "http://localhost:54545/callback", "st-1", "ch-1");
        assert!(
            url.starts_with("https://claude.ai/oauth/authorize?"),
            "{url}"
        );
        let query = url.split_once('?').expect("query").1;
        let params = encode::parse_query(query);
        let expected = [
            ("client_id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
            ("response_type", "code"),
            ("redirect_uri", "http://localhost:54545/callback"),
            (
                "scope",
                "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            ),
            ("code_challenge", "ch-1"),
            ("code_challenge_method", "S256"),
            ("state", "st-1"),
            ("code", "true"),
        ];
        assert_eq!(params.len(), expected.len(), "{params:?}");
        for (key, value) in expected {
            let found: Vec<&String> = params
                .iter()
                .filter(|(name, _)| name == key)
                .map(|(_, value)| value)
                .collect();
            assert_eq!(found, vec![&value.to_owned()], "param {key} in {params:?}");
        }
    }

    #[test]
    fn the_codex_url_pins_the_redirect_uri_and_appends_its_extra_params() {
        let provider = describe("openai-codex");
        let url = build_authorize_url(
            provider,
            "http://localhost:1455/auth/callback",
            "st-2",
            "ch-2",
        );
        let params = encode::parse_query(url.split_once('?').expect("query").1);
        let value = |key: &str| {
            params
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            value("redirect_uri").as_deref(),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(value("id_token_add_organizations").as_deref(), Some("true"));
        assert_eq!(value("codex_cli_simplified_flow").as_deref(), Some("true"));
        assert_eq!(value("originator").as_deref(), Some("titi"));
        assert_eq!(value("code_challenge_method").as_deref(), Some("S256"));
    }

    #[tokio::test]
    async fn anthropic_exchange_posts_a_json_body_with_the_state() {
        let fetch = MockFetch::new(vec![ok(serde_json::json!({
            "access_token": "sk-test-access",
            "refresh_token": "sk-test-refresh",
            "expires_in": 3600,
        }))]);
        let before = now_secs();
        let tokens = exchange_code(
            describe("anthropic"),
            &fetch,
            "code-1",
            "http://localhost:54545/callback",
            "verifier-1",
            "state-1",
        )
        .await
        .expect("exchange");

        let request = fetch.requests.lock().expect("requests")[0].clone();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://api.anthropic.com/v1/oauth/token");
        let content_type = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.to_string());
        assert_eq!(content_type.as_deref(), Some("application/json"));
        let body: Value =
            serde_json::from_slice(&request.body.clone().expect("body")).expect("json body");
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(body["code"], "code-1");
        assert_eq!(body["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(body["code_verifier"], "verifier-1");
        assert_eq!(body["state"], "state-1");

        // now + expires_in - skew, with the clock between the two reads.
        let expires_at = tokens.expires_at.expect("expiry");
        assert!(
            (expires_at - (before + 3600 - 300)).abs() <= 5,
            "{expires_at} vs {before}"
        );
        assert_eq!(tokens.refresh.as_deref(), Some("sk-test-refresh"));
    }

    #[tokio::test]
    async fn codex_exchange_posts_a_form_body() {
        let fetch = MockFetch::new(vec![ok(serde_json::json!({
            "access_token": jwt(serde_json::json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_plan_type": "Plus",
                },
                "https://api.openai.com/profile": { "email": "User@Example.COM" },
            })),
            "refresh_token": "sk-test-refresh",
            "expires_in": 3600,
        }))]);
        let tokens = exchange_code(
            describe("openai-codex"),
            &fetch,
            "code-2",
            "http://localhost:1455/auth/callback",
            "verifier-2",
            "",
        )
        .await
        .expect("exchange");

        let request = fetch.requests.lock().expect("requests")[0].clone();
        let content_type = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.to_string());
        assert_eq!(
            content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        let body = String::from_utf8(request_body(&fetch, 0)).expect("utf-8");
        assert!(body.contains("grant_type=authorization_code"), "{body}");
        assert!(body.contains("code=code-2"), "{body}");
        assert!(body.contains("code_verifier=verifier-2"), "{body}");
        assert!(!body.contains("state"), "{body}");

        // Identity comes from the access token's claims.
        assert_eq!(tokens.account_id.as_deref(), Some("acct-1"));
        assert_eq!(tokens.org_id.as_deref(), Some("acct-1"));
        assert_eq!(tokens.email.as_deref(), Some("user@example.com"));
        assert_eq!(tokens.org_name.as_deref(), Some("plus"));
    }

    #[tokio::test]
    async fn anthropic_identity_comes_from_the_bootstrap_endpoint() {
        let fetch = MockFetch::new(vec![
            ok(serde_json::json!({
                "access_token": "sk-test-access",
                "refresh_token": "sk-test-refresh",
                "expires_in": 3600,
            })),
            ok(serde_json::json!({
                "oauth_account": {
                    "account_uuid": "acct-2",
                    "account_email": "user@example.invalid",
                    "organization_uuid": "org-2",
                    "organization_name": "Team",
                }
            })),
        ]);
        let tokens = exchange_code(
            describe("anthropic"),
            &fetch,
            "code-3",
            "http://localhost:54545/callback",
            "verifier-3",
            "state-3",
        )
        .await
        .expect("exchange");
        assert_eq!(tokens.account_id.as_deref(), Some("acct-2"));
        assert_eq!(tokens.email.as_deref(), Some("user@example.invalid"));
        assert_eq!(tokens.org_id.as_deref(), Some("org-2"));
        assert_eq!(tokens.org_name.as_deref(), Some("Team"));

        let bootstrap = fetch.requests.lock().expect("requests")[1].clone();
        assert_eq!(bootstrap.method, "GET");
        assert_eq!(
            bootstrap.url,
            "https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli&model=claude-opus-4-8"
        );
        let header = |name: &str| {
            bootstrap
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_string())
        };
        assert_eq!(
            header("anthropic-beta").as_deref(),
            Some("oauth-2025-04-20")
        );
        assert_eq!(
            header("authorization").as_deref(),
            Some("Bearer sk-test-access")
        );
    }

    #[tokio::test]
    async fn a_failing_identity_lookup_still_yields_a_login() {
        let fetch = MockFetch::new(vec![ok(serde_json::json!({
            "access_token": "sk-test-access",
            "refresh_token": "sk-test-refresh",
            "expires_in": 3600,
        }))]);
        let tokens = exchange_code(
            describe("anthropic"),
            &fetch,
            "code-4",
            "http://localhost:54545/callback",
            "verifier-4",
            "state-4",
        )
        .await
        .expect("exchange");
        assert_eq!(tokens.access, "sk-test-access");
        assert!(tokens.account_id.is_none());
    }

    #[tokio::test]
    async fn refresh_keeps_the_stored_org_and_refresh_token() {
        let current = OAuthTokens {
            access: "sk-test-old".to_owned(),
            refresh: Some("sk-test-refresh".to_owned()),
            expires_at: Some(1),
            account_id: Some("acct-1".to_owned()),
            email: Some("user@example.invalid".to_owned()),
            org_id: Some("org-1".to_owned()),
            org_name: Some("team".to_owned()),
        };
        let fetch = MockFetch::new(vec![ok(serde_json::json!({
            "access_token": "sk-test-new",
            "expires_in": 3600,
        }))]);
        let tokens = refresh(describe("anthropic"), &current, &fetch)
            .await
            .expect("refresh");
        assert_eq!(tokens.access, "sk-test-new");
        assert_eq!(tokens.refresh.as_deref(), Some("sk-test-refresh"));
        assert_eq!(tokens.account_id.as_deref(), Some("acct-1"));
        assert_eq!(tokens.email.as_deref(), Some("user@example.invalid"));
        assert_eq!(tokens.org_id.as_deref(), Some("org-1"));
        assert_eq!(tokens.org_name.as_deref(), Some("team"));

        let request = fetch.requests.lock().expect("requests")[0].clone();
        assert_eq!(request.url, "https://api.anthropic.com/v1/oauth/token");
        let body: Value =
            serde_json::from_slice(&request.body.clone().expect("body")).expect("json body");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "sk-test-refresh");
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_string())
        };
        assert_eq!(
            header("anthropic-beta").as_deref(),
            Some("oauth-2025-04-20")
        );
        // The `{sdk}` slot is filled from the Claude Code SDK version, never
        // sent as the raw template.
        assert_eq!(
            header("user-agent").as_deref(),
            Some("anthropic-sdk-typescript/0.112.1 userOAuthProvider")
        );
    }

    #[tokio::test]
    async fn a_rejected_refresh_is_terminal_and_a_transport_failure_is_not() {
        let current = OAuthTokens {
            access: "sk-test-old".to_owned(),
            refresh: Some("sk-test-refresh".to_owned()),
            expires_at: Some(1),
            account_id: None,
            email: None,
            org_id: Some("org-1".to_owned()),
            org_name: None,
        };
        let rejected = MockFetch::new(vec![status(
            400,
            serde_json::json!({ "error": "invalid_grant" }),
        )]);
        let error = refresh(describe("anthropic"), &current, &rejected)
            .await
            .expect_err("rejected");
        assert!(
            matches!(error, OAuthError::TokenRejected { status: 400, .. }),
            "{error:?}"
        );
        assert!(error.is_terminal());
        assert!(!error.is_transport());
        assert!(error.to_string().contains("invalid_grant"), "{error}");

        let offline = MockFetch::new(vec![Err(TransportError::Retryable {
            status: None,
            message: "offline".into(),
        })]);
        let error = refresh(describe("anthropic"), &current, &offline)
            .await
            .expect_err("offline");
        assert!(matches!(error, OAuthError::Transport { .. }), "{error:?}");
        assert!(error.is_transport());
        assert!(!error.is_terminal());
    }

    #[tokio::test]
    async fn a_refresh_without_a_stored_token_is_a_configuration_error() {
        let current = OAuthTokens {
            access: "sk-test-old".to_owned(),
            refresh: None,
            expires_at: None,
            account_id: None,
            email: None,
            org_id: None,
            org_name: None,
        };
        let fetch = MockFetch::default();
        let error = refresh(describe("anthropic"), &current, &fetch)
            .await
            .expect_err("no refresh token");
        assert!(matches!(error, OAuthError::Config { .. }), "{error:?}");
        assert_eq!(fetch.request_count(), 0);
    }

    #[tokio::test]
    async fn the_manual_path_needs_no_http() {
        let mut session =
            LoginSession::start(describe("anthropic"), Arc::new(MockFetch::default()))
                .await
                .expect("start");
        assert!(
            session
                .url()
                .starts_with("https://claude.ai/oauth/authorize?")
        );
        assert!(session.redirect_uri().ends_with("/callback"));
        assert!(!session.instructions().is_empty());
        let code = session
            .wait_for_code(|| Some("sk-test-manual".to_owned()))
            .await
            .expect("code");
        assert_eq!(code, "sk-test-manual");
    }

    #[tokio::test]
    async fn a_pasted_redirect_url_needs_a_matching_state() {
        let mut server = callback::CallbackServer::bind(
            &CallbackSpec {
                host: "127.0.0.1",
                port: 0,
                path: "/callback",
                redirect_uri: None,
                port_fallback: true,
            },
            "st-9",
        )
        .await
        .expect("bind");
        let mut attempt = 0;
        let code = server
            .wait_for_code(|| {
                attempt += 1;
                Some(match attempt {
                    1 => "http://127.0.0.1:1/callback?code=sk-test-wrong&state=st-other".to_owned(),
                    _ => "http://127.0.0.1:1/callback?code=sk-test-right&state=st-9".to_owned(),
                })
            })
            .await
            .expect("code");
        assert_eq!(code, "sk-test-right");
        assert!(attempt >= 2);
    }

    #[tokio::test]
    async fn the_convenience_login_drives_the_ui() {
        let ui = TestUi {
            codes: Mutex::new(vec!["code-9".to_owned()]),
            ..TestUi::default()
        };
        let fetch = MockFetch::new(vec![ok(serde_json::json!({
            "access_token": "sk-test-access",
            "refresh_token": "sk-test-refresh",
            "expires_in": 3600,
        }))]);
        let tokens = login(describe("anthropic"), &fetch, &ui)
            .await
            .expect("login");
        assert_eq!(tokens.access, "sk-test-access");
        let auth = ui.auth.lock().expect("auth");
        assert_eq!(auth.len(), 1);
        assert!(auth[0].0.starts_with("https://claude.ai/oauth/authorize?"));
        assert_eq!(auth[0].1, describe("anthropic").instructions);
        assert!(ui.progress.lock().expect("progress").len() >= 2);
    }

    #[test]
    fn tokens_are_masked_in_debug() {
        let tokens = OAuthTokens {
            access: "sk-test-access".to_owned(),
            refresh: Some("sk-test-refresh".to_owned()),
            expires_at: None,
            account_id: None,
            email: None,
            org_id: None,
            org_name: None,
        };
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains("sk-test-access"), "{rendered}");
        assert!(!rendered.contains("sk-test-refresh"), "{rendered}");
        assert!(rendered.contains("cess"), "{rendered}");
    }

    #[test]
    fn stored_rows_convert_both_ways() {
        let credential = Credential {
            provider: "anthropic".to_owned(),
            label: "default".to_owned(),
            kind: "oauth".to_owned(),
            token: "sk-test-access".to_owned(),
            expires_at: Some(42),
            updated_at: 7,
            refresh_token: Some("sk-test-refresh".to_owned()),
            account_id: Some("acct-1".to_owned()),
            email: Some("user@example.invalid".to_owned()),
            org_id: Some("org-1".to_owned()),
            org_name: Some("team".to_owned()),
            authorized_at: Some(1),
        };
        let tokens = from_stored(&credential);
        assert_eq!(tokens.access, "sk-test-access");
        assert_eq!(tokens.refresh.as_deref(), Some("sk-test-refresh"));
        assert_eq!(tokens.expires_at, Some(42));
        assert_eq!(tokens.org_name.as_deref(), Some("team"));

        let record = to_record(&tokens, "anthropic", "default");
        assert_eq!(record.provider, "anthropic");
        assert_eq!(record.label, "default");
        assert_eq!(record.access, "sk-test-access");
        assert_eq!(record.refresh, Some("sk-test-refresh"));
        assert_eq!(record.expires_at, Some(42));
        assert_eq!(record.org_id, Some("org-1"));
        assert!(record.authorized_at.is_some());
    }
}
