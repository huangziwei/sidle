//! KDF SQLite "fingerprint" stripping.
//!
//! On-device Scribe `.nbk` files are SQLite databases with a 1024-byte
//! *fingerprint* record spliced in at offset 1024 and repeating every 1 MB of
//! payload (signature `\xfa\x50\x0a\x5f`). The fingerprint corrupts SQLite's
//! page 1, so stock SQLite rejects the file with "malformed database schema"
//! until the records are removed. This is intrinsic to the format, not a copy
//! artifact — a clean file (no signature at offset 1024) passes through
//! untouched. Verbatim port of kfxlib's `SQLiteFingerprintWrapper`
//! (kfxlib's `kpf_container.py`).

const SIGNATURE: [u8; 4] = [0xfa, 0x50, 0x0a, 0x5f];
const OFFSET: usize = 1024;
const RECORD_LEN: usize = 1024;
/// Distance between successive fingerprints = `DATA_RECORD_LEN * DATA_RECORD_COUNT`.
const STRIDE: usize = 1024 * 1024;

/// Remove all KDF SQLite fingerprint records, returning clean SQLite bytes.
/// No-op when the signature is absent at offset 1024 (already-clean file).
pub fn strip_fingerprints(mut data: Vec<u8>) -> Vec<u8> {
    if data.len() < OFFSET + RECORD_LEN || data[OFFSET..OFFSET + 4] != SIGNATURE {
        return data;
    }

    let mut offset = OFFSET;
    while data.len() >= offset + RECORD_LEN && data[offset..offset + 4] == SIGNATURE {
        data.drain(offset..offset + RECORD_LEN);
        offset += STRIDE;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_signature_is_noop() {
        let data = b"SQLite format 3\0 and some more bytes".to_vec();
        let orig = data.clone();
        assert_eq!(strip_fingerprints(data), orig);
    }

    #[test]
    fn strips_single_fingerprint() {
        // 1024 bytes of "real" data, then a 1024-byte fingerprint, then more.
        let mut data = vec![0xAAu8; OFFSET];
        let mut fp = vec![0u8; RECORD_LEN];
        fp[0..4].copy_from_slice(&SIGNATURE);
        data.extend_from_slice(&fp);
        data.extend_from_slice(&[0xBBu8; 512]);

        let out = strip_fingerprints(data);
        assert_eq!(out.len(), OFFSET + 512);
        assert_eq!(&out[0..OFFSET], &[0xAAu8; OFFSET][..]);
        assert_eq!(&out[OFFSET..], &[0xBBu8; 512][..]);
    }
}
