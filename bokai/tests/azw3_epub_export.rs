//! A real Amazon AZW3 must convert to a *clean* EPUB-3.

use std::io::Cursor;

use bokai::Exporter as _;

#[test]
fn azw3_fixture_converts_to_clean_epub3() {
    let mut book =
        bokai::Book::open("tests/fixtures/[太宰 治] 人間失格.azw3").expect("open AZW3 fixture");

    let mut buf = Cursor::new(Vec::<u8>::new());
    bokai::EpubExporter::new()
        .export(&mut book, &mut buf)
        .expect("azw3 -> epub export");
    let epub = buf.into_inner();

    let report = bokai::validate::source::epub::validate(&epub);
    assert!(
        report.is_clean(),
        "azw3 -> epub must pass EPUB-3 validation (the importer gate):\n{report}"
    );
}
