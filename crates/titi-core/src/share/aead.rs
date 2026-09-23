//! ChaCha20-Poly1305 (RFC 8439) for the share package.
//!
//! The algorithm is not implemented here: this is a thin adapter over the
//! audited RustCrypto `chacha20poly1305` crate, which owns the primitive,
//! its constant-time properties, and its `P_MAX` length checks. The adapter
//! exists only to keep the rest of `share` in plain byte arrays and to turn
//! the crate's opaque error into `None`, so that a failed tag can never be
//! confused with a decryption result.
//!
//! Nothing here inspects or reimplements the cipher. Swapping the crate out
//! again means rewriting this file and nothing else.

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce, Tag};

/// ChaCha20 key length.
pub const KEY_LEN: usize = 32;
/// RFC 8439 nonce length: 96 bits.
pub const NONCE_LEN: usize = 12;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// RFC 8439 `P_MAX`: a 32-bit block counter starting at 1 caps one message
/// 64 bytes short of 256 GiB. The crate enforces this too; the constant is
/// here so callers can report the limit before allocating.
pub const MAX_PLAINTEXT: usize = 64 * (u32::MAX as usize - 1);

fn cipher(key: &[u8; KEY_LEN]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&Key::from(*key))
}

/// Encrypts `plaintext` and authenticates it together with `aad`.
///
/// Returns the ciphertext and its detached tag, or `None` when the message
/// is longer than the algorithm allows.
pub fn seal(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Option<(Vec<u8>, [u8; TAG_LEN])> {
    let mut buffer = plaintext.to_vec();
    let tag = cipher(key)
        .encrypt_inout_detached(&Nonce::from(*nonce), aad, buffer.as_mut_slice().into())
        .ok()?;
    let mut detached = [0u8; TAG_LEN];
    detached.copy_from_slice(&tag);
    Some((buffer, detached))
}

/// Verifies the tag and decrypts. `None` means the key, the nonce, the
/// associated data, or the ciphertext is not the one that was sealed —
/// nothing is returned in that case.
pub fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
    tag: &[u8; TAG_LEN],
) -> Option<Vec<u8>> {
    let mut buffer = ciphertext.to_vec();
    cipher(key)
        .decrypt_inout_detached(
            &Nonce::from(*nonce),
            aad,
            buffer.as_mut_slice().into(),
            &Tag::from(*tag),
        )
        .ok()?;
    Some(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text
            .bytes()
            .filter(|b| b.is_ascii_hexdigit())
            .map(|b| match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                _ => b - b'A' + 10,
            })
            .collect();
        digits.chunks(2).map(|p| (p[0] << 4) | p[1]).collect()
    }

    fn key32(text: &str) -> [u8; 32] {
        let mut key = [0u8; 32];
        key.copy_from_slice(&decode_hex(text));
        key
    }

    fn nonce12(text: &str) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&decode_hex(text));
        nonce
    }

    /// RFC 8439 §2.8.2. The crate is trusted for the algorithm; this pins
    /// the adapter — key, nonce, aad and tag reaching it in the right order
    /// and byte layout.
    #[test]
    fn aead_matches_the_rfc_vector() {
        let key = key32("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let nonce = nonce12("070000004041424344454647");
        let aad = decode_hex("50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you \
only one tip for the future, sunscreen would be it.";

        let (ciphertext, tag) = seal(&key, &nonce, &aad, plaintext).expect("the vector seals");
        assert_eq!(
            ciphertext,
            decode_hex(
                "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
                 3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
                 92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
                 3ff4def08e4b7a9de576d26586cec64b6116"
            )
        );
        assert_eq!(tag.to_vec(), decode_hex("1ae10b594f09e26a7e902ecbd0600691"));

        let opened = open(&key, &nonce, &aad, &ciphertext, &tag).expect("the tag verifies");
        assert_eq!(opened, plaintext);
    }

    /// A multi-block message with associated data, against a vector taken
    /// from an independent ChaCha20-Poly1305 implementation.
    #[test]
    fn aead_matches_an_independent_implementation_over_many_blocks() {
        let key = key32("0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0");
        let nonce = nonce12("000102030405060708090a0b");
        let aad = b"titi-share/v1 chacha20-poly1305 s-1 md";
        let plaintext: Vec<u8> = (0..300).map(|i| (i * 7 % 251) as u8).collect();

        let (ciphertext, tag) = seal(&key, &nonce, aad, &plaintext).expect("seals");
        assert_eq!(
            ciphertext[..32].to_vec(),
            decode_hex("fa8104702d2afa0eaf880c0bfe2e0694f64a95f60911bb5aad81d75ca0a6aef9")
        );
        assert_eq!(tag.to_vec(), decode_hex("c712c851c337f1cc17447177ff056871"));
    }

    #[test]
    fn open_rejects_a_changed_tag_ciphertext_or_aad() {
        let key = key32("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let nonce = nonce12("070000004041424344454647");
        let aad = b"titi-share v1";
        let (ciphertext, tag) = seal(&key, &nonce, aad, b"secret transcript").expect("seals");

        let mut wrong_tag = tag;
        wrong_tag[0] ^= 1;
        assert!(open(&key, &nonce, aad, &ciphertext, &wrong_tag).is_none());

        let mut wrong_ct = ciphertext.clone();
        wrong_ct[0] ^= 1;
        assert!(open(&key, &nonce, aad, &wrong_ct, &tag).is_none());

        assert!(open(&key, &nonce, b"titi-share v2", &ciphertext, &tag).is_none());

        let mut wrong_key = key;
        wrong_key[31] ^= 1;
        assert!(open(&wrong_key, &nonce, aad, &ciphertext, &tag).is_none());

        let mut wrong_nonce = nonce;
        wrong_nonce[0] ^= 1;
        assert!(open(&key, &wrong_nonce, aad, &ciphertext, &tag).is_none());
    }

    /// Lengths around the 16-byte tag and 64-byte keystream boundaries, so a
    /// padding or offset mistake in the adapter would show up.
    #[test]
    fn round_trip_holds_at_block_boundaries() {
        let key = key32("0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0");
        let nonce = nonce12("000102030405060708090a0b");
        for len in [0usize, 1, 15, 16, 17, 63, 64, 65, 127, 128, 129, 200] {
            let plaintext: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let aad: Vec<u8> = (0..len % 19).map(|i| i as u8).collect();
            let (ciphertext, tag) = seal(&key, &nonce, &aad, &plaintext).expect("seals");
            assert_eq!(ciphertext.len(), len);
            assert_eq!(
                open(&key, &nonce, &aad, &ciphertext, &tag).expect("opens"),
                plaintext,
                "length {len}"
            );
        }
    }
}
