//! Secrets never reach the memory index.
//!
//! A memory is injected into the system prompt of every later turn, so a key
//! stored once is a key shown forever. The value is replaced before the write;
//! the fact that a key existed is kept, because "the deploy token was rotated"
//! is worth remembering and the token is not.

use regex::Regex;
use std::path::{Path, PathBuf};
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

/// `redact`, plus IP addresses and the user's own server names, for text
/// about to leave for a provider.
///
/// Tool output can carry the address of a server the user runs; that is
/// theirs to keep. Loopback and the unspecified address say nothing about
/// anyone and stay, so "listening on 127.0.0.1" and "bound to ::1" still read.
pub fn redact_for_model(text: &str) -> Redaction {
    redact_for_model_with_hosts(text, user_hostnames())
}

/// The provider-bound redaction with an explicit host deny-list. The call site
/// in the engine cannot pass the list, so [`redact_for_model`] loads it from the
/// agent directory; tests reach the pure masking through here.
pub fn redact_for_model_with_hosts(text: &str, hosts: &[String]) -> Redaction {
    let secrets = redact(text);
    let mut removed = secrets.removed;
    // IPv6 first: an IPv4-mapped address such as `::ffff:192.0.2.1` is one span,
    // not a loose IPv4 tail.
    let (text, count) = mask_ipv6(&secrets.text);
    removed += count;
    let (text, count) = mask_ipv4(&text);
    removed += count;
    let (text, count) = mask_hosts(&text, hosts);
    removed += count;
    Redaction { text, removed }
}

const IP_MASK: &str = "[ip]";
const HOST_MASK: &str = "[host]";

/// Server names the user lists under `<agent_dir>/private-hosts`.
const HOSTS_FILE: &str = "private-hosts";

const IPV6_LOOPBACK: [u16; 8] = [0, 0, 0, 0, 0, 0, 0, 1];
const IPV6_UNSPECIFIED: [u16; 8] = [0, 0, 0, 0, 0, 0, 0, 0];

fn mask_ipv4(source: &str) -> (String, usize) {
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    let mut removed = 0;
    for found in IPV4.find_iter(source) {
        let address = found.as_str();
        if !is_address(source, found.start(), found.end(), address)
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
    (out, removed)
}

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

/// A superset of IPv6: any run of hex, colons and dots that carries a colon,
/// plus an optional zone id (`%en0`). [`parse_ipv6`] decides what is really an
/// address, so a Rust path (`std::vec::Vec`), a time (`12:34:56`) or a MAC never
/// masks — the colons are there, the eight-group / `::` structure is not.
static IPV6: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9A-Fa-f.]*:[0-9A-Fa-f:.]*(?:%[0-9A-Za-z._~-]+)?")
        .expect("ipv6 candidate pattern compiles")
});

fn mask_ipv6(source: &str) -> (String, usize) {
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    let mut removed = 0;
    for found in IPV6.find_iter(source) {
        let token = found.as_str();
        // A zone id is masked with the address; without one, trailing sentence
        // punctuation (`2001:db8::1.`) is not part of the address.
        let (address, span_len) = match token.split_once('%') {
            Some((address, zone)) if !zone.is_empty() => (address, token.len()),
            _ => {
                let trimmed = trim_ipv6(token);
                (trimmed, trimmed.len())
            }
        };
        let end = found.start() + span_len;
        let Some(groups) = parse_ipv6(&address.to_ascii_lowercase()) else {
            continue;
        };
        if !ipv6_boundaries_ok(source, found.start(), end) {
            continue;
        }
        if groups == IPV6_LOOPBACK || groups == IPV6_UNSPECIFIED {
            continue;
        }
        out.push_str(&source[last..found.start()]);
        out.push_str(IP_MASK);
        last = end;
        removed += 1;
    }
    out.push_str(&source[last..]);
    (out, removed)
}

