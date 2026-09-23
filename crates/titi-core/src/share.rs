//! `/share`: a session export sealed under a one-off symmetric key.
//!
//! The package is the part you can hand to anyone — a transcript exported by
//! [`session::export`](crate::session::export), encrypted with
//! ChaCha20-Poly1305 (RFC 8439) by the audited RustCrypto
//! `chacha20poly1305` crate, through the thin adapter in [`aead`]. The key
//! never goes into it: it is
//! returned separately as hex, to travel in the fragment of a link
//! (`…/s/<id>#<key>`), which a browser never sends to a server. Whoever
//! stores the package sees ciphertext and nothing else.
//!
//! The package header (version, algorithm, session id, format) is
//! authenticated as associated data, so a host cannot relabel a package to
//! pass one session off as another: the tag stops verifying.
//!
//! Spec: `docs/PLAN.md` (P3-4).

pub mod aead;

use std::fmt;
use std::fs::File;
use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::session::SessionError;
use crate::session::export::ExportFormat;
use crate::session::store::SessionStore;

/// Package format version.
pub const VERSION: u32 = 1;
/// Algorithm identifier written into the package and checked when opening.
pub const ALGORITHM: &str = "chacha20-poly1305";

/// Errors surfaced while sharing or opening a session.
#[derive(Debug)]
pub enum ShareError {
    Session(SessionError),
    Json(serde_json::Error),
    /// The system entropy source could not be read; no key is made up.
    Entropy(std::io::Error),
    /// A key, nonce, or tag that is not valid hex of the expected length.
    BadKey(String),
    /// The package is structurally broken: bad base64, wrong field widths.
    Corrupt(String),
    /// The package was not produced by this version or algorithm.
    Unsupported(String),
    /// The tag did not verify: wrong key, or the package was altered.
    /// Nothing was decrypted.
    NotAuthentic,
    /// The transcript is too long for a single ChaCha20 message.
    TooLarge(usize),
}

impl fmt::Display for ShareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareError::Session(e) => write!(f, "share session: {e}"),
            ShareError::Json(e) => write!(f, "share json: {e}"),
            ShareError::Entropy(e) => write!(f, "share entropy: {e}"),
            ShareError::BadKey(what) => write!(f, "share key: {what}"),
            ShareError::Corrupt(what) => write!(f, "share package: {what}"),
            ShareError::Unsupported(what) => write!(f, "share package: unsupported {what}"),
            ShareError::NotAuthentic => {
                write!(f, "share package: wrong key or altered package")
            }
            ShareError::TooLarge(len) => {
                write!(f, "share package: {len} bytes exceeds the message limit")
            }
        }
    }
}

impl std::error::Error for ShareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShareError::Session(e) => Some(e),
            ShareError::Json(e) => Some(e),
            ShareError::Entropy(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SessionError> for ShareError {
    fn from(error: SessionError) -> Self {
        ShareError::Session(error)
    }
}

impl From<serde_json::Error> for ShareError {
    fn from(error: serde_json::Error) -> Self {
        ShareError::Json(error)
    }
}

/// The symmetric key of one share. Never serialized with the package.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareKey([u8; aead::KEY_LEN]);

impl ShareKey {
    /// Draws a fresh key from the operating system's entropy pool.
    pub fn generate() -> Result<Self, ShareError> {
        let mut key = [0u8; aead::KEY_LEN];
        fill_random(&mut key)?;
        Ok(Self(key))
    }

    pub fn from_bytes(bytes: [u8; aead::KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The key as lowercase hex — what goes after `#` in a share link.
    pub fn to_hex(&self) -> String {
        encode_hex(&self.0)
    }

    /// Parses a key back from its hex form.
    pub fn parse(hex: &str) -> Result<Self, ShareError> {
        let bytes = decode_hex(hex.trim()).map_err(ShareError::BadKey)?;
        let key: [u8; aead::KEY_LEN] = bytes
            .try_into()
            .map_err(|_| ShareError::BadKey(format!("expected {} bytes", aead::KEY_LEN)))?;
        Ok(Self(key))
    }
}

/// Keys are redacted in logs and error reports: printing one would defeat
/// keeping it out of the package.
impl fmt::Debug for ShareKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ShareKey(redacted)")
    }
}

impl Drop for ShareKey {
    fn drop(&mut self) {
        self.0.fill(0);
        // Best effort against the store being optimised away; without an
        // audited zeroize crate this is as far as safe Rust reaches.
        std::hint::black_box(&self.0);
    }
}

/// A shareable, encrypted session. Safe to store or send anywhere: without
/// the [`ShareKey`] it is ciphertext and an authentication tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePackage {
    pub version: u32,
    pub algorithm: String,
    pub session_id: String,
    /// Export format of the plaintext inside: `md` or `jsonl`.
    pub format: String,
    /// 96-bit nonce, hex.
    pub nonce: String,
    /// Encrypted transcript, base64.
    pub ciphertext: String,
    /// Poly1305 tag, hex.
    pub tag: String,
}

