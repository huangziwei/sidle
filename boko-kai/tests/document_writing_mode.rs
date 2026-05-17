//! Regression: the document-level writing-mode in the `document_data`
//! fragment must reflect the book's actual orientation.
//!
//! Per-style writing_mode was always correct, so text rendered vertically.
//! But the device reads the **document-level** value (in the `document_data`
//! fragment) to decide which layout-controls to expose. With it hardcoded to
//! `horizontal_tb`, the KOA2's Layout panel showed horizontal Orientation
//! icons and offered an Alignment toggle that doesn't apply to vertical text.
//! Fix: snapshot the dominant per-style writing_mode before draining the
//! style registry and reuse it for `document_data`.

use boko::{Book, Format};

/// Ion struct field for `writing_mode: <symbol>` in the base KFX symbol space:
///   field name `writing_mode` = sym 560 → VarUInt `0x04 0xB0`
///   value type = symbol, length 2 → type descriptor `0x72`
///   then 2-byte magnitude (big-endian) for the value symbol ID
const WRITING_MODE_FIELD_PREFIX: &[u8] = &[0x04, 0xB0, 0x72];

/// `horizontal_tb` = sym 557 = `0x02 0x2D`
const HORIZONTAL_TB_SYMBOL_MAG: &[u8] = &[0x02, 0x2D];

/// `vertical_rl` = sym 559 = `0x02 0x2F`
const VERTICAL_RL_SYMBOL_MAG: &[u8] = &[0x02, 0x2F];

#[test]
fn document_writing_mode_matches_book_orientation() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let books_dir = std::path::PathBuf::from(manifest_dir).parent().unwrap().join("books");
    let epub_path = books_dir.join("闇.epub");
    if !epub_path.exists() {
        eprintln!("skipping: reference EPUB not available at {:?}", epub_path);
        return;
    }
    let mut book = Book::open(&epub_path).expect("open 闇.epub");
    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut buf).expect("export kfx");
    let kfx = buf.into_inner();

    // Count writing_mode fields by their value symbol. 闇 is a vertical book,
    // so every emission should be `vertical_rl`; the buggy code emitted
    // `horizontal_tb` only once (the document_data fragment), with all other
    // style fragments correct. Asserting zero horizontal_tb entries catches
    // the document_data regression specifically.
    let h_pat: Vec<u8> = [WRITING_MODE_FIELD_PREFIX, HORIZONTAL_TB_SYMBOL_MAG].concat();
    let v_pat: Vec<u8> = [WRITING_MODE_FIELD_PREFIX, VERTICAL_RL_SYMBOL_MAG].concat();
    let h_hits = kfx.windows(h_pat.len()).filter(|w| *w == h_pat).count();
    let v_hits = kfx.windows(v_pat.len()).filter(|w| *w == v_pat).count();
    assert!(v_hits > 0, "no writing_mode: vertical_rl found at all");
    assert_eq!(
        h_hits, 0,
        "writing_mode: horizontal_tb was emitted {} time(s) in a vertical book — likely document_data fragment regression",
        h_hits
    );
}