/// Trailing `.` and a lone trailing `:` are punctuation, never part of an
/// address; a trailing `::` is legitimate compression and stays.
fn trim_ipv6(token: &str) -> &str {
    let mut end = token.len();
    while end > 0 {
        let head = &token[..end];
        if head.ends_with('.') || (head.ends_with(':') && !head.ends_with("::")) {
            end -= 1;
        } else {
            break;
        }
    }
    &token[..end]
}

/// An address touching an identifier character is part of that identifier, not a
/// bare address: `std::vec` and `cafe::babeXY` are rejected here.
fn ipv6_boundaries_ok(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    if before.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '%' | '#'))
    {
        return false;
    }
    let after = text[end..].chars().next();
    !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The eight 16-bit groups of a valid IPv6 address, or `None`. A trailing
/// dotted IPv4 (`::ffff:192.0.2.1`) expands into the last two groups.
fn parse_ipv6(text: &str) -> Option<[u16; 8]> {
    if text.is_empty() || text.matches("::").count() > 1 {
        return None;
    }
    if let Some((head, tail)) = text.split_once("::") {
        let head = parse_ipv6_side(head, false)?;
        let tail = parse_ipv6_side(tail, true)?;
        // `::` must stand for at least one omitted group.
        if head.len() + tail.len() >= 8 {
            return None;
        }
        let mut out = [0u16; 8];
        out[..head.len()].copy_from_slice(&head);
        out[8 - tail.len()..].copy_from_slice(&tail);
        Some(out)
    } else {
        let groups = parse_ipv6_side(text, true)?;
        if groups.len() != 8 {
            return None;
        }
        let mut out = [0u16; 8];
        out.copy_from_slice(&groups);
        Some(out)
    }
}

fn parse_ipv6_side(part: &str, allow_ipv4_tail: bool) -> Option<Vec<u16>> {
    if part.is_empty() {
        return Some(Vec::new());
    }
    let tokens: Vec<&str> = part.split(':').collect();
    let mut groups = Vec::with_capacity(tokens.len() + 1);
    for (index, token) in tokens.iter().enumerate() {
        if token.is_empty() {
            return None;
        }
        let last = index == tokens.len() - 1;
        if last && allow_ipv4_tail && token.contains('.') {
            let octets = parse_ipv4_bytes(token)?;
            groups.push(u16::from(octets[0]) << 8 | u16::from(octets[1]));
            groups.push(u16::from(octets[2]) << 8 | u16::from(octets[3]));
        } else {
            if token.len() > 4 || token.contains('.') {
                return None;
            }
            groups.push(u16::from_str_radix(token, 16).ok()?);
        }
    }
    Some(groups)
}

fn parse_ipv4_bytes(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

/// A hostname token: dot-separated labels, each bounded by an alphanumeric so a
/// trailing dot or a leading `.` never enters the match.
static HOST_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)*",
    )
    .expect("hostname pattern compiles")
});

fn mask_hosts(source: &str, hosts: &[String]) -> (String, usize) {
    if hosts.is_empty() {
        return (source.to_owned(), 0);
    }
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    let mut removed = 0;
    for found in HOST_TOKEN.find_iter(source) {
        let name = found.as_str().to_ascii_lowercase();
        if !hosts.iter().any(|host| is_host_or_subdomain(&name, host)) {
            continue;
        }
        out.push_str(&source[last..found.start()]);
        out.push_str(HOST_MASK);
        last = found.end();
        removed += 1;
    }
    out.push_str(&source[last..]);
    (out, removed)
}

/// The whole name, or one of its subdomains — `notexample.invalid` is neither
/// `example.invalid` nor a subdomain of it.
fn is_host_or_subdomain(name: &str, host: &str) -> bool {
    if name == host {
        return true;
    }
    name.len() > host.len()
        && name.ends_with(host)
        && name.as_bytes()[name.len() - host.len() - 1] == b'.'
}

fn user_hostnames() -> &'static [String] {
    static HOSTS: LazyLock<Vec<String>> = LazyLock::new(|| load_hostnames(&agent_dir()));
    &HOSTS
}

