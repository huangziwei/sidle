//! Shared helpers for the KFX substrate tests.
//!
//! These tests used to assert "the IR route agrees with the mechanical port",
//! which was the right question while a second implementation existed to
//! answer it. It is gone, and keeping one alive purely to be compared against
//! would defeat the migration — so the question becomes the one a single
//! implementation can still be held to: **did this change move the substrate?**
//!
//! A pinned digest answers that. It cannot say the substrate is *correct* (only
//! the corpus-wide parity runs against the port could, and they passed 435/435
//! before it was deleted); it says loudly when the substrate *changes*, which
//! is what protects stored `(eid, offset)` annotations from silently
//! re-resolving.

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
