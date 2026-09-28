//! Credential administration for the CLI: `--set-key` / `--list-keys`.
//!
//! Keys and OAuth tokens land in `<agent_dir>/auth.db`, the store
//! `LayeredCredentialSource` already consults after the environment and
//! `.env` layers. Tokens are never printed back — listing shows provider ids,
//! kinds, and for a token with a lifetime its remaining time.

use std::path::Path;

use titi_providers::oauth::OAuthTokens;
use titi_secrets::store::{AuthStore, DEFAULT_LABEL};

/// One stored credential, without its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredKey {
    pub provider: String,
    pub kind: String,
    pub updated_at: i64,
    /// Unix seconds; `None` = never expires. An API key never has one.
    pub expires_at: Option<i64>,
}

/// Stores an API key for `provider`.
pub fn store_key(agent_dir: &Path, provider: &str, key: &str) -> Result<(), String> {
    let provider = provider.trim();
    let key = key.trim();
    if provider.is_empty() {
        return Err("a provider id is required".into());
    }
    if key.is_empty() {
        return Err("a key value is required".into());
    }
    let store = AuthStore::open(&agent_dir.join("auth.db")).map_err(|e| e.to_string())?;
    store
        .store(provider, "api_key", key, None)
        .map_err(|e| e.to_string())
}

/// Stores an OAuth token set under `provider` — the descriptor's `store_as`.
///
/// Replaces whatever credential the default account held: the same provider
/// answering to a new grant is a sign-in, not a second account.
pub fn store_oauth(agent_dir: &Path, provider: &str, tokens: &OAuthTokens) -> Result<(), String> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Err("a provider id is required".into());
    }
    if tokens.access.trim().is_empty() {
        return Err("an access token is required".into());
    }
    let store = AuthStore::open(&agent_dir.join("auth.db")).map_err(|e| e.to_string())?;
    let record = titi_providers::oauth::to_record(tokens, provider, DEFAULT_LABEL);
    store.store_oauth(record).map_err(|e| e.to_string())
}

/// Drops the stored credential for `provider`. `Ok(false)` means there was none.
pub fn remove_key(agent_dir: &Path, provider: &str) -> Result<bool, String> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Err("a provider id is required".into());
    }
    let store = AuthStore::open(&agent_dir.join("auth.db")).map_err(|e| e.to_string())?;
    store.remove(provider).map_err(|e| e.to_string())
}

/// Every stored credential's provider id, kind, timestamp and lifetime. No tokens.
pub fn list_keys(agent_dir: &Path) -> Result<Vec<StoredKey>, String> {
    let store = AuthStore::open(&agent_dir.join("auth.db")).map_err(|e| e.to_string())?;
    store.list().map_err(|e| e.to_string()).map(|rows| {
        rows.into_iter()
            .map(|row| StoredKey {
                provider: row.provider,
                kind: row.kind,
                updated_at: row.updated_at,
                expires_at: row.expires_at,
            })
            .collect()
    })
}

/// The `--list-keys` rendering of one credential: `oauth, 3h 12m left`,
/// `oauth` for a token with no stated lifetime, `api_key` for a key.
///
/// Nothing here can reach a token: the input has no token field.
pub fn describe_key(key: &StoredKey, now: i64) -> String {
    match key.expires_at {
        Some(at) => format!("{}, {}", key.kind, remaining_lifetime(at, now)),
        None => key.kind.clone(),
    }
}

/// How long a credential has left, in the coarsest unit that still says
/// something a person can act on — a token about to die is the one case
/// where minutes matter.
pub fn remaining_lifetime(expires_at: i64, now: i64) -> String {
    let seconds = expires_at - now;
    if seconds <= 0 {
        return "expired".to_owned();
    }
    let minutes = seconds / 60;
    if minutes < 1 {
        return "<1m left".to_owned();
    }
    if minutes < 60 {
        return format!("{minutes}m left");
    }
    format!("{}h {}m left", minutes / 60, minutes % 60)
}

/// Unix seconds now.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(expires_at: Option<i64>) -> OAuthTokens {
        OAuthTokens {
            access: "sk-test-access".into(),
            refresh: Some("sk-test-refresh".into()),
            expires_at,
            account_id: Some("acct-1".into()),
            email: Some("user@example.invalid".into()),
            org_id: Some("acct-1".into()),
            org_name: Some("Pro".into()),
        }
    }

    #[test]
    fn an_oauth_row_round_trips_as_oauth_with_its_lifetime() {
        let dir = tempfile::tempdir().expect("temp");
        let now = now_secs();
        store_oauth(dir.path(), "anthropic", &tokens(Some(now + 3 * 3600))).expect("store");

        let rows = list_keys(dir.path()).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[0].kind, "oauth");
        assert!(rows[0].expires_at.is_some());
        assert_eq!(describe_key(&rows[0], now), "oauth, 3h 0m left");
    }

    #[test]
    fn an_api_key_stays_distinguishable_from_an_oauth_token() {
        let dir = tempfile::tempdir().expect("temp");
        store_key(dir.path(), "openai", "sk-test-key").expect("store key");
        store_oauth(dir.path(), "anthropic", &tokens(None)).expect("store oauth");

        let rows = list_keys(dir.path()).expect("list");
        let now = now_secs();
        let rendered: Vec<String> = rows
            .iter()
            .map(|row| format!("{}  ({})", row.provider, describe_key(row, now)))
            .collect();
        assert!(
            rendered.contains(&"openai  (api_key)".to_owned()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&"anthropic  (oauth)".to_owned()),
            "{rendered:?}"
        );
    }

    #[test]
    fn a_lifetime_reads_in_the_coarsest_useful_unit() {
        let now = 1_000_000;
        assert_eq!(remaining_lifetime(now, now), "expired");
        assert_eq!(remaining_lifetime(now - 10, now), "expired");
        assert_eq!(remaining_lifetime(now + 30, now), "<1m left");
        assert_eq!(remaining_lifetime(now + 12 * 60, now), "12m left");
        assert_eq!(
            remaining_lifetime(now + 3 * 3600 + 12 * 60, now),
            "3h 12m left"
        );
    }

    #[test]
    fn an_empty_provider_or_token_is_refused() {
        let dir = tempfile::tempdir().expect("temp");
        assert!(store_oauth(dir.path(), "  ", &tokens(None)).is_err());
        let mut empty = tokens(None);
        empty.access = "   ".into();
        assert!(store_oauth(dir.path(), "anthropic", &empty).is_err());
        assert!(list_keys(dir.path()).expect("list").is_empty());
    }
}
