//! Credential ladder: runtime > config > OAuth > login-key > env > stored >
//! fallback. The model-facing [`Credential`] type carries only the access
//! material — refresh tokens are typologically hidden from this layer and
//! owned exclusively by the auth actor.
//!
//! A provider may hold several accounts (`anthropic/work`,
//! `anthropic/personal`); [`AccountRotation`] picks which one feeds the
//! `stored` rung and moves to the next one when the upstream rate limits.

use std::fmt;

use smol_str::SmolStr;

use crate::transport::TransportError;

/// Ladder rungs in strict priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LadderLevel {
    /// 1. `--api-key` runtime override; never persisted.
    Runtime = 1,
    /// 2. `models.yml` config key (beats OAuth so a proxy never receives an
    /// upstream OAuth token).
    Config = 2,
    /// 3. Stored OAuth access token.
    OAuth = 3,
    /// 4. Key saved by an interactive `/login`.
    LoginKey = 4,
    /// 5. Provider env var (including `.env` files).
    Env = 5,
    /// 6. Other stored keys (broker-migrated).
    Stored = 6,
    /// 7. Fallback resolver in the descriptor.
    Fallback = 7,
}

/// The resolved access material. Deliberately has **no** refresh field:
/// refresh tokens never cross the credential-actor boundary into the model
/// layer. `Debug` masks the access material down to its last four characters.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub access: SmolStr,
    pub kind: CredKind,
    /// Which rung produced this credential (for telemetry/rotation).
    pub level: LadderLevel,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access", &mask_secret(&self.access))
            .field("kind", &self.kind)
            .field("level", &self.level)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredKind {
    ApiKey,
    BearerToken,
}

/// Reduce a secret to its last four characters. Anything shorter than five
/// characters is hidden entirely.
pub fn mask_secret(secret: &str) -> String {
    let len = secret.chars().count();
    if len <= 4 {
        return "…".to_owned();
    }
    let tail: String = secret.chars().skip(len - 4).collect();
    format!("…{tail}")
}

/// One account of a provider: the `label` half of a `provider/label` id plus
/// its access material. The material is private — callers either build a
/// [`Credential`] from it or render [`Account::masked`].
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    label: SmolStr,
    access: SmolStr,
    kind: CredKind,
}

