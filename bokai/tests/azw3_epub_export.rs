//! A real Amazon AZW3 must convert to a *clean* EPUB-3.
//!
//! This is the gate an importing application applies: run
//! `bokai::validate::source::epub` on the freshly-synthesized EPUB and reject
//! the conversion if it isn't clean. Two separate bugs each broke this path for
//! `[太宰 治] 人間失格.azw3`:
//!
//!  1. A HUFF/CDIC `mincode`/`maxcode` off-by-one decoded the body to
//!     intermittent mojibake (see `azw3_huffcdic.rs`).
//!  2. The KF8 source carries a verbatim
//!     `<link rel="alternate stylesheet" href="../styles/a00301_h.css">` (the
//!     Aozora horizontal-writing-mode sheet, never embedded as a flow). The
//!     `..` both points at a missing file and escapes the OPF root — two
//!     EPUB-3 violations. While the text was garbage the href was mojibake'd,
//!     so the validator never saw it; fixing the decode surfaced it.
//!
//! Validating the whole conversion guards both at once — and anything else
//! that would make a consumer reject the book.

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
