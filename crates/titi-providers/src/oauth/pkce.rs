//! PKCE (RFC 7636, `S256`) and the anti-CSRF `state` value.
//!
//! Randomness comes from `/dev/urandom` — the same crateless source
//! `titi-core`'s share module uses — because neither value may ever repeat
//! across logins.

use sha2::{Digest, Sha256};

use crate::oauth::OAuthError;
use crate::oauth::encode;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// 96 random bytes rendered base64url: the 128-character verifier RFC 7636
/// §4.1 allows at its maximum entropy.
pub fn verifier() -> Result<String, OAuthError> {
    let mut bytes = [0u8; 96];
    fill_random(&mut bytes)?;
    Ok(encode::base64url_encode(&bytes))
}

/// `code_challenge` = base64url(SHA-256(ASCII(verifier))), method `S256`.
pub fn challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    encode::base64url_encode(&digest)
}

/// 32 lowercase hex characters (16 random bytes), compared verbatim on the
/// callback and matched as-is by the provider.
pub fn state() -> Result<String, OAuthError> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes)?;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[(byte >> 4) as usize]));
        out.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    Ok(out)
}

fn fill_random(buffer: &mut [u8]) -> Result<(), OAuthError> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom").map_err(|e| OAuthError::Entropy {
        message: e.to_string(),
    })?;
    file.read_exact(buffer).map_err(|e| OAuthError::Entropy {
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B: the verifier is hashed over its ASCII text, not
    /// over the base64url alphabet's numeric value.
    #[test]
    fn rfc7636_appendix_b_verifier_yields_the_published_challenge() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_state_and_challenge_have_the_declared_shape() {
        let sample = verifier().expect("entropy");
        assert_eq!(sample.len(), 128);
        assert!(
            sample
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(challenge(&sample).len(), 43);

        let state = state().expect("entropy");
        assert_eq!(state.len(), 32);
        assert!(
            state
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );

        // Two draws of a fresh 96-byte verifier must not collide.
        assert_ne!(sample, verifier().expect("entropy"));
    }
}