/// Reads server names from `<agent_dir>/private-hosts`: one per line, `#` for a
/// comment, blanks ignored. A missing file is not an error — nothing to mask.
fn load_hostnames(agent_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(agent_dir.join(HOSTS_FILE)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The agent directory from the environment only: this crate must not depend on
/// `titi-config`. Mirrors its `$TITI_AGENT_DIR` override and `~/.titi/agent`
/// default.
fn agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TITI_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".titi").join("agent")
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
    fn ipv6_addresses_are_masked_for_the_model() {
        for address in [
            "2001:db8::1",
            "2001:db8:0:0:0:0:0:1",
            "::ffff:192.0.2.1",
            "2001:db8::1%eth0",
        ] {
            let redacted = redact_for_model_with_hosts(&format!("bound to {address} now"), &[]);
            assert!(
                !redacted.text.contains(address),
                "{address} -> {}",
                redacted.text
            );
            assert!(
                redacted.text.contains("bound to [ip] now"),
                "{address} -> {}",
                redacted.text
            );
            assert_eq!(redacted.removed, 1, "{address} -> {}", redacted.text);
        }
    }

    #[test]
    fn ipv6_loopback_and_unspecified_are_kept() {
        let redacted = redact_for_model_with_hosts("listening on ::1 and :: only", &[]);
        assert_eq!(redacted.removed, 0, "{}", redacted.text);
        assert_eq!(redacted.text, "listening on ::1 and :: only");
    }

    #[test]
    fn ipv6_in_brackets_keeps_the_port() {
        let redacted = redact_for_model_with_hosts("dial [2001:db8::1]:443 now", &[]);
        assert!(!redacted.text.contains("2001:db8::1"), "{}", redacted.text);
        assert_eq!(redacted.text, "dial [[ip]]:443 now", "{}", redacted.text);
        assert_eq!(redacted.removed, 1, "{}", redacted.text);
    }

    #[test]
    fn ipv6_lookalikes_are_left_verbatim() {
        for text in [
            "path std::vec::Vec here",
            "map HashMap::<String, i64>::new() call",
            "at 12:34:56 today",
            "rust 1.85.0 release",
            "hash deadbeefcafebabe0123456789abcdef done",
        ] {
            let redacted = redact_for_model_with_hosts(text, &[]);
            assert_eq!(redacted.removed, 0, "{text} -> {}", redacted.text);
            assert_eq!(redacted.text, text, "{text} -> {}", redacted.text);
        }
    }

    #[test]
    fn a_listed_host_and_its_subdomains_are_masked() {
        let hosts = vec!["example.invalid".to_owned()];
        let redacted = redact_for_model_with_hosts(
            "ssh deploy@api.example.invalid and curl example.invalid/health",
            &hosts,
        );
        assert!(
            !redacted.text.contains("example.invalid"),
            "{}",
            redacted.text
        );
        assert!(redacted.text.contains("deploy@[host]"), "{}", redacted.text);
        assert!(redacted.text.contains("[host]/health"), "{}", redacted.text);
        assert_eq!(redacted.removed, 2, "{}", redacted.text);
    }

    #[test]
    fn a_host_that_only_looks_similar_is_kept() {
        let hosts = vec!["example.invalid".to_owned()];
        let redacted = redact_for_model_with_hosts(
            "notexample.invalid and example.invalid.attacker.invalid stay",
            &hosts,
        );
        assert_eq!(
            redacted.text, "notexample.invalid and example.invalid.attacker.invalid stay",
            "{}",
            redacted.text
        );
        assert_eq!(redacted.removed, 0, "{}", redacted.text);
    }

    #[test]
    fn hostnames_load_from_the_agent_dir_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_hostnames(dir.path()).is_empty());
        std::fs::write(
            dir.path().join(HOSTS_FILE),
            "# my servers\nExample.Invalid\n\n  api.example.invalid  \n",
        )
        .unwrap();
        assert_eq!(
            load_hostnames(dir.path()),
            vec![
                "example.invalid".to_owned(),
                "api.example.invalid".to_owned()
            ]
        );
    }

    #[test]
    fn prose_about_tokens_passes() {
        let redacted = redact("the token check uses < not <=");
        assert_eq!(redacted.removed, 0);
    }
}
