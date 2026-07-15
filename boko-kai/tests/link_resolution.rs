//! Test link resolution for different formats.

use boko::Book;
use sha1_smol::Sha1;
use std::path::Path;

fn sha1_hex(bytes: &[u8]) -> String {
    Sha1::from(bytes).hexdigest()
}
// NOTE: epictetus.azw3 had a fragment-bearing TOC, so this used to assert that
// TOC entries gain fragments + unique hrefs after resolve_links. The 人間失格.azw3
// fixture has a *file-level* TOC (第三の手記 and 一/二 all point into the same
// part, with no '#' anchors), so that invariant has no analog here. azw3 import +
// HUFF decode is covered by tests/azw3_huffcdic.rs; the unique-href invariant
// stays covered by the EPUB/MOBI/KFX tests below.
#[test]
fn test_azw3_toc_resolves_with_titles() {
    let path = "tests/fixtures/[太宰 治] 人間失格.azw3";
    let mut book = Book::open(path).expect("Should open AZW3");
    book.resolve_links().expect("Should resolve links");

    let toc = book.toc();
    assert!(
        toc.iter().any(|e| e.title == "はしがき"),
        "AZW3 TOC should contain はしがき, got {:?}",
        toc.iter().map(|e| &e.title).collect::<Vec<_>>()
    );
}

/// Helper to collect all TOC hrefs recursively.
fn collect_toc_hrefs(entries: &[boko::model::TocEntry], hrefs: &mut Vec<String>) {
    for entry in entries {
        hrefs.push(entry.href.clone());
        collect_toc_hrefs(&entry.children, hrefs);
    }
}

/// Helper to assert all TOC entries have unique hrefs.
fn assert_unique_toc_hrefs(toc: &[boko::model::TocEntry], format_name: &str) {
    use std::collections::HashMap;

    let mut all_hrefs = Vec::new();
    collect_toc_hrefs(toc, &mut all_hrefs);

    let mut href_counts: HashMap<&String, usize> = HashMap::new();
    for href in &all_hrefs {
        *href_counts.entry(href).or_default() += 1;
    }
    let unique_count = href_counts.len();
    assert_eq!(
        all_hrefs.len(),
        unique_count,
        "{}: Every TOC entry should have a unique href",
        format_name
    );
}

#[test]
fn test_epub_toc_resolution() {
    let path = "tests/fixtures/[太宰 治] 人間失格.epub";
    let mut book = Book::open(path).expect("Should open EPUB");
    let _ = book.resolve_links().expect("Should resolve links");

    assert_unique_toc_hrefs(book.toc(), "EPUB");
}

#[test]
fn test_mobi_toc_resolution() {
    let path = "tests/fixtures/[太宰 治] 人間失格.mobi";
    let mut book = Book::open(path).expect("Should open MOBI");
    let _ = book.resolve_links().expect("Should resolve links");

    assert_unique_toc_hrefs(book.toc(), "MOBI");
}

#[test]
fn test_kfx_toc_resolution() {
    let path = "tests/fixtures/[太宰 治] 人間失格.kfx";
    let mut book = Book::open(path).expect("Should open KFX");
    let _ = book.resolve_links().expect("Should resolve links");

    assert_unique_toc_hrefs(book.toc(), "KFX");
}

#[test]
fn test_kfx_asset_is_binary_media_with_expected_hash() {
    let path = "tests/fixtures/[太宰 治] 人間失格.kfx";
    let mut book = Book::open(path).expect("Should open KFX");

    // 人間失格 has a single image asset — the cover JPEG. The asset list
    // carries exported filenames (shared with the mechanical route), so the
    // cover appears under its `cover.<ext>` rename.
    assert!(
        book.list_assets()
            .iter()
            .any(|p| p == Path::new("cover.jpeg")),
        "Expected cover.jpeg to be listed as a KFX asset, got {:?}",
        book.list_assets()
    );

    let bytes = book
        .load_asset(Path::new("cover.jpeg"))
        .expect("Expected asset cover.jpeg to load");

    // Must be the real media payload (JPEG), not an Ion metadata struct.
    assert!(
        !bytes.starts_with(&[0xE0, 0x01, 0x00, 0xEA]),
        "Expected binary media bytes for cover.jpeg, got Ion metadata payload ({} bytes)",
        bytes.len()
    );
    assert!(
        bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "Expected cover.jpeg to be a JPEG"
    );
    assert_eq!(bytes.len(), 32316, "cover.jpeg payload size");
    assert_eq!(
        sha1_hex(bytes.as_slice()),
        "2180e85c65eaa649e770645853c1c94bc4f77618",
        "Unexpected SHA-1 for cover.jpeg"
    );

    // Raw entity addressing (`#<id>`) still works for direct extraction and
    // returns the identical payload.
    let by_id = book
        .load_asset(Path::new("#880"))
        .expect("Expected asset #880 to load");
    assert_eq!(sha1_hex(by_id.as_slice()), sha1_hex(bytes.as_slice()));
}
