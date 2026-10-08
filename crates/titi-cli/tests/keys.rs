//! `--set-key` / `--list-keys`: stored credentials reach the engine's
//! credential ladder without an environment variable.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use titi_cli::secrets::{list_keys, store_key};
use titi_engine::{CredentialSource, LayeredCredentialSource, ProviderDescriptor};
use titi_providers::ApiKind;

fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        id: "opencode-go".into(),
        api: ApiKind::OpenAiCompletions,
        base_url: "https://example.invalid/v1".into(),
        credential_env: Some("TITI_TEST_OPENCODE_GO_KEY".into()),
        credential_required: true,
        discover_with_credential: false,
    }
}

#[test]
fn a_stored_key_round_trips_without_echoing_the_token() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list_keys(dir.path()).unwrap().is_empty());

    store_key(dir.path(), "opencode-go", "sk-test-value").unwrap();

    let keys = list_keys(dir.path()).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].provider, "opencode-go");
    assert_eq!(keys[0].kind, "api_key");

    // Storing again replaces rather than duplicating.
    store_key(dir.path(), "opencode-go", "sk-rotated").unwrap();
    assert_eq!(list_keys(dir.path()).unwrap().len(), 1);
}

#[test]
fn an_empty_provider_or_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    assert!(store_key(dir.path(), "  ", "sk-x").is_err());
    assert!(store_key(dir.path(), "opencode-go", "   ").is_err());
    assert!(list_keys(dir.path()).unwrap().is_empty());
}

#[test]
fn the_engine_credential_ladder_reads_the_stored_key() {
    // No environment variable is set for this provider, so the store is the
    // only source that can answer.
    assert!(std::env::var_os("TITI_TEST_OPENCODE_GO_KEY").is_none());
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();

    let source = LayeredCredentialSource::for_agent_dir(agent_dir);
    assert!(
        source.resolve(&descriptor()).is_none(),
        "nothing stored yet"
    );

    store_key(agent_dir, "opencode-go", "sk-test-value").unwrap();
    let credential = source
        .resolve(&descriptor())
        .expect("the stored key resolves");
    assert_eq!(credential.access, "sk-test-value");
}