impl SharePackage {
    pub fn to_json(&self) -> Result<String, ShareError> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(text: &str) -> Result<Self, ShareError> {
        Ok(serde_json::from_str(text)?)
    }

    /// The header bytes bound to the ciphertext as associated data.
    fn associated_data(&self) -> Vec<u8> {
        header_bytes(
            self.version,
            &self.algorithm,
            &self.session_id,
            &self.format,
        )
    }
}

fn header_bytes(version: u32, algorithm: &str, session_id: &str, format: &str) -> Vec<u8> {
    format!("titi-share/v{version} {algorithm} {session_id} {format}").into_bytes()
}

/// Exports a session and seals it under a freshly generated key.
///
/// The key is returned alongside the package and is the only way back in —
/// it is not stored anywhere by this call.
pub fn share_session(
    store: &SessionStore,
    session_id: &str,
    format: ExportFormat,
) -> Result<(SharePackage, ShareKey), ShareError> {
    let export = store.export(session_id, format)?;
    let key = ShareKey::generate()?;
    let package = seal_export(&export, session_id, format, &key)?;
    Ok((package, key))
}

/// Seals an already rendered export. Each call draws a new nonce, so sharing
/// the same session twice never produces the same ciphertext.
pub fn seal_export(
    export: &str,
    session_id: &str,
    format: ExportFormat,
    key: &ShareKey,
) -> Result<SharePackage, ShareError> {
    if export.len() > aead::MAX_PLAINTEXT {
        return Err(ShareError::TooLarge(export.len()));
    }
    let mut nonce = [0u8; aead::NONCE_LEN];
    fill_random(&mut nonce)?;
    let format = format.extension().to_owned();
    let aad = header_bytes(VERSION, ALGORITHM, session_id, &format);
    let (ciphertext, tag) = aead::seal(&key.0, &nonce, &aad, export.as_bytes())
        .ok_or(ShareError::TooLarge(export.len()))?;
    Ok(SharePackage {
        version: VERSION,
        algorithm: ALGORITHM.to_owned(),
        session_id: session_id.to_owned(),
        format,
        nonce: encode_hex(&nonce),
        ciphertext: encode_base64(&ciphertext),
        tag: encode_hex(&tag),
    })
}

/// Opens a package with its key and returns the exported transcript.
///
/// Fails with [`ShareError::NotAuthentic`] when the key is wrong or any part
/// of the package — ciphertext or header — was altered.
pub fn open_package(package: &SharePackage, key: &ShareKey) -> Result<String, ShareError> {
    if package.version != VERSION {
        return Err(ShareError::Unsupported(format!(
            "version {}",
            package.version
        )));
    }
    if package.algorithm != ALGORITHM {
        return Err(ShareError::Unsupported(format!(
            "algorithm {}",
            package.algorithm
        )));
    }
    let nonce: [u8; aead::NONCE_LEN] = decode_hex(&package.nonce)
        .map_err(ShareError::Corrupt)?
        .try_into()
        .map_err(|_| ShareError::Corrupt("nonce length".to_owned()))?;
    let tag: [u8; aead::TAG_LEN] = decode_hex(&package.tag)
        .map_err(ShareError::Corrupt)?
        .try_into()
        .map_err(|_| ShareError::Corrupt("tag length".to_owned()))?;
    let ciphertext = decode_base64(&package.ciphertext).map_err(ShareError::Corrupt)?;

    let plaintext = aead::open(
        &key.0,
        &nonce,
        &package.associated_data(),
        &ciphertext,
        &tag,
    )
    .ok_or(ShareError::NotAuthentic)?;
    String::from_utf8(plaintext).map_err(|_| ShareError::Corrupt("not utf-8".to_owned()))
}

