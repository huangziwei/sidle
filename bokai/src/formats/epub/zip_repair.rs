//! Repair for EPUBs that carry a spurious ZIP64 extended-information field in
//! every entry.

const ZIP64_ID: u16 = 0x0001;
/// Replacement id for a neutralized ZIP64 field. Outside the 0x0000–0x001f
/// PKWARE-reserved range and not assigned to any known third-party field, so
/// every conforming reader (the `zip` crate, Apple Books, Calibre) skips it.
const NEUTRALIZED_ID: u16 = 0x4B41;
const SENTINEL32: u32 = 0xFFFF_FFFF;

const SIG_LOCAL: u32 = 0x0403_4b50; // PK\x03\x04
const SIG_CENTRAL: u32 = 0x0201_4b50; // PK\x01\x02
const SIG_EOCD: u32 = 0x0605_4b50; // PK\x05\x06
const SIG_ZIP64_EOCD: u32 = 0x0606_4b50; // PK\x06\x06
const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50; // PK\x06\x07

fn ru16(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}
fn ru32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
}
fn ru64(b: &[u8], p: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[p..p + 8]);
    u64::from_le_bytes(a)
}

/// Find the classic end-of-central-directory record by scanning back from the
/// end over the 64 KiB the trailing comment can occupy.
fn find_eocd(b: &[u8]) -> Option<usize> {
    if b.len() < 22 {
        return None;
    }
    let min = b.len().saturating_sub(22 + 0xffff);
    let mut p = b.len() - 22;
    loop {
        if ru32(b, p) == SIG_EOCD {
            return Some(p);
        }
        if p <= min {
            return None;
        }
        p -= 1;
    }
}

/// Locate the central directory as `(offset, entry_count)`, following the ZIP64
/// EOCD when the classic record carries `0xFFFF` / `0xFFFFFFFF` sentinels.
fn locate_central_directory(b: &[u8]) -> Option<(usize, usize)> {
    let eocd = find_eocd(b)?;
    let entries16 = ru16(b, eocd + 10);
    let cd_off32 = ru32(b, eocd + 16);
    if entries16 != 0xFFFF && cd_off32 != SENTINEL32 {
        return Some((cd_off32 as usize, entries16 as usize));
    }
    // The ZIP64 EOCD locator sits 20 bytes before the classic record.
    let locator = eocd.checked_sub(20)?;
    if ru32(b, locator) != SIG_ZIP64_LOCATOR {
        return None;
    }
    let z64 = ru64(b, locator + 8) as usize;
    if z64 + 56 > b.len() || ru32(b, z64) != SIG_ZIP64_EOCD {
        return None;
    }
    let entries = ru64(b, z64 + 32) as usize;
    let cd_off = ru64(b, z64 + 48) as usize;
    Some((cd_off, entries))
}

/// Walk one header's extra-field region and relabel spurious ZIP64 fields.
/// `zip64_needed` is true when a 32-bit size/offset of the owning header is the
/// `0xFFFFFFFF` sentinel — i.e. the ZIP64 field is real and must be preserved.
fn relabel_extra(buf: &mut [u8], start: usize, len: usize, zip64_needed: bool) -> usize {
    let Some(end) = start.checked_add(len).filter(|&e| e <= buf.len()) else {
        return 0;
    };
    let mut relabeled = 0;
    let mut i = start;
    while i + 4 <= end {
        let id = ru16(buf, i);
        let size = ru16(buf, i + 2) as usize;
        if id == ZIP64_ID && !zip64_needed {
            buf[i..i + 2].copy_from_slice(&NEUTRALIZED_ID.to_le_bytes());
            relabeled += 1;
        }
        // Advance past this field even when we leave it alone, so a real ZIP64
        // field followed by another field is still scanned.
        match i.checked_add(4 + size) {
            Some(next) => i = next,
            None => break,
        }
    }
    relabeled
}

