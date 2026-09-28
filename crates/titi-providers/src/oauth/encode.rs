//! Hand-rolled `base64url` and `application/x-www-form-urlencoded` codecs plus
//! query-string building.
//!
//! The surface is small (~60 lines) and every property that matters is pinned
//! by tests against the RFC 4648 and RFC 7636 vectors, so a crate would add a
//! dependency without buying correctness.

use crate::oauth::OAuthError;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
/// RFC 3986 recommends uppercase hex; every decoder accepts both.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Unpadded base64url (RFC 4648 §5) — the alphabet PKCE and JWT segments use.
pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[(triple >> 18) as usize & 0x3f]));
        out.push(char::from(ALPHABET[(triple >> 12) as usize & 0x3f]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[(triple >> 6) as usize & 0x3f]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[triple as usize & 0x3f]));
        }
    }
    out
}

/// Strict decoder: rejects any character outside the base64url alphabet
/// (including `=` padding), an undecodable trailing length and non-zero
/// trailing bits.
pub fn base64url_decode(text: &str) -> Result<Vec<u8>, OAuthError> {
    if text.len() % 4 == 1 {
        return Err(OAuthError::protocol("base64url length is undecodable"));
    }
    let mut out = Vec::with_capacity(text.len().saturating_mul(3) / 4);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let Some(value) = base64url_value(byte) else {
            return Err(OAuthError::protocol("invalid base64url character"));
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1u32 << bits) - 1;
        }
    }
    if bits > 0 && accumulator & ((1u32 << bits) - 1) != 0 {
        return Err(OAuthError::protocol("base64url trailing bits are not zero"));
    }
    Ok(out)
}

fn base64url_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// Percent-encodes a component: RFC 3986 unreserved characters survive, a
/// space becomes `%20`. Used for both query strings and form bodies so a value
/// is never double-encoded differently in the two paths.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[(byte >> 4) as usize]));
            out.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    out
}

/// Lenient inverse of [`percent_encode`] for parsing: `+` is a space and a
/// malformed `%` escape is kept verbatim rather than failing a whole redirect.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `k=v&k=v` with both sides percent-encoded (`application/x-www-form-urlencoded`).
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&percent_encode(key));
        out.push('=');
        out.push_str(&percent_encode(value));
    }
    out
}

/// Appends `pairs` to `url`, choosing `?` or `&` by what the URL already has.
pub fn query_append(url: &str, pairs: &[(&str, &str)]) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{}", form_encode(pairs))
}

/// Splits a query string into decoded pairs, dropping empty segments.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let (key, value) = match segment.split_once('=') {
                Some((key, value)) => (key, value),
                None => (segment, ""),
            };
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_every_length_class() {
        for len in 0..32usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let encoded = base64url_encode(&bytes);
            assert!(!encoded.contains('='), "padding must be absent: {encoded}");
            assert_eq!(base64url_decode(&encoded).expect("decodes"), bytes);
        }
    }

    #[test]
    fn base64url_matches_rfc4648_and_rejects_invalid_input() {
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        assert!(base64url_decode("Zm9vYg==").is_err());
        assert!(base64url_decode("A").is_err());
        assert!(base64url_decode("****").is_err());
        assert!(base64url_decode("Zh").is_err());
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(
            form_encode(&[("scope", "openid profile")]),
            "scope=openid%20profile"
        );
        assert_eq!(percent_encode("a/b?c&d=e+f"), "a%2Fb%3Fc%26d%3De%2Bf");
        assert_eq!(percent_decode("a%2Fb%3Fc%26d%3De%2Bf"), "a/b?c&d=e+f");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%zz"), "100%zz");
    }

    #[test]
    fn query_append_keeps_an_existing_query() {
        assert_eq!(
            query_append("https://x/y", &[("a", "1")]),
            "https://x/y?a=1"
        );
        assert_eq!(
            query_append("https://x/y?z=0", &[("a", "1")]),
            "https://x/y?z=0&a=1"
        );
    }

    #[test]
    fn parse_query_decodes_pairs_in_order() {
        assert_eq!(
            parse_query("code=ab%20cd&state=ff&empty="),
            vec![
                ("code".to_owned(), "ab cd".to_owned()),
                ("state".to_owned(), "ff".to_owned()),
                ("empty".to_owned(), String::new()),
            ]
        );
    }
}
