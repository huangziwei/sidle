//! End-to-end tests for `.kfx-zip` ingestion.
//!
//! Amazon ships KFX books as a zip of several `.kfx` containers (the main
//! storyline file plus `metadata.kfx` and one or more `CR!*.kfx` resource
//! containers). boko-kai handles them by merging into a single in-memory KFX
//! container (via `kfx::merge`) before importing — see the merge module for
//! the design.

use std::io::Write;
use std::path::Path;

use boko::Book;

/// Pack the given single-container `.kfx` file into a one-entry `.kfx-zip`
/// inside a tempdir. Used to exercise the kfx-zip path without needing a real
/// multi-container Amazon bundle.
fn pack_single_kfx_as_zip(kfx_path: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let zip_path = dir.path().join("bundle.kfx-zip");
    let zip_file = std::fs::File::create(&zip_path).expect("create zip");
    let mut writer = zip::ZipWriter::new(zip_file);
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer.start_file("book.kfx", opts).expect("start_file");
    let kfx_bytes = std::fs::read(kfx_path).expect("read source kfx");
    writer.write_all(&kfx_bytes).expect("write kfx bytes");
    writer.finish().expect("finish zip");
    dir
}

/// A one-entry `.kfx-zip` round-trips identically to opening the original
/// `.kfx` directly. Proves the merge path preserves metadata and spine.
#[test]
fn kfx_zip_with_single_container_matches_plain_kfx() {
    let kfx_path = "tests/fixtures/[太宰 治] 人間失格.kfx";

    let plain = Book::open(kfx_path).expect("open .kfx");
    let plain_title = plain.metadata().title.clone();
    let plain_authors = plain.metadata().authors.clone();
    let plain_spine_len = plain.spine().len();
    let plain_toc_len = plain.toc().len();

    let dir = pack_single_kfx_as_zip(Path::new(kfx_path));
    let zip_path = dir.path().join("bundle.kfx-zip");
    let bundled = Book::open(&zip_path).expect("open .kfx-zip");

    assert_eq!(bundled.metadata().title, plain_title);
    assert_eq!(bundled.metadata().authors, plain_authors);
    assert_eq!(bundled.spine().len(), plain_spine_len);
    assert_eq!(bundled.toc().len(), plain_toc_len);
}

/// `boko::formats::kfx::merge::merge_kfx_zip` produces a self-contained `.kfx` that
/// loads identically to the source bundle. Verifies the merge fast-path used
/// by `boko convert in.kfx-zip out.kfx`.
#[test]
fn merge_produces_loadable_single_kfx() {
    let kfx_path = "tests/fixtures/[太宰 治] 人間失格.kfx";
    let dir = pack_single_kfx_as_zip(Path::new(kfx_path));
    let zip_path = dir.path().join("bundle.kfx-zip");

    let merged_bytes = boko::formats::kfx::merge::merge_kfx_zip(&zip_path).expect("merge");
    assert!(
        merged_bytes.starts_with(b"CONT"),
        "merge output must be a CONT container"
    );

    let merged_path = dir.path().join("merged.kfx");
    std::fs::write(&merged_path, &merged_bytes).expect("write merged");

    let plain = Book::open(kfx_path).expect("open original");
    let merged = Book::open(&merged_path).expect("open merged");
    assert_eq!(merged.metadata().title, plain.metadata().title);
    assert_eq!(merged.spine().len(), plain.spine().len());
    assert_eq!(merged.toc().len(), plain.toc().len());
}

/// The committed Amazon `.kfx-zip` opens to the same book as the monolithic
/// `.kfx` — exercises the real kfx-zip merge path on an actual bundle (not the
/// synthetic single-entry zip the tests above build).
#[test]
fn real_kfx_zip_fixture_matches_plain_kfx() {
    let kfx = "tests/fixtures/[太宰 治] 人間失格.kfx";
    let zip = "tests/fixtures/[太宰 治] 人間失格.kfx-zip";

    let plain = Book::open(kfx).expect("open .kfx");
    let bundled = Book::open(zip).expect("open .kfx-zip");

    assert_eq!(bundled.metadata().title, plain.metadata().title);
    assert_eq!(bundled.metadata().authors, plain.metadata().authors);
    assert_eq!(bundled.spine().len(), plain.spine().len());
    assert_eq!(bundled.toc().len(), plain.toc().len());
}
