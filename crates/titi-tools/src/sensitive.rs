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
