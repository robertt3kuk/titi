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
        assert_eq!(fingerprint(b"pub fn a() {}\n"), fingerprint(b"pub fn a() {}\n"));
        assert_ne!(fingerprint(b"pub fn a() {}\n"), fingerprint(b"pub fn a() {}\n\n"));
        // Same length, one character apart: the case the content hash exists
        // for, since size alone cannot tell these apart.
        assert_ne!(fingerprint(b"pub fn aa() {}\n"), fingerprint(b"pub fn ab() {}\n"));
        // Reordering lines keeps every byte, so a byte-multiset hash would
        // call these equal; FNV-1a does not.
        assert_ne!(fingerprint(b"one\ntwo\n"), fingerprint(b"two\none\n"));
        // Empty input is still a value, not an error: a file that cannot be
        // read parses as empty, and that must compare equal to itself.
        assert_eq!(fingerprint(b""), fingerprint(b""));
    }
}