impl Account {
    pub fn new(label: impl Into<SmolStr>, access: impl Into<SmolStr>, kind: CredKind) -> Self {
        Self {
            label: label.into(),
            access: access.into(),
            kind,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn kind(&self) -> CredKind {
        self.kind
    }

    /// The only renderable form: `label(…last4)`.
    pub fn masked(&self) -> String {
        format!("{}({})", self.label, mask_secret(&self.access))
    }

    /// Hand the access material to the model layer at `level`.
    pub fn credential(&self, level: LadderLevel) -> Credential {
        Credential {
            access: self.access.clone(),
            kind: self.kind,
            level,
        }
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("label", &self.label)
            .field("access", &mask_secret(&self.access))
            .field("kind", &self.kind)
            .finish()
    }
}

/// Whether `err` is an upstream rate limit (HTTP 429) — the only failure that
/// rotates accounts. Server errors and stalls hit every account of the
/// provider alike and belong to the fallback chain instead.
pub fn is_rate_limit(err: &TransportError) -> bool {
    matches!(
        err,
        TransportError::Retryable {
            status: Some(429),
            ..
        } | TransportError::Fatal {
            status: Some(429),
            ..
        }
    )
}

/// Round-robin over the accounts of one provider.
///
/// Only a rate limit rotates: it retires the active account for the current
/// cycle and hands out the next one, wrapping around the list. Each account
/// is handed out at most once per cycle, so a provider with a single account
/// behaves exactly as it did before rotation existed, and a provider whose
/// accounts are all limited stops instead of spinning. A successful turn
/// ([`AccountRotation::reset`]) opens a fresh cycle from the account that
/// worked.
#[derive(Debug, Clone, Default)]
pub struct AccountRotation {
    accounts: Vec<Account>,
    active: usize,
    /// Accounts handed out in the current cycle, the active one included.
    used: usize,
}

impl AccountRotation {
    pub fn new(accounts: Vec<Account>) -> Self {
        let used = usize::from(!accounts.is_empty());
        Self {
            accounts,
            active: 0,
            used,
        }
    }

    /// Same as [`AccountRotation::new`] but resumes on `label` (the last
    /// account known to work); an unknown label starts at the first account.
    pub fn starting_at(accounts: Vec<Account>, label: &str) -> Self {
        let mut rot = Self::new(accounts);
        if let Some(i) = rot.accounts.iter().position(|a| a.label == label) {
            rot.active = i;
        }
        rot
    }

    pub fn current(&self) -> Option<&Account> {
        self.accounts.get(self.active)
    }

    pub fn current_label(&self) -> Option<&str> {
        self.current().map(Account::label)
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Every account of this cycle has been rate limited.
    pub fn is_exhausted(&self) -> bool {
        !self.accounts.is_empty() && self.used >= self.accounts.len()
    }

    /// Advance after a failed turn. Returns the next account to try, or
    /// `None` when the failure is not a rate limit or the cycle is spent.
    pub fn next_on_rate_limit(&mut self, err: &TransportError) -> Option<&Account> {
        if !is_rate_limit(err) || self.is_exhausted() || self.accounts.is_empty() {
            return None;
        }
        self.active = (self.active + 1) % self.accounts.len();
        self.used += 1;
        self.accounts.get(self.active)
    }

    /// A turn succeeded: start a fresh cycle from the active account.
    pub fn reset(&mut self) {
        self.used = usize::from(!self.accounts.is_empty());
    }

    /// Adopt a freshly read account list (the store may have gained or lost
    /// accounts) while keeping the active label and the cycle's progress, so
    /// re-reading the store never un-rotates a rate-limited account.
    pub fn refresh(&mut self, accounts: Vec<Account>) {
        let active = self.current_label().map(SmolStr::new);
        let used = self.used.min(accounts.len());
        self.accounts = accounts;
        self.active = active
            .and_then(|label| self.accounts.iter().position(|a| a.label == label))
            .unwrap_or(0);
        self.used = used.max(usize::from(!self.accounts.is_empty()));
    }
}

/// All ladder inputs for one resolution.
#[derive(Debug, Clone, Default)]
pub struct LadderCtx {
    pub runtime_override: Option<SmolStr>,
    pub config_key: Option<SmolStr>,
    pub oauth_token: Option<SmolStr>,
    pub login_key: Option<SmolStr>,
    pub env_key: Option<SmolStr>,
    pub stored_key: Option<SmolStr>,
    pub fallback_key: Option<SmolStr>,
}

/// Resolve a credential: first matching rung in ladder order wins.
pub fn resolve_credential(ctx: &LadderCtx) -> Option<Credential> {
    let rungs = [
        (
            LadderLevel::Runtime,
            &ctx.runtime_override,
            CredKind::ApiKey,
        ),
        (LadderLevel::Config, &ctx.config_key, CredKind::ApiKey),
        (LadderLevel::OAuth, &ctx.oauth_token, CredKind::BearerToken),
        (LadderLevel::LoginKey, &ctx.login_key, CredKind::ApiKey),
        (LadderLevel::Env, &ctx.env_key, CredKind::ApiKey),
        (LadderLevel::Stored, &ctx.stored_key, CredKind::ApiKey),
        (LadderLevel::Fallback, &ctx.fallback_key, CredKind::ApiKey),
    ];
    for (level, v, kind) in rungs {
        if let Some(access) = v.clone() {
            return Some(Credential {
                access,
                kind,
                level,
            });
        }
    }
    None
}

/// Parse a minimal `.env` file: `KEY=value` lines, quotes stripped, NUL
/// bytes dropped, existing keys in `out` are never overwritten
/// (process-env precedence).
pub fn parse_env_file(content: &str, out: &mut std::collections::HashMap<SmolStr, SmolStr>) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key: SmolStr = key.trim().to_owned().into();
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || key.is_empty() {
            continue;
        }
        let value = value.replace('\0', "");
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        out.entry(key).or_insert_with(|| value.to_owned().into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> LadderCtx {
        LadderCtx {
            runtime_override: None,
            config_key: None,
            oauth_token: None,
            login_key: None,
            env_key: None,
            stored_key: None,
            fallback_key: None,
        }
    }

    fn accounts(labels: &[&str]) -> Vec<Account> {
        labels
            .iter()
            .map(|l| Account::new(*l, format!("sk-test-{l}"), CredKind::ApiKey))
            .collect()
    }

    fn rate_limited() -> TransportError {
        TransportError::Retryable {
            status: Some(429),
            message: "rate limited".into(),
        }
    }

    #[test]
    fn rate_limit_rotates_to_the_next_account_then_stops() {
        let mut rot = AccountRotation::new(accounts(&["work", "personal", "spare"]));
        assert_eq!(rot.current_label(), Some("work"));

        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("personal")
        );
        assert_eq!(rot.current_label(), Some("personal"));
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("spare")
        );
        // Every account has been rate limited in this cycle: stop, never spin.
        assert!(rot.next_on_rate_limit(&rate_limited()).is_none());
        assert!(rot.is_exhausted());
        assert_eq!(rot.current_label(), Some("spare"));

        // A successful turn opens a fresh cycle from the account that worked.
        rot.reset();
        assert!(!rot.is_exhausted());
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("work")
        );
    }

