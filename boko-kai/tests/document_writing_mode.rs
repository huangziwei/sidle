//! Regression: the **book-level** writing-mode in the `document_data` fragment
//! must reflect the book's actual orientation.
//!
//! The device reads the document-level value (in the `document_data` fragment)
//! to decide which layout controls to expose. With it hardcoded to
//! `horizontal_tb`, the KOA2's Layout panel showed horizontal Orientation icons
//! and offered an Alignment toggle that doesn't apply to vertical text. Fix:
//! snapshot the dominant per-style writing_mode before draining the style
//! registry and reuse it for `document_data` (see
//! `export/kfx.rs::dominant_writing_mode_from_ir`).
//!
//! IMPORTANT: writing-mode is a **per-style** property — a vertical book
//! legitimately contains horizontal-tb runs (colophon, Latin passages, some
//! tables), and those correctly emit `writing_mode: horizontal_tb` as per-style
//! overrides (see `export/kfx.rs` ~L993). 闇 is exactly such a book (its source
//! CSS declares both vertical-rl and horizontal-tb). So this test must NOT
//! assert "no horizontal_tb anywhere" — an earlier version did, and went stale
//! the moment per-style override emission landed. We isolate and check the
//! `document_data` fragment's writing-mode specifically.

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

/// In the `document_data` fragment — and only there — the `writing_mode` field
/// is immediately followed by `selection: enabled` (see
/// `build_document_data_fragment`). That adjacency isolates the **book-level**
/// writing-mode from the per-style override fields, which are never followed by
/// `selection`. Encoding: field `selection` = sym 436 → VarUInt `0x03 0xB4`;
/// value `enabled` = sym 441, symbol type len 2 → `0x72 0x01 0xB9`.
const SELECTION_ENABLED_SUFFIX: &[u8] = &[0x03, 0xB4, 0x72, 0x01, 0xB9];

#[test]
fn document_writing_mode_matches_book_orientation() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let books_dir = std::path::PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .join("books");
    let epub_path = books_dir.join("闇.epub");
    if !epub_path.exists() {
        eprintln!("skipping: reference EPUB not available at {:?}", epub_path);
        return;
    }
    let mut book = Book::open(&epub_path).expect("open 闇.epub");
    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut buf).expect("export kfx");
    let kfx = buf.into_inner();

    let count = |pat: &[u8]| kfx.windows(pat.len()).filter(|w| *w == pat).count();

    // The book-level (document_data) writing-mode for 闇 — a vertical book —
    // must be vertical_rl, isolated via the `writing_mode` → `selection:enabled`
    // adjacency that only the document_data fragment has.
    let doc_vertical: Vec<u8> = [
        WRITING_MODE_FIELD_PREFIX,
        VERTICAL_RL_SYMBOL_MAG,
        SELECTION_ENABLED_SUFFIX,
    ]
    .concat();
    let doc_horizontal: Vec<u8> = [
        WRITING_MODE_FIELD_PREFIX,
        HORIZONTAL_TB_SYMBOL_MAG,
        SELECTION_ENABLED_SUFFIX,
    ]
    .concat();
    assert_eq!(
        count(&doc_vertical),
        1,
        "document_data writing-mode should be vertical_rl exactly once in 闇 (a vertical book)"
    );
    assert_eq!(
        count(&doc_horizontal),
        0,
        "document_data writing-mode is horizontal_tb — the book-level orientation regressed"
    );

    // Per-style horizontal_tb overrides are EXPECTED in a vertical book that has
    // horizontal runs (闇 has one), so we deliberately do NOT assert on the
    // global horizontal_tb count — only that vertical_rl is emitted at all.
    let any_vertical: Vec<u8> = [WRITING_MODE_FIELD_PREFIX, VERTICAL_RL_SYMBOL_MAG].concat();
    assert!(
        count(&any_vertical) > 0,
        "no writing_mode: vertical_rl found at all"
    );

    // `spacing_percent_base: width` pins percentage-spacing to the horizontal
    // axis and breaks the Layout > Spacing slider in vertical-rl mode (the
    // device ends up adjusting left/right margins instead of column-to-column
    // line spacing). Encoded as: field `spacing_percent_base` (sym 477,
    // VarUInt 0x03 0xDD) + value symbol `width` (sym 56, type 0x71, mag 0x38).
    // Calibre never emits this field; neither should we.
    let spacing_bug: &[u8] = &[0x03, 0xDD, 0x71, 0x38];
    assert_eq!(
        count(spacing_bug),
        0,
        "spacing_percent_base: width emitted — Layout > Spacing slider will adjust margins instead of line spacing in vertical books"
    );
}
