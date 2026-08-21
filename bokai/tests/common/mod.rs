//! Shared helpers for the KFX substrate tests.
//!
//! With one implementation there is nothing to compare against, so these hold
//! it to the question it can still answer: **did this change move the
//! substrate?** A pinned digest says loudly when it does.
//!
//! The digest cannot say the substrate is *correct*, only that it is the same
//! one — which is what protects stored `(eid, offset)` annotations from
//! silently re-resolving. Update a pin only for a substrate change you intend.

/// FNV-1a, 64-bit.
///
/// Chosen over `DefaultHasher` because the value is checked into a test:
/// `DefaultHasher`'s output is explicitly not stable across Rust releases, so
/// pinning it would turn a toolchain upgrade into a test failure.
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