    #[test]
    fn rotation_wraps_around_the_account_list() {
        let mut rot = AccountRotation::starting_at(accounts(&["a", "b", "c"]), "c");
        assert_eq!(rot.current_label(), Some("c"));
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("a")
        );
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("b")
        );
        assert!(rot.next_on_rate_limit(&rate_limited()).is_none());
    }

    #[test]
    fn refreshing_the_account_list_keeps_the_rotation_honest() {
        let mut rot = AccountRotation::new(accounts(&["work", "personal", "spare"]));
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("personal")
        );

        // Re-reading the store must not un-rotate the rate-limited account …
        rot.refresh(accounts(&["work", "personal", "spare"]));
        assert_eq!(rot.current_label(), Some("personal"));
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("spare")
        );
        assert!(rot.next_on_rate_limit(&rate_limited()).is_none());

        // … and a newly stored account joins the current cycle.
        rot.refresh(accounts(&["work", "personal", "spare", "extra"]));
        assert_eq!(rot.current_label(), Some("spare"));
        assert_eq!(
            rot.next_on_rate_limit(&rate_limited()).map(Account::label),
            Some("extra")
        );

        // A dropped active account falls back to the first one.
        rot.refresh(accounts(&["work"]));
        assert_eq!(rot.current_label(), Some("work"));
        assert!(rot.next_on_rate_limit(&rate_limited()).is_none());
    }

    #[test]
    fn single_account_provider_never_rotates() {
        let mut rot = AccountRotation::new(accounts(&["default"]));
        assert_eq!(rot.current_label(), Some("default"));
        assert!(rot.next_on_rate_limit(&rate_limited()).is_none());
        assert_eq!(rot.current_label(), Some("default"));
        assert_eq!(
            rot.current().map(|a| a.credential(LadderLevel::Stored)),
            Some(Credential {
                access: "sk-test-default".into(),
                kind: CredKind::ApiKey,
                level: LadderLevel::Stored,
            })
        );

        // No accounts at all: nothing to hand out, nothing to rotate.
        let mut empty = AccountRotation::new(Vec::new());
        assert!(empty.current().is_none());
        assert!(empty.next_on_rate_limit(&rate_limited()).is_none());
    }

    #[test]
    fn only_rate_limits_rotate_accounts() {
        let not_rate_limits = [
            TransportError::Retryable {
                status: Some(500),
                message: "server".into(),
            },
            TransportError::Retryable {
                status: None,
                message: "unknown".into(),
            },
            TransportError::Fatal {
                status: Some(401),
                message: "unauthorized".into(),
            },
            TransportError::Stalled {
                phase: crate::transport::StallPhase::Idle,
            },
        ];
        for err in not_rate_limits {
            let mut rot = AccountRotation::new(accounts(&["work", "personal"]));
            assert!(
                rot.next_on_rate_limit(&err).is_none(),
                "{err:?} must not rotate accounts"
            );
            assert_eq!(rot.current_label(), Some("work"));
        }
        // A 429 the provider marked fatal (quota exhausted) still rotates.
        let mut rot = AccountRotation::new(accounts(&["work", "personal"]));
        assert!(
            rot.next_on_rate_limit(&TransportError::Fatal {
                status: Some(429),
                message: "quota".into(),
            })
            .is_some()
        );
    }

    #[test]
    fn no_display_path_renders_a_full_secret() {
        let account = Account::new("work", "sk-test-secret-1234", CredKind::ApiKey);
        let rot = AccountRotation::new(vec![account.clone()]);
        for rendered in [
            format!("{account:?}"),
            format!("{rot:?}"),
            account.masked(),
            format!("{:?}", account.credential(LadderLevel::Stored)),
        ] {
            assert!(
                !rendered.contains("sk-test-secret-1234"),
                "secret leaked: {rendered}"
            );
            assert!(rendered.contains("1234"), "last-4 missing: {rendered}");
        }
        assert_eq!(account.masked(), "work(…1234)");
        assert_eq!(mask_secret("abcd"), "…");
    }

    /// Pairwise priority: with only rungs i and j set, the lower rung number
    /// must win.
    #[test]
    fn ladder_priority_pairwise() {
        let levels: [(LadderLevel, fn(&mut LadderCtx, SmolStr)); 7] = [
            (LadderLevel::Runtime, |c, v| c.runtime_override = Some(v)),
            (LadderLevel::Config, |c, v| c.config_key = Some(v)),
            (LadderLevel::OAuth, |c, v| c.oauth_token = Some(v)),
            (LadderLevel::LoginKey, |c, v| c.login_key = Some(v)),
            (LadderLevel::Env, |c, v| c.env_key = Some(v)),
            (LadderLevel::Stored, |c, v| c.stored_key = Some(v)),
            (LadderLevel::Fallback, |c, v| c.fallback_key = Some(v)),
        ];
        for (i, (li, set_i)) in levels.iter().enumerate() {
            for (j, (lj, set_j)) in levels.iter().enumerate() {
                if i == j {
                    continue;
                }
                let mut c = ctx();
                set_i(&mut c, format!("key-{i:?}").into());
                set_j(&mut c, format!("key-{j:?}").into());
                let cred = resolve_credential(&c).expect("pair must resolve");
                let expected = if li < lj { *li } else { *lj };
                assert_eq!(cred.level, expected, "rung {li:?} vs {lj:?}");
            }
        }
    }

    #[test]
    fn runtime_beats_everything_single_check() {
        let mut c = ctx();
        c.runtime_override = Some("rt".into());
        c.config_key = Some("cfg".into());
        c.oauth_token = Some("oauth".into());
        c.env_key = Some("env".into());
        c.fallback_key = Some("fb".into());
        let cred = resolve_credential(&c).expect("resolve");
        assert_eq!(cred.access, "rt");
        assert_eq!(cred.level, LadderLevel::Runtime);
        assert_eq!(cred.kind, CredKind::ApiKey);
    }

    #[test]
    fn oauth_is_bearer_kind() {
        let mut c = ctx();
        c.oauth_token = Some("tok".into());
        let cred = resolve_credential(&c).expect("resolve");
        assert_eq!(cred.kind, CredKind::BearerToken);
    }

    #[test]
    fn empty_ladder_resolves_none() {
        assert!(resolve_credential(&ctx()).is_none());
    }

    /// Compile-time check of the public API: `Credential` exposes only
    /// access material — no refresh field can exist here.
    #[test]
    fn credential_has_no_refresh_surface() {
        let cred = Credential {
            access: "a".into(),
            kind: CredKind::ApiKey,
            level: LadderLevel::Env,
        };
        // Destructure exhaustively: any added field would break this.
        let Credential {
            access,
            kind: _,
            level: _,
        } = cred;
        assert_eq!(access, "a");
    }

    #[test]
    fn env_file_parsing_minimal() {
        let mut map = std::collections::HashMap::new();
        map.insert("PRE".to_owned().into(), "kept".into());
        parse_env_file(
            "# comment\nFOO=bar\nQUOTED=\"double v\"\nSQ='single'\nEMPTY=\nBAD KEY=x\nNUL=a\0b\nPRE=overwritten\n",
            &mut map,
        );
        assert_eq!(map.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert_eq!(map.get("QUOTED").map(|s| s.as_str()), Some("double v"));
        assert_eq!(map.get("SQ").map(|s| s.as_str()), Some("single"));
        assert_eq!(map.get("EMPTY").map(|s| s.as_str()), Some(""));
        assert!(!map.contains_key("BAD KEY"));
        assert_eq!(map.get("NUL").map(|s| s.as_str()), Some("ab"));
        // Existing key not overwritten.
        assert_eq!(map.get("PRE").map(|s| s.as_str()), Some("kept"));
    }
}
