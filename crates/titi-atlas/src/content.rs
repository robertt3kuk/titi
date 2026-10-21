//! Fingerprint of a file's bytes, for the refresh pre-filter.
//!
//! The pre-filter compares size and mtime, which is one `stat` per file and no
//! read at all, but a `stat` moves for reasons that are not a content change:
//! an editor that rewrites identical bytes, `touch`, a `git checkout` that
//! restores the same blob, a formatter with nothing to do. A file that passes
//! the pre-filter is read anyway — parsing it is the expensive step, the read
//! is not — so the fingerprint is computed over bytes already in hand and
//! turns "mtime moved" into "content moved" without a second read.
//!
//! The hash is FNV-1a, 64-bit, hand-rolled: no dependency, deterministic, and
//! the only consumer compares it against the previously recorded value for the
//! *same path* on the same machine. It is not a security boundary and is not
//! used for one, so a cryptographic digest would buy nothing for the cost of a
//! crate. A collision would skip one re-parse of a file whose content did
//! change; at 2⁻⁶⁴ per comparison that is not a class of event worth an extra
//! compile unit, and the next content change of any kind heals it.

/// FNV-1a 64-bit over `bytes`.
///
/// The constants are the published ones; the mix is a xor of the byte into
/// the low 8 bits followed by a multiply, which is why it reads every byte and
/// distinguishes one-character edits.
pub(crate) fn fingerprint(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    #[test]
    fn identical_bytes_share_a_fingerprint_and_edits_do_not() {
        assert_eq!(
            fingerprint(b"pub fn a() {}\n"),
            fingerprint(b"pub fn a() {}\n")
        );
        assert_ne!(
            fingerprint(b"pub fn a() {}\n"),
            fingerprint(b"pub fn a() {}\n\n")
        );
        // Same length, one character apart: the case the content hash exists
        // for, since size alone cannot tell these apart.
        assert_ne!(
            fingerprint(b"pub fn aa() {}\n"),
            fingerprint(b"pub fn ab() {}\n")
        );
        // Reordering lines keeps every byte, so a byte-multiset hash would
        // call these equal; FNV-1a does not.
        assert_ne!(fingerprint(b"one\ntwo\n"), fingerprint(b"two\none\n"));
        // Empty input is still a value, not an error: a file that cannot be
        // read parses as empty, and that must compare equal to itself.
        assert_eq!(fingerprint(b""), fingerprint(b""));
    }
}

/// A source file's text, whatever its encoding.
///
/// Parsers and the use-site scan read `&str`. A file with bytes that are not
/// UTF-8 (a Latin-1 comment, a Windows-1252 quote, a Shift-JIS header) used to
/// read as an error and index as empty, which drops every symbol it defines
/// and every edge it carries. Decoding it lossily replaces each bad byte with
/// U+FFFD and costs nothing else: an identifier is ASCII or valid UTF-8 in
/// every language the atlas reads, so what the replacement can touch is
/// comments and string contents, which the parsers do not need exact. Valid
/// UTF-8, nearly every file, is taken without a copy.
pub(crate) fn decode(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned())
}

/// [`decode`] of the file at `path`; a file that cannot be read is empty text.
pub(crate) fn read_source(path: &std::path::Path) -> String {
    std::fs::read(path).map(decode).unwrap_or_default()
}
