//! Regression: CSS @import inlining must preserve UTF-8 multibyte sequences.
//!
//! Older code copied non-@import content byte-by-byte via `out.push(byte as char)`,
//! which silently re-encoded multibyte UTF-8 as Latin-1-per-byte. The result was
//! double-encoded Japanese font names (`@ＭＳ 明朝` → `@ï¼­ï¼³ ææ`) and any other
//! non-ASCII content sailing past Stylesheet::parse, ending up corrupted in the
//! KFX `font_family` field — which then breaks on-device font matching.

use boko::{Book, Format};

#[test]
fn font_family_with_japanese_survives_round_trip() {
    // Test is anchored to the boko-kai package; the books/ dir is a sibling.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let epub_path = std::path::PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .join("books/夏.epub");
    if !epub_path.exists() {
        eprintln!("skipping: reference EPUB not available at {:?}", epub_path);
        return;
    }
    let mut book = Book::open(epub_path).expect("open 夏.epub");
    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut buf).expect("export kfx");
    let kfx = buf.into_inner();

    // The font_family should contain the real fullwidth Japanese chars,
    // not their double-UTF-8-encoded Latin-1 counterparts.
    let needle = "@ＭＳ 明朝".as_bytes(); // ef bc ad ef bc b3 ...
    let mojibake: &[u8] = b"\xc3\xaf\xc2\xbc\xc2\xad"; // what the bug emitted

    assert!(
        kfx.windows(needle.len()).any(|w| w == needle),
        "expected clean UTF-8 font name to appear in KFX bytes"
    );
    assert!(
        !kfx.windows(mojibake.len()).any(|w| w == mojibake),
        "found double-encoded Latin-1 bytes in KFX — UTF-8 mangling regression"
    );
}