/// Relabel every spurious ZIP64 extended-information field in `bytes` (both
pub fn neutralize_spurious_zip64(bytes: &[u8]) -> Option<Vec<u8>> {
    let (cd_offset, entries) = locate_central_directory(bytes)?;
    let mut out = bytes.to_vec();
    let mut p = cd_offset;
    let mut relabeled = 0;

    for _ in 0..entries {
        if p + 46 > out.len() || ru32(&out, p) != SIG_CENTRAL {
            break;
        }
        let comp = ru32(&out, p + 20);
        let uncomp = ru32(&out, p + 24);
        let name_len = ru16(&out, p + 28) as usize;
        let extra_len = ru16(&out, p + 30) as usize;
        let comment_len = ru16(&out, p + 32) as usize;
        let local_off = ru32(&out, p + 42);

        // The central header's ZIP64 field is real when any of its 32-bit
        // size/offset fields is maxed out.
        let central_needs_zip64 =
            comp == SENTINEL32 || uncomp == SENTINEL32 || local_off == SENTINEL32;
        let central_extra = p + 46 + name_len;
        relabeled += relabel_extra(&mut out, central_extra, extra_len, central_needs_zip64);

        // Follow the (correct, 32-bit) offset to the matching local header and
        // clean its extra field too, so the persisted archive is fully sane.
        let lo = local_off as usize;
        if lo + 30 <= out.len() && ru32(&out, lo) == SIG_LOCAL {
            let l_comp = ru32(&out, lo + 18);
            let l_uncomp = ru32(&out, lo + 22);
            let l_name = ru16(&out, lo + 26) as usize;
            let l_extra = ru16(&out, lo + 28) as usize;
            let local_needs_zip64 = l_comp == SENTINEL32 || l_uncomp == SENTINEL32;
            relabeled += relabel_extra(&mut out, lo + 30 + l_name, l_extra, local_needs_zip64);
        }

        p = central_extra + extra_len + comment_len;
    }

    (relabeled > 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Append a 24-byte spurious ZIP64 field (id 0x0001) whose third u64 — the
    /// relative-header-offset slot the `zip` crate wrongly trusts — is garbage.
    fn push_spurious_zip64(buf: &mut Vec<u8>) {
        buf.extend_from_slice(&ZIP64_ID.to_le_bytes());
        buf.extend_from_slice(&24u16.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes()); // uncompressed (ignored)
        buf.extend_from_slice(&1u64.to_le_bytes()); // compressed (ignored)
        buf.extend_from_slice(&0xDEAD_BEEFu64.to_le_bytes()); // bogus offset
    }

    /// A one-entry archive that mirrors the real-world defect: a stored file
    /// with a spurious ZIP64 field in both headers, addressed through a ZIP64
    /// EOCD while the classic EOCD is all sentinels.
    fn craft_malformed_zip() -> Vec<u8> {
        let name = b"m";
        let content = b"Z";
        let mut buf = Vec::new();

        // Local header at offset 0.
        buf.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        buf.extend_from_slice(&0u32.to_le_bytes()); // mod time+date
        buf.extend_from_slice(&0u32.to_le_bytes()); // crc (unread)
        buf.extend_from_slice(&1u32.to_le_bytes()); // compressed
        buf.extend_from_slice(&1u32.to_le_bytes()); // uncompressed
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(&28u16.to_le_bytes()); // extra len (4 + 24)
        buf.extend_from_slice(name);
        push_spurious_zip64(&mut buf);
        buf.extend_from_slice(content);

        // Central directory.
        let cd_offset = buf.len() as u32;
        buf.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes()); // version made
        buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // method
        buf.extend_from_slice(&0u32.to_le_bytes()); // mod time+date
        buf.extend_from_slice(&0u32.to_le_bytes()); // crc
        buf.extend_from_slice(&1u32.to_le_bytes()); // compressed
        buf.extend_from_slice(&1u32.to_le_bytes()); // uncompressed
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(&28u16.to_le_bytes()); // extra len
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
        buf.extend_from_slice(&0u16.to_le_bytes()); // disk start
        buf.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        buf.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        buf.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        buf.extend_from_slice(name);
        push_spurious_zip64(&mut buf);
        let cd_size = buf.len() as u32 - cd_offset;

        // ZIP64 EOCD record.
        let z64_off = buf.len() as u64;
        buf.extend_from_slice(&SIG_ZIP64_EOCD.to_le_bytes());
        buf.extend_from_slice(&44u64.to_le_bytes()); // record size - 12
        buf.extend_from_slice(&45u16.to_le_bytes()); // version made
        buf.extend_from_slice(&45u16.to_le_bytes()); // version needed
        buf.extend_from_slice(&0u32.to_le_bytes()); // this disk
        buf.extend_from_slice(&0u32.to_le_bytes()); // cd start disk
        buf.extend_from_slice(&1u64.to_le_bytes()); // entries this disk
        buf.extend_from_slice(&1u64.to_le_bytes()); // entries total
        buf.extend_from_slice(&(cd_size as u64).to_le_bytes());
        buf.extend_from_slice(&(cd_offset as u64).to_le_bytes());

        // ZIP64 EOCD locator.
        buf.extend_from_slice(&SIG_ZIP64_LOCATOR.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // disk with zip64 eocd
        buf.extend_from_slice(&z64_off.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // total disks

        // Classic EOCD, all sentinels (defers to the ZIP64 EOCD).
        buf.extend_from_slice(&SIG_EOCD.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // this disk
        buf.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entries this disk
        buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entries total
        buf.extend_from_slice(&SENTINEL32.to_le_bytes()); // cd size
        buf.extend_from_slice(&SENTINEL32.to_le_bytes()); // cd offset
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
        buf
    }

    /// Whether the stock `zip` reader can resolve the first entry's local
    /// header (the exact step that fails on the malformed input).
    fn first_entry_resolves(bytes: &[u8]) -> bool {
        match zip::ZipArchive::new(Cursor::new(bytes.to_vec())) {
            Ok(mut a) => a.by_index(0).is_ok(),
            Err(_) => false,
        }
    }

    #[test]
    fn repairs_spurious_zip64_for_the_zip_crate() {
        let bad = craft_malformed_zip();
        assert!(
            !first_entry_resolves(&bad),
            "stock reader should choke on the bogus per-entry ZIP64 fields"
        );

        let fixed = neutralize_spurious_zip64(&bad).expect("should repair");
        assert_eq!(fixed.len(), bad.len(), "repair must be offset-preserving");
        assert!(
            first_entry_resolves(&fixed),
            "the repaired archive must read cleanly"
        );
    }

    #[test]
    fn repair_is_idempotent_and_clean_input_is_a_noop() {
        let bad = craft_malformed_zip();
        let fixed = neutralize_spurious_zip64(&bad).expect("first pass repairs");
        // No ZIP64 ids remain, so a second pass finds nothing to do.
        assert!(
            neutralize_spurious_zip64(&fixed).is_none(),
            "a clean archive must not be rewritten"
        );
    }

    #[test]
    fn preserves_a_genuine_zip64_field() {
        // Same archive, but mark the local header's compressed size as the
        // sentinel so the local ZIP64 field is legitimately required. Its id
        // must survive; the central field (still spurious) is relabeled.
        let mut bad = craft_malformed_zip();
        // Local header compressed-size field is at offset 18.
        bad[18..22].copy_from_slice(&SENTINEL32.to_le_bytes());

        let fixed = neutralize_spurious_zip64(&bad).expect("central field still spurious");
        // The local header's ZIP64 id (right after the 1-byte name at offset
        // 30) is preserved.
        let local_extra_id = ru16(&fixed, 30 + 1);
        assert_eq!(local_extra_id, ZIP64_ID, "a real ZIP64 field must be kept");
    }

    #[test]
    fn non_zip_input_is_ignored() {
        assert!(neutralize_spurious_zip64(b"").is_none());
        assert!(neutralize_spurious_zip64(b"not a zip at all").is_none());
    }
}
