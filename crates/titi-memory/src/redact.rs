//! Secrets never reach the memory index.
//!
//! A memory is injected into the system prompt of every later turn, so a key
//! stored once is a key shown forever. The value is replaced before the write;
//! the fact that a key existed is kept, because "the deploy token was rotated"
//! is worth remembering and the token is not.

use regex::Regex;
use std::sync::LazyLock;

/// What was removed, so the caller can say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    pub removed: usize,
}

/// Replaces secret-shaped spans with a fixed mask.
pub fn redact(text: &str) -> Redaction {
    let mut out = text.to_owned();
    let mut removed = 0;
    for pattern in PATTERNS.iter() {
        let count = pattern.find_iter(&out).count();
        if count > 0 {
            out = pattern.replace_all(&out, MASK).into_owned();
            removed += count;
        }
    }
    Redaction { text: out, removed }
}

const MASK: &str = "[redacted]";

/// `redact`, plus IPv4 addresses, for text about to leave for a provider.
///
/// Tool output can carry the address of a server the user runs; that is
/// theirs to keep. Loopback and the unspecified address say nothing about
/// anyone and stay, so "listening on 127.0.0.1" still reads.
pub fn redact_for_model(text: &str) -> Redaction {
    let secrets = redact(text);
    let source = secrets.text;
    let mut removed = secrets.removed;
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    for found in IPV4.find_iter(&source) {
        let address = found.as_str();
        if !is_address(&source, found.start(), found.end(), address)
            || address.starts_with("127.")
            || address == "0.0.0.0"
        {
            continue;
        }
        out.push_str(&source[last..found.start()]);
        out.push_str(IP_MASK);
        last = found.end();
        removed += 1;
    }
    out.push_str(&source[last..]);
    Redaction { text: out, removed }
}

const IP_MASK: &str = "[ip]";

static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}").expect("ipv4 pattern compiles")
});

/// Octets within 0–255, and not part of a longer dotted number such as a
/// five-part version or `1300.1.2.3`.
fn is_address(text: &str, start: usize, end: usize, address: &str) -> bool {
    if !address.split('.').all(|octet| octet.parse::<u8>().is_ok()) {
        return false;
    }
    let before = text[..start].chars().next_back();
    if before.is_some_and(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    let mut after = text[end..].chars();
    match after.next() {
        Some(c) if c.is_ascii_digit() => false,
        Some('.') => !after.next().is_some_and(|c| c.is_ascii_digit()),
        _ => true,
    }
}

/// Shapes that are a secret and almost nothing else.
///
/// A bare hex string is not here: commit hashes and colours look the same.
/// The patterns require a prefix a person would recognise as a credential.
static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // OpenAI, Anthropic, GitHub (classic + fine-grained), Slack, Stripe.
        r"\bsk-[A-Za-z0-9_\-]{16,}",
        r"\bsk-ant-[A-Za-z0-9_\-]{16,}",
        r"\bgh[pousr]_[A-Za-z0-9]{20,}",
        r"\bgithub_pat_[A-Za-z0-9_]{16,}",
        r"\bxox[baprs]-[A-Za-z0-9\-]{10,}",
        r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}",
        // AWS access key id.
        r"\bAKIA[0-9A-Z]{16}",
        // PEM blocks and JWTs.
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
        // An assignment whose value is long enough to be a token.
        r#"(?i)\b(?:api[_-]?key|token|secret|password|passwd)\b\s*[:=]\s*['"]?[A-Za-z0-9_\-\./+]{12,}"#,
    ]
    .into_iter()
    .map(|p| Regex::new(p).expect("secret pattern compiles"))
    .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_masked_and_the_sentence_survives() {
        let redacted = redact("deployed with sk-proj-abc1234567890xyz and it worked");
        assert_eq!(redacted.removed, 1);
        assert!(!redacted.text.contains("abc1234567890xyz"));
        assert!(redacted.text.contains("deployed with"));
        assert!(redacted.text.contains("[redacted]"));
    }

    #[test]
    fn a_commit_hash_is_not_a_secret() {
        let hash = "deadbeefcafebabe0123456789abcdef";
        let redacted = redact(&format!("fixed in {hash}"));
        assert_eq!(redacted.removed, 0);
        assert!(redacted.text.contains(hash));
    }

    #[test]
    fn an_assigned_token_is_masked() {
        let redacted = redact("token: supersecretvalue12345");
        assert_eq!(redacted.removed, 1);
        assert!(!redacted.text.contains("supersecretvalue12345"));
    }

    #[test]
    fn for_the_model_an_address_is_masked_and_loopback_is_kept() {
        let redacted = redact_for_model(
            "ssh root@203.0.113.7 then curl 198.51.100.20:8080, local 127.0.0.1 and 0.0.0.0",
        );
        assert!(!redacted.text.contains("203.0.113.7"), "{}", redacted.text);
        assert!(
            !redacted.text.contains("198.51.100.20"),
            "{}",
            redacted.text
        );
        assert!(redacted.text.contains("root@[ip]"), "{}", redacted.text);
        assert!(redacted.text.contains("127.0.0.1"), "{}", redacted.text);
        assert!(redacted.text.contains("0.0.0.0"), "{}", redacted.text);
        assert_eq!(redacted.removed, 2);
    }

    #[test]
    fn for_the_model_secrets_are_masked_as_well() {
        let redacted = redact_for_model("KEY=sk-test-0000000000000000 on 10.0.0.5");
        assert!(!redacted.text.contains("sk-test-0000000000000000"));
        assert!(!redacted.text.contains("10.0.0.5"));
        assert_eq!(redacted.removed, 2);
    }

    #[test]
    fn for_the_model_versions_that_are_not_addresses_pass() {
        let redacted = redact_for_model("rust 1.85.0, semver 2.0.0, build 300.1.2.3");
        assert_eq!(redacted.removed, 0, "{}", redacted.text);
    }

    #[test]
    fn prose_about_tokens_passes() {
        let redacted = redact("the token check uses < not <=");
        assert_eq!(redacted.removed, 0);
    }
}
