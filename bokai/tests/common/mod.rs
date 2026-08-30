//! Shared helpers for the KFX substrate tests.

/// FNV-1a, 64-bit.
///
/// Chosen over `DefaultHasher` because the value is checked into a test:
pub fn digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Digest a sequence of already-canonical lines.
pub fn digest_lines(lines: impl IntoIterator<Item = String>) -> u64 {
    digest(&lines.into_iter().collect::<Vec<_>>().join("\n"))
}
