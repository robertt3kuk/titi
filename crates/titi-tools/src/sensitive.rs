//! Files that hold credentials and must never reach the model.
//!
//! Read-tier tools run without asking, and whatever they return is sent to a
//! remote provider. A `.env` or a private key read "just to look" is a key
//! handed to a third party, so these paths are refused outright rather than
//! gated behind an approval the user might wave through.

use std::path::Path;

/// Directories whose every file is a credential or points at one.
const SECRET_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws"];

/// A file inside a directory that is otherwise harmless.
const SECRET_IN_DIR: &[(&str, &str)] = &[(".kube", "config"), (".docker", "config.json")];

const SECRET_NAMES: &[&str] = &[
    ".env",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    "auth.json",
    "auth.db",
    "credentials",
    "credentials.json",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_ecdsa_sk",
    "id_ed25519_sk",
    // Browser profiles: cookies, saved passwords, and their key store.
    "Cookies",
    "Login Data",
    "cookies.sqlite",
    "logins.json",
    "key4.db",
];

const SECRET_EXTENSIONS: &[&str] = &[
    "pem",
    "key",
    "p12",
    "pfx",
    "jks",
    "keystore",
    "kdbx",
    "keychain",
    "keychain-db",
    "tfstate",
    "ovpn",
];

/// `.env.example` and friends document the keys without holding them.
const ENV_TEMPLATES: &[&str] = &[".example", ".sample", ".template", ".dist"];

/// The built-in list plus what the user added, minus what the user allowed.
///
/// The allow-list comes only from the user's own settings: a cloned repo
/// controls its project config and must not be able to open `.env`.
#[derive(Debug, Clone, Default)]
pub struct SensitivePolicy {
    extra: Vec<String>,
    allow: Vec<String>,
}

impl SensitivePolicy {
    pub fn new(extra: Vec<String>, allow: Vec<String>) -> Self {
        Self { extra, allow }
    }

    pub fn blocks(&self, path: &Path) -> bool {
        if self.allow.iter().any(|pattern| matches(pattern, path)) {
            return false;
        }
        is_sensitive(path) || self.extra.iter().any(|pattern| matches(pattern, path))
    }
}

/// A pattern without `/` is a file name (`*` wildcards allowed); with `/` it
/// must equal the trailing path components, the last of which may use `*`.
fn matches(pattern: &str, path: &Path) -> bool {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();
    let wanted: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let Some((last_wanted, dirs_wanted)) = wanted.split_last() else {
        return false;
    };
    if parts.len() < wanted.len() {
        return false;
    }
    let tail = &parts[parts.len() - wanted.len()..];
    let Some((last, dirs)) = tail.split_last() else {
        return false;
    };
    dirs == dirs_wanted && wildcard(last_wanted, last)
}

/// `*` matches any run of characters; everything else matches itself.
fn wildcard(pattern: &str, text: &str) -> bool {
    let pieces: Vec<&str> = pattern.split('*').collect();
    let Some((first, rest)) = pieces.split_first() else {
        return false;
    };
    let Some(mut remaining) = text.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return remaining.is_empty();
    };
    for piece in middle {
        match remaining.find(piece) {
            Some(at) => remaining = &remaining[at + piece.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
}

/// Whether `path` names a file whose contents are credentials.
pub fn is_sensitive(path: &Path) -> bool {
    let components: Vec<&str> = path
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();
    let Some((name, dirs)) = components.split_last() else {
        return false;
    };
    if dirs.iter().any(|dir| SECRET_DIRS.contains(dir)) {
        return true;
    }
    if let Some(parent) = dirs.last()
        && SECRET_IN_DIR.contains(&(*parent, *name))
    {
        return true;
    }
    if SECRET_NAMES.contains(name) {
        return true;
    }
    if let Some(rest) = name.strip_prefix(".env.") {
        let rest = format!(".{rest}");
        return !ENV_TEMPLATES
            .iter()
            .any(|template| rest.ends_with(template));
    }
    if name.contains(".tfstate.") {
        return true;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SECRET_EXTENSIONS.contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_files_are_sensitive() {
        for path in [
            ".env",
            "app/.env.local",
            ".env.production",
            "id_rsa",
            "home/.ssh/id_ed25519",
            ".ssh/config",
            ".ssh/known_hosts",
            "certs/server.pem",
            "tls/server.key",
            "store.p12",
            ".aws/credentials",
            ".kube/config",
            ".docker/config.json",
            ".netrc",
            ".git-credentials",
            ".npmrc",
            ".pypirc",
            ".pgpass",
            "auth.json",
            "auth.db",
            "credentials.json",
            "Default/Cookies",
            "Default/Login Data",
            "profile/cookies.sqlite",
            "profile/logins.json",
            "profile/key4.db",
            "vault.kdbx",
            "login.keychain-db",
            "infra/terraform.tfstate",
            "vpn/office.ovpn",
            ".gnupg/private-keys-v1.d/x",
        ] {
            assert!(is_sensitive(Path::new(path)), "{path} should be sensitive");
        }
    }

    #[test]
    fn extra_patterns_block_more_files() {
        let policy = SensitivePolicy::new(
            vec![
                "*.sops.yml".into(),
                "deploy/prod.yml".into(),
                "secrets*".into(),
            ],
            Vec::new(),
        );
        for path in [
            "k8s/app.sops.yml",
            "infra/deploy/prod.yml",
            "secrets.txt",
            ".env",
        ] {
            assert!(policy.blocks(Path::new(path)), "{path} should be blocked");
        }
        for path in ["deploy/staging.yml", "prod.yml", "src/main.rs"] {
            assert!(!policy.blocks(Path::new(path)), "{path} should pass");
        }
    }

    #[test]
    fn the_allow_list_opens_only_what_it_names() {
        let policy = SensitivePolicy::new(Vec::new(), vec![".env.test".into()]);
        assert!(!policy.blocks(Path::new("app/.env.test")));
        assert!(policy.blocks(Path::new("app/.env")));
        assert!(policy.blocks(Path::new(".ssh/id_ed25519")));
    }

    #[test]
    fn the_default_policy_is_the_built_in_list() {
        let policy = SensitivePolicy::default();
        assert!(policy.blocks(Path::new(".env")));
        assert!(!policy.blocks(Path::new("README.md")));
    }

    #[test]
    fn ordinary_and_template_files_are_not() {
        for path in [
            "README.md",
            "src/main.rs",
            ".env.example",
            ".env.sample",
            ".env.template",
            "id_ed25519.pub",
            "docs/keys.md",
            "src/auth.rs",
            "environment.rs",
            "keyboard.rs",
            "monkey.rs",
        ] {
            assert!(
                !is_sensitive(Path::new(path)),
                "{path} should not be sensitive"
            );
        }
    }
}
