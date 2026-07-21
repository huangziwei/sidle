//! Describing a book's assets instead of loading them.
//!
//! A renderer that streams a book needs each asset's identity and size before
//! any bytes arrive, and must not pay to decode the whole set at open. This
//! pins that a described build names exactly the same assets as a loading one,
//! carries the declared pixel sizes, and cannot be mistaken for something
//! shippable.

use std::io::Cursor;

use bokai::Book;
use bokai::export::{EpubExporter, PackageOptions, build_package};
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

fn package(path: &str, opts: PackageOptions) -> Option<bokai::export::EpubPackage> {
    let kfx = std::fs::read(path).ok()?;
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    Some(build_package(&mut book, opts, &|_, _, _, _| {}).expect("build the package"))
}

#[test]
fn describing_assets_names_the_same_set_as_loading_them() {
    let Some(loaded) = package(REFLOWABLE, PackageOptions::container()) else {
        return; // fixture not present in this checkout
    };
    let described = package(REFLOWABLE, PackageOptions::rendered()).expect("described build");

    let loaded_hrefs: Vec<&str> = loaded.assets.iter().map(|a| a.href.as_str()).collect();
    let described_hrefs: Vec<&str> = described.assets.iter().map(|a| a.href.as_str()).collect();
    assert_eq!(
        loaded_hrefs, described_hrefs,
        "a described build must account for the same assets, in the same order"
    );
    assert!(
        !loaded_hrefs.is_empty(),
        "this fixture has no assets — the comparison would pass vacuously"
    );

    assert!(
        described.assets.iter().all(|a| a.bytes.is_none()),
        "a described build must not carry bytes"
    );
    assert!(
        loaded.assets.iter().all(|a| a.bytes.is_some()),
        "a loading build must carry bytes for every asset it names"
    );
    assert!(
        described.assets.iter().any(|a| a.width.is_some()),
        "the source declares image dimensions; a described build should carry them"
    );
}

#[test]
fn a_described_package_still_builds_its_documents() {
    let Some(described) = package(REFLOWABLE, PackageOptions::rendered()) else {
        return;
    };
    assert!(!described.documents.is_empty(), "no documents were built");
    assert!(
        described
            .documents
            .iter()
            .any(|d| d.xhtml.contains("data-eid")),
        "a rendered build carries source element ids"
    );
    assert!(!described.css.is_empty(), "the stylesheet is still built");
}

#[test]
fn a_described_package_cannot_be_written_as_a_container() {
    let Some(described) = package(REFLOWABLE, PackageOptions::rendered()) else {
        return;
    };
    // Shipping this would produce a container whose manifest promises files it
    // does not hold — the write must fail rather than emit one.
    let mut out = Cursor::new(Vec::new());
    let err = EpubExporter::new()
        .write_package(&described, &mut out)
        .expect_err("writing a described package must fail");
    assert!(
        err.to_string().contains("describe"),
        "the error should say why: {err}"
    );
}
