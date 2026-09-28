//! OAuth login from the surfaces: the chat's code mode, the `--login` flag
//! and the credential listing. No socket, browser or provider is involved —
//! the chat gets a fake driver, the flag tests run against a temp store.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use titi_cli::chat::{Chat, ChatEffect, Key};
use titi_cli::engine::{ModelCatalog, default_registry_config};
use titi_cli::login::{LoginDriver, LoginEvent, LoginFlow, OAuthProvider};
use titi_engine::{CredentialSource, HttpTransportFactory, ProviderDescriptor, ProviderRegistry};
use titi_providers::oauth::OAuthTokens;

/// A login that needs no browser, no socket and no provider: it announces a
/// URL, waits for a code on the channel and answers with a grant.
struct FakeLogin {
    url: String,
    expires_at: Option<i64>,
}

impl LoginDriver for FakeLogin {
    fn begin(&self, _provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
        let (events, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (codes, mut code_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = events.send(LoginEvent::Url {
            url: self.url.clone(),
            instructions: "Sign in, then paste the code.".to_owned(),
        });
        let expires_at = self.expires_at;
        std::thread::spawn(move || {
            // The code end closing is a cancel: no answer, no event.
            if code_rx.blocking_recv().is_some() {
                let _ = events.send(LoginEvent::Done(Box::new(OAuthTokens {
                    access: "sk-test-access".to_owned(),
                    refresh: Some("sk-test-refresh".to_owned()),
                    expires_at,
                    account_id: Some("acct-1".to_owned()),
                    email: Some("user@example.invalid".to_owned()),
                    org_id: None,
                    org_name: Some("Pro".to_owned()),
                })));
            }
        });
        Ok(LoginFlow {
            events: event_rx,
            codes,
        })
    }
}

fn chat(agent_dir: &Path) -> Chat {
    let mut chat = Chat::new("openai/gpt-4.1", "session-123");
    chat.set_agent_dir(agent_dir);
    chat
}

fn chat_with_fake(agent_dir: &Path, url: &str) -> Chat {
    let mut chat = chat(agent_dir);
    chat.set_login_driver(Arc::new(FakeLogin {
        url: url.to_owned(),
        expires_at: None,
    }));
    chat
}

fn type_text(chat: &mut Chat, text: &str) {
    let now = Instant::now();
    for ch in text.chars() {
        chat.on_key(Key::Char(ch), now);
    }
}

fn send(chat: &mut Chat, line: &str) -> titi_cli::chat::Applied {
    type_text(chat, line);
    chat.on_key(Key::Enter, Instant::now())
}

fn transcript(chat: &Chat) -> String {
    let lines: Vec<&str> = chat
        .transcript()
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    lines.join("\n")
}

fn has_line(chat: &Chat, expected: &str) -> bool {
    chat.transcript().iter().any(|line| line.text == expected)
}

/// The credential rows an agent directory holds. A store that cannot be read
/// is a failed test, not a case to tolerate.
fn rows(dir: &Path) -> Vec<titi_cli::secrets::StoredKey> {
    titi_cli::secrets::list_keys(dir).expect("keys")
}

/// Polls the way the render loop does, until `ready` says the flow landed.
fn poll_until(chat: &mut Chat, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        chat.poll_login();
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the login never finished");
}

#[test]
fn esc_leaves_the_login_without_writing_anything() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_with_fake(dir.path(), "https://example.invalid/authorize");

    send(&mut chat, "/login anthropic");
    chat.poll_login();
    let shown = transcript(&chat);
    assert!(
        shown.contains("https://example.invalid/authorize"),
        "{shown}"
    );
    assert!(shown.contains("Sign in, then paste the code."), "{shown}");

    let escaped = chat.on_key(Key::Esc, Instant::now());
    assert!(escaped.effect.is_none());
    assert!(
        rows(dir.path()).is_empty(),
        "a cancelled login writes nothing"
    );

    // Back at the composer: Enter sends a prompt rather than feeding a login.
    let sent = send(&mut chat, "hello");
    assert!(
        matches!(sent.effect, Some(ChatEffect::Send(_))),
        "the prompt came back: {sent:?}"
    );
}

#[test]
fn a_pasted_code_stores_an_oauth_credential() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_with_fake(dir.path(), "https://example.invalid/authorize");

    send(&mut chat, "/login anthropic");
    chat.poll_login();
    send(&mut chat, "sk-test-code");
    poll_until(&mut chat, || !rows(dir.path()).is_empty());

    let keys = rows(dir.path());
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].provider, "anthropic");
    assert_eq!(keys[0].kind, "oauth");
    let shown = transcript(&chat);
    assert!(shown.contains("logged in to"), "{shown}");
    assert!(
        !shown.contains("sk-test-access"),
        "the token stays out of the transcript: {shown}"
    );
}

#[test]
fn keys_shows_a_signed_in_provider_as_oauth() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat_with_fake(dir.path(), "https://example.invalid/authorize");

    // openai-codex reads no environment variable, so the stored row is the
    // only thing /keys can find — nothing here depends on this machine.
    send(&mut chat, "/login openai-codex");
    chat.poll_login();
    send(&mut chat, "sk-test-code");
    poll_until(&mut chat, || !rows(dir.path()).is_empty());

    send(&mut chat, "/keys");
    assert!(
        has_line(&chat, "openai-codex  oauth"),
        "{}",
        transcript(&chat)
    );
}