/// The export format a package carries, when this build understands it.
pub fn package_format(package: &SharePackage) -> Option<ExportFormat> {
    ExportFormat::parse(&package.format)
}

/// Fills `buffer` from the operating system's entropy pool.
///
/// `/dev/urandom` is the portable interface available without a crate; it
/// blocks only until the pool is initialised and never returns short reads
/// in practice, but a short read is still treated as a failure rather than
/// silently leaving zeros in the key.
fn fill_random(buffer: &mut [u8]) -> Result<(), ShareError> {
    let mut file = File::open("/dev/urandom").map_err(ShareError::Entropy)?;
    file.read_exact(buffer).map_err(ShareError::Entropy)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[(byte >> 4) as usize]));
        out.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err("odd number of hex digits".to_owned());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_digit(pair[0])?;
        let lo = hex_digit(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("{:?} is not a hex digit", char::from(byte))),
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let packed = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(B64[(packed >> 18) as usize & 63]));
        out.push(char::from(B64[(packed >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(B64[(packed >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(B64[packed as usize & 63])
        } else {
            '='
        });
    }
    out
}

fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let symbols: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !symbols.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of four".to_owned());
    }
    let chunks = symbols.len() / 4;
    let mut out = Vec::with_capacity(chunks * 3);
    for (index, chunk) in symbols.chunks_exact(4).enumerate() {
        let mut packed = 0u32;
        let mut padding = 0usize;
        for (position, &symbol) in chunk.iter().enumerate() {
            let value = match symbol {
                b'A'..=b'Z' => symbol - b'A',
                b'a'..=b'z' => symbol - b'a' + 26,
                b'0'..=b'9' => symbol - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                // Padding closes the final chunk only, and only its tail.
                b'=' if index + 1 == chunks && position >= 2 => {
                    padding += 1;
                    0
                }
                _ => return Err(format!("{:?} is not base64", char::from(symbol))),
            };
            if padding > 0 && symbol != b'=' {
                return Err("base64 padding in the middle of a group".to_owned());
            }
            packed = (packed << 6) | value as u32;
        }
        out.push((packed >> 16) as u8);
        if padding < 2 {
            out.push((packed >> 8) as u8);
        }
        if padding < 1 {
            out.push(packed as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entry::Role;
    use crate::session::{SessionMeta, store::SessionStore};

    fn store_with_session() -> (tempfile::TempDir, SessionStore, String) {
        let dir = tempfile::tempdir().expect("a temp agent dir");
        let store = SessionStore::new(dir.path()).expect("the store opens");
        let session_id = store
            .create(SessionMeta {
                title: Some("shared chat".to_owned()),
                ..SessionMeta::default()
            })
            .expect("a session");
        store
            .append(&session_id, Role::User, "how do I share this?")
            .expect("a user entry");
        store
            .append(&session_id, Role::Assistant, "run /share")
            .expect("an assistant entry");
        (dir, store, session_id)
    }

    #[test]
    fn a_shared_session_opens_back_to_the_same_export() {
        let (_dir, store, session_id) = store_with_session();
        for format in [ExportFormat::Markdown, ExportFormat::Jsonl] {
            let expected = store.export(&session_id, format).expect("an export");
            let (package, key) = share_session(&store, &session_id, format).expect("a package");

            assert_eq!(package.session_id, session_id);
            assert_eq!(package_format(&package), Some(format));
            assert!(
                !package.ciphertext.contains("how do I share this?"),
                "the transcript must not survive in the package"
            );

            assert_eq!(open_package(&package, &key).expect("it opens"), expected);
        }
    }

    #[test]
    fn a_package_survives_json_and_a_key_survives_hex() {
        let (_dir, store, session_id) = store_with_session();
        let (package, key) =
            share_session(&store, &session_id, ExportFormat::Markdown).expect("a package");
        let expected = open_package(&package, &key).expect("it opens");

        // What a link actually carries: the package as JSON, the key as hex.
        let shipped = SharePackage::from_json(&package.to_json().expect("json"))
            .expect("the package parses back");
        let fragment = ShareKey::parse(&key.to_hex()).expect("the key parses back");
        assert_eq!(shipped, package);
        assert_eq!(
            open_package(&shipped, &fragment).expect("it opens"),
            expected
        );
    }

    #[test]
    fn the_wrong_key_reveals_nothing() {
        let (_dir, store, session_id) = store_with_session();
        let (package, _key) =
            share_session(&store, &session_id, ExportFormat::Jsonl).expect("a package");

        let other = ShareKey::generate().expect("another key");
        assert!(matches!(
            open_package(&package, &other),
            Err(ShareError::NotAuthentic)
        ));
    }

    #[test]
    fn an_altered_package_is_refused_rather_than_decrypted() {
        let (_dir, store, session_id) = store_with_session();
        let (package, key) =
            share_session(&store, &session_id, ExportFormat::Markdown).expect("a package");

        // Ciphertext flipped.
        let mut tampered = package.clone();
        let mut raw = decode_base64(&tampered.ciphertext).expect("base64");
        raw[0] ^= 1;
        tampered.ciphertext = encode_base64(&raw);
        assert!(matches!(
            open_package(&tampered, &key),
            Err(ShareError::NotAuthentic)
        ));

        // Header relabelled: the metadata is authenticated too, so a host
        // cannot pass this package off as another session.
        let mut relabelled = package.clone();
        relabelled.session_id = "someone-elses-session".to_owned();
        assert!(matches!(
            open_package(&relabelled, &key),
            Err(ShareError::NotAuthentic)
        ));

        let mut reformatted = package.clone();
        reformatted.format = "jsonl".to_owned();
        assert!(matches!(
            open_package(&reformatted, &key),
            Err(ShareError::NotAuthentic)
        ));

        // A package from a build we do not understand is refused before any
        // decryption is attempted.
        let mut future = package.clone();
        future.version = VERSION + 1;
        assert!(matches!(
            open_package(&future, &key),
            Err(ShareError::Unsupported(_))
        ));
        let mut other_algorithm = package;
        other_algorithm.algorithm = "rot13".to_owned();
        assert!(matches!(
            open_package(&other_algorithm, &key),
            Err(ShareError::Unsupported(_))
        ));
    }

    #[test]
    fn sharing_twice_never_repeats_a_ciphertext() {
        let (_dir, store, session_id) = store_with_session();
        let (first, first_key) =
            share_session(&store, &session_id, ExportFormat::Markdown).expect("a package");
        let (second, second_key) =
            share_session(&store, &session_id, ExportFormat::Markdown).expect("a package");

        assert_ne!(first_key.to_hex(), second_key.to_hex());
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
        assert_eq!(
            open_package(&first, &first_key).expect("first opens"),
            open_package(&second, &second_key).expect("second opens")
        );
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = ShareKey::from_bytes([7u8; aead::KEY_LEN]);
        let shown = format!("{key:?}");
        assert!(!shown.contains(&key.to_hex()), "{shown}");
        assert_eq!(shown, "ShareKey(redacted)");
    }

    #[test]
    fn a_malformed_key_is_rejected() {
        assert!(matches!(
            ShareKey::parse("nothex"),
            Err(ShareError::BadKey(_))
        ));
        assert!(matches!(ShareKey::parse("ab"), Err(ShareError::BadKey(_))));
        assert!(matches!(ShareKey::parse("abc"), Err(ShareError::BadKey(_))));
        let key = ShareKey::from_bytes([3u8; aead::KEY_LEN]);
        assert_eq!(ShareKey::parse(&key.to_hex()).expect("parses"), key);
    }

    #[test]
    fn a_corrupt_package_is_reported_not_panicked() {
        let (_dir, store, session_id) = store_with_session();
        let (package, key) =
            share_session(&store, &session_id, ExportFormat::Markdown).expect("a package");

        let mut broken = package.clone();
        broken.ciphertext = "not base64!!".to_owned();
        assert!(matches!(
            open_package(&broken, &key),
            Err(ShareError::Corrupt(_))
        ));

        let mut short_nonce = package;
        short_nonce.nonce = "00ff".to_owned();
        assert!(matches!(
            open_package(&short_nonce, &key),
            Err(ShareError::Corrupt(_))
        ));
    }

    #[test]
    fn base64_and_hex_round_trip_every_tail_length() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 256) as u8).collect();
            let encoded = encode_base64(&bytes);
            assert_eq!(encoded.len() % 4, 0);
            assert_eq!(decode_base64(&encoded).expect("base64 decodes"), bytes);
            assert_eq!(decode_hex(&encode_hex(&bytes)).expect("hex decodes"), bytes);
        }
        assert!(decode_base64("AAAA=AAA").is_err());
        assert!(decode_base64("AAA").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
