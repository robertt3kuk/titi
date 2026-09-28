//! Declarative OAuth descriptors.
//!
//! Every constant here is ported from omp's auth rules
//! (`@oh-my-pi/pi-catalog`, MIT, Stencil Labs, Inc.):
//! `src/compat/rules/auth/anthropic.kdl` and
//! `src/compat/rules/auth/openai-codex.kdl`, cross-checked against
//! `@oh-my-pi/pi-ai` `src/registry/oauth/{anthropic,openai-codex}.ts`.
//!
//! The values are protocol facts (client ids, endpoints, scopes, beta
//! headers) that a provider only honours verbatim; the surrounding logic in
//! this module is titi's own.

/// Everything the login flow needs to know about one provider.
#[derive(Debug, Clone, Copy)]
pub struct OAuthProvider {
    /// Stable id, also the first half of a `provider/label` store key.
    pub id: &'static str,
    pub name: &'static str,
    /// Row id written into the auth store (equals [`Self::id`] for both).
    pub store_as: &'static str,
    pub client_id: &'static str,
    pub authorize_url: &'static str,
    pub scopes: &'static [&'static str],
    /// Provider-specific extras appended after the standard authorize params.
    pub authorize_params: &'static [(&'static str, &'static str)],
    pub callback: CallbackSpec,
    pub token: TokenSpec,
    /// Subtracted from `expires_in` so a token is refreshed before it dies.
    pub refresh_skew_secs: i64,
    pub identity: IdentitySource,
    pub instructions: &'static str,
    pub supports_device: bool,
}

/// Loopback callback listener shape.
#[derive(Debug, Clone, Copy)]
pub struct CallbackSpec {
    pub host: &'static str,
    pub port: u16,
    pub path: &'static str,
    /// Pinned redirect URI; when set, the bound port must match it exactly.
    pub redirect_uri: Option<&'static str>,
    /// Whether a busy `port` may fall back to an ephemeral one. A provider
    /// that allowlists the redirect URI must set this to `false`.
    pub port_fallback: bool,
}

/// Token-endpoint request shape.
#[derive(Debug, Clone, Copy)]
pub struct TokenSpec {
    pub url: &'static str,
    pub body: Body,
    /// Extras merged into the exchange parameters; `{state}` is substituted.
    pub params: &'static [(&'static str, &'static str)],
    pub timeout_secs: u64,
    /// Headers sent only on refresh (Claude Code sends them there and not on
    /// the code exchange). `{sdk}` is substituted with the Claude Code SDK
    /// version the fingerprint in `wire` reports.
    pub refresh_headers: &'static [(&'static str, &'static str)],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    Form,
    Json,
}

/// Where the account/org identity slice comes from after a token exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// `GET /api/claude_cli/bootstrap`, with the token response as the first
    /// (optional) source.
    AnthropicBootstrap,
    /// Claims of the `access_token` JWT.
    JwtClaims,
}

const ANTHROPIC: OAuthProvider = OAuthProvider {
    id: "anthropic",
    name: "Anthropic (Claude Pro/Max)",
    store_as: "anthropic",
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    authorize_url: "https://claude.ai/oauth/authorize",
    scopes: &[
        "org:create_api_key",
        "user:profile",
        "user:inference",
        "user:sessions:claude_code",
        "user:mcp_servers",
        "user:file_upload",
    ],
    authorize_params: &[("code", "true")],
    callback: CallbackSpec {
        host: "localhost",
        port: 54545,
        path: "/callback",
        redirect_uri: None,
        port_fallback: true,
    },
    token: TokenSpec {
        url: "https://api.anthropic.com/v1/oauth/token",
        body: Body::Json,
        // Claude Code echoes the CSRF state on the exchange but not on refresh.
        params: &[("state", "{state}")],
        timeout_secs: 30,
        refresh_headers: &[
            ("anthropic-beta", "oauth-2025-04-20"),
            // omp's rule templates the Claude Code SDK version into the
            // Anthropic TypeScript SDK user agent
            // (`pi-catalog/src/compat/rules/auth/anthropic.kdl`).
            (
                "user-agent",
                "anthropic-sdk-typescript/{sdk} userOAuthProvider",
            ),
        ],
    },
    refresh_skew_secs: 300,
    identity: IdentitySource::AnthropicBootstrap,
    instructions: "Complete login in your browser. If the browser cannot reach this machine, \
                   paste the final redirect URL or authorization code when prompted.",
    supports_device: false,
};

const OPENAI_CODEX: OAuthProvider = OAuthProvider {
    id: "openai-codex",
    name: "ChatGPT Plus/Pro (Codex Subscription)",
    store_as: "openai-codex",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    authorize_url: "https://auth.openai.com/oauth/authorize",
    scopes: &[
        "openid",
        "profile",
        "email",
        "offline_access",
        "api.connectors.read",
        "api.connectors.invoke",
    ],
    authorize_params: &[
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "titi"),
    ],
    callback: CallbackSpec {
        host: "localhost",
        port: 1455,
        path: "/auth/callback",
        // OpenAI allowlists this exact URI, so a busy port must fail rather
        // than fall back to an ephemeral one.
        redirect_uri: Some("http://localhost:1455/auth/callback"),
        port_fallback: false,
    },
    token: TokenSpec {
        url: "https://auth.openai.com/oauth/token",
        body: Body::Form,
        params: &[],
        timeout_secs: 15,
        refresh_headers: &[],
    },
    refresh_skew_secs: 0,
    identity: IdentitySource::JwtClaims,
    instructions: "A browser window should open. Complete login to finish.",
    supports_device: true,
};

const BUILTIN: &[OAuthProvider] = &[ANTHROPIC, OPENAI_CODEX];

/// The providers titi can sign into, in presentation order.
pub fn builtin() -> &'static [OAuthProvider] {
    BUILTIN
}

/// Looks a provider up by [`OAuthProvider::id`] or [`OAuthProvider::store_as`].
pub fn find(id: &str) -> Option<&'static OAuthProvider> {
    BUILTIN
        .iter()
        .find(|provider| provider.id == id || provider.store_as == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_providers_are_present_with_their_ported_constants() {
        assert_eq!(builtin().len(), 2);
        let anthropic = find("anthropic").expect("anthropic");
        assert_eq!(anthropic.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(anthropic.authorize_url, "https://claude.ai/oauth/authorize");
        assert_eq!(anthropic.scopes.len(), 6);
        assert_eq!(anthropic.callback.port, 54545);
        assert_eq!(anthropic.callback.path, "/callback");
        assert!(anthropic.callback.port_fallback);
        assert_eq!(anthropic.token.body, Body::Json);
        assert_eq!(anthropic.refresh_skew_secs, 300);
        assert!(!anthropic.supports_device);

        let codex = find("openai-codex").expect("codex");
        assert_eq!(codex.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(
            codex.authorize_url,
            "https://auth.openai.com/oauth/authorize"
        );
        assert_eq!(codex.scopes.len(), 6);
        assert_eq!(codex.callback.port, 1455);
        assert_eq!(codex.callback.path, "/auth/callback");
        assert!(!codex.callback.port_fallback);
        assert_eq!(
            codex.callback.redirect_uri,
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(codex.token.body, Body::Form);
        assert_eq!(codex.token.timeout_secs, 15);
        assert_eq!(codex.refresh_skew_secs, 0);
        assert!(codex.supports_device);

        assert_eq!(find("anthropic").map(|p| p.store_as), Some("anthropic"));
        assert_eq!(
            find("openai-codex").map(|p| p.store_as),
            Some("openai-codex")
        );
        assert!(find("nope").is_none());
    }
}