#[test]
fn an_inline_key_is_still_stored_as_an_api_key() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat(dir.path());

    let applied = send(&mut chat, "/login anthropic sk-test-key");
    assert!(applied.effect.is_none());
    let keys = rows(dir.path());
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].provider, "anthropic");
    assert_eq!(keys[0].kind, "api_key");
    assert!(
        has_line(&chat, "anthropic: key stored"),
        "{}",
        transcript(&chat)
    );
}

#[test]
fn the_login_flag_lists_the_oauth_providers() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--login")
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for id in ["anthropic", "openai-codex"] {
        let line = stderr
            .lines()
            .find(|line| line.starts_with(id))
            .unwrap_or_else(|| panic!("{id} is not listed: {stderr}"));
        assert!(line.len() > id.len() + 2, "{id} has no name: {line}");
    }
}

#[test]
fn an_unknown_oauth_provider_exits_two() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .args(["--login", "nope"])
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown oauth provider nope"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn the_list_keys_flag_shows_kind_and_lifetime() {
    let dir = tempfile::tempdir().expect("temp");
    let now = titi_cli::secrets::now_secs();
    let tokens = OAuthTokens {
        access: "sk-test-access".to_owned(),
        refresh: None,
        expires_at: Some(now + 3_600),
        account_id: None,
        email: None,
        org_id: None,
        org_name: None,
    };
    titi_cli::secrets::store_oauth(dir.path(), "anthropic", &tokens).expect("store oauth");
    titi_cli::secrets::store_key(dir.path(), "openai", "sk-test-key").expect("store key");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_titi"))
        .arg("--list-keys")
        .env("TITI_AGENT_DIR", dir.path())
        .output()
        .expect("the binary runs");
    assert!(output.status.success(), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("anthropic  (oauth")),
        "{stderr}"
    );
    assert!(
        stderr.lines().any(|line| line == "openai  (api_key)"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("sk-test"),
        "a token never reaches the listing: {stderr}"
    );
}

#[test]
fn the_codex_provider_is_registered_with_its_endpoint() {
    let config = default_registry_config();
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == "openai-codex")
        .expect("openai-codex is a builtin");
    assert_eq!(provider.api.to_string(), "openai-responses");
    assert_eq!(
        provider.base_url.as_str(),
        "https://chatgpt.com/backend-api/codex"
    );
    assert!(provider.credential_env.is_none());
    assert!(
        provider.credential_required,
        "the endpoint serves no keyless model list"
    );
    assert!(
        provider.discover_with_credential,
        "the subscription catalog is only reachable with the stored token"
    );

    let codex: Vec<&str> = config
        .models
        .iter()
        .filter(|model| model.provider == "openai-codex")
        .map(|model| model.wire_model.as_str())
        .collect();
    assert!(!codex.is_empty(), "the codex models are the catalog");
    assert!(codex.contains(&"gpt-5.5"), "{codex:?}");
    for model in config
        .models
        .iter()
        .filter(|model| model.provider == "openai-codex")
    {
        assert_eq!(
            model.id.as_str(),
            format!("openai-codex/{}", model.wire_model)
        );
    }
}

/// Only the Codex subscription answers, so whatever a login unlocked has to
/// come out ahead of the models that were keyless when the screen started.
struct OnlyCodex;

impl CredentialSource for OnlyCodex {
    fn resolve(&self, provider: &ProviderDescriptor) -> Option<titi_providers::Credential> {
        (provider.id == "openai-codex").then(|| titi_providers::Credential {
            access: "sk-test".into(),
            kind: titi_providers::CredKind::BearerToken,
            account_id: None,
            level: titi_providers::LadderLevel::Stored,
        })
    }
}

#[test]
fn a_login_moves_the_models_it_unlocked_to_the_front() {
    let config = default_registry_config();
    let startup: Vec<String> = config
        .models
        .iter()
        .map(|model| model.id.to_string())
        .collect();
    let unlocked: Vec<String> = startup
        .iter()
        .filter(|id| id.starts_with("openai-codex/"))
        .cloned()
        .collect();
    let built = ProviderRegistry::new(config, Arc::new(OnlyCodex), Arc::new(HttpTransportFactory))
        .expect("the built-in catalog builds a registry");
    let mut catalog = ModelCatalog::new(startup, Arc::new(built));
    assert!(
        catalog
            .ids()
            .first()
            .is_some_and(|id| id.starts_with("openai/")),
        "the configured order comes first: {:?}",
        catalog.ids()
    );

    catalog.refresh_after_login();

    // A login reorders the startup list, it hides nothing: `ids` is always
    // that order followed by whatever the registry knows, so the models the
    // subscription unlocked lead and the keyless ones still follow.
    let ids = catalog.ids();
    assert_eq!(ids[..unlocked.len()], unlocked[..], "{ids:?}");
}
