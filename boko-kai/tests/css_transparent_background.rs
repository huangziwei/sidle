//! Regression: `background: transparent` must NOT emit an opaque KFX color.
//!
//! KFX color packing hardcodes alpha=0xFF, so any color that flows through
//! the packer becomes opaque. Previously `parse_css_color` returned (0,0,0)
//! for `transparent`, which then got packed as 0xFF000000 = opaque black —
//! painting a solid black box behind every element with `background:
//! transparent`. On the KOA2 this rendered TOC links as solid black blocks
//! covering the link text entirely. The fix: drop the property when the
//! source value is `transparent` so the device's default (no fill) applies.

use boko::{Book, Format};

#[test]
fn transparent_background_does_not_emit_opaque_black() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let epub_path = std::path::PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .join("books/闇.epub");
    if !epub_path.exists() {
        eprintln!("skipping: reference EPUB not available at {:?}", epub_path);
        return;
    }
    let mut book = Book::open(&epub_path).expect("open 闇.epub");
    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut buf).expect("export kfx");
    let kfx = buf.into_inner();

    // The bug emits the packed ARGB value 0xFF000000 for `transparent` because
    // the color packer hardcodes alpha=0xFF. In Ion binary that 4-byte uint
    // serialises as type-descriptor 0x24 (positive int, 4-byte magnitude)
    // followed by the big-endian bytes FF 00 00 00. Preceded by the VLQ
    // symbol-ID for the relevant style field:
    //   text_background_color = sym 21 -> VLQ 0x95
    //   fill_color            = sym 70 -> VLQ 0xC6
    // Either signature in the file is a regression.
    let bg_bug: &[u8] = &[0x95, 0x24, 0xFF, 0x00, 0x00, 0x00];
    let fill_bug: &[u8] = &[0xC6, 0x24, 0xFF, 0x00, 0x00, 0x00];

    let bg_hits = kfx.windows(bg_bug.len()).filter(|w| *w == bg_bug).count();
    let fill_hits = kfx
        .windows(fill_bug.len())
        .filter(|w| *w == fill_bug)
        .count();

    assert_eq!(
        bg_hits, 0,
        "text_background_color = 0xFF000000 emitted {} times (transparent → black regression)",
        bg_hits
    );
    assert_eq!(
        fill_hits, 0,
        "fill_color = 0xFF000000 emitted {} times (transparent → black regression)",
        fill_hits
    );
}
