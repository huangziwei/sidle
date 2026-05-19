//! Smoke test: parse each real Aozora fixture under `books/aozora/` and
//! print a structural summary. Skips with a message if a fixture is
//! missing — per `feedback_no_gitignored_test_data`, only commit tiny
//! self-contained fixtures, not these multi-megabyte books.

use std::io::Read;
use std::path::Path;

use boko::aozora;

struct ZipContents {
    document: aozora::Document,
    images: Vec<(String, Vec<u8>)>,
}

fn read_zip(zip_path: &Path) -> Option<ZipContents> {
    let file = std::fs::File::open(zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut txt_buf: Option<Vec<u8>> = None;
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf).ok()?;
        if lower.ends_with(".txt") {
            txt_buf = Some(buf);
        } else if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
        {
            let basename = Path::new(&name)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(name);
            images.push((basename, buf));
        }
    }
    let txt = txt_buf?;
    let text = aozora::parser_txt::decode_bytes(&txt);
    Some(ZipContents {
        document: aozora::parse_txt(&text),
        images,
    })
}

fn parse_zip_txt(zip_path: &Path) -> Option<aozora::Document> {
    read_zip(zip_path).map(|c| c.document)
}

/// Minimal valid JPEG (1×1 white). Used as a placeholder cover until
/// Phase 4 (resvg-based cover renderer) lands.
fn tiny_placeholder_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06,
        0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B,
        0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20,
        0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31,
        0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF,
        0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00,
        0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05,
        0x04, 0x04, 0x00, 0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08,
        0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A,
        0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75,
        0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9,
        0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6,
        0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
        0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7,
        0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFB, 0xD0,
        0xFF, 0xD9,
    ]
}

fn dump(label: &str, doc: &aozora::Document) {
    println!("=== {} ===", label);
    println!("  title:   {}", doc.title);
    println!("  author:  {}", doc.author);
    println!("  body:    {} bytes", doc.body_xhtml.len());
    println!("  toc:     {} entries", doc.toc.len());
    for entry in doc.toc.iter().take(5) {
        println!("    h{} #{} {}", entry.level, entry.id, entry.text);
    }
    if doc.toc.len() > 5 {
        println!("    ... +{} more", doc.toc.len() - 5);
    }
    println!("  images:  {} refs", doc.referenced_images.len());
    println!("  colophon: {} chars", doc.colophon.len());
    // Sanity: body_xhtml should have *no* leftover ［＃...］ markers.
    let leftover = doc.body_xhtml.matches("［＃").count();
    println!("  leftover ［＃ markers in body: {}", leftover);
}

#[test]
fn parses_kokushikan_satsujin_jiken() {
    // 黒死館殺人事件 — the dense one (ruby, gaiji, kanji-composition,
    // explicit-marker ruby, postfix bold/sesame, 49 images).
    let path = Path::new("../books/aozora/1317_ruby_22263.zip");
    if !path.exists() {
        eprintln!("Skipping: fixture not found at {}", path.display());
        return;
    }
    let doc = parse_zip_txt(path).expect("parse 1317");
    dump("1317 黒死館殺人事件", &doc);
    assert_eq!(doc.title, "黒死館殺人事件");
    assert_eq!(doc.author, "小栗虫太郎");
    assert!(!doc.toc.is_empty(), "should have headings");
    assert!(!doc.referenced_images.is_empty(), "should reference images");
    let leftover = doc.body_xhtml.matches("［＃").count();
    assert_eq!(
        leftover, 0,
        "every ［＃...］ marker should be consumed; {} left",
        leftover
    );
}

#[test]
fn parses_kakinotane() {
    // 柿の種 — Terada Torahiko essays; suggested for boxed-region testing.
    let path = Path::new("../books/aozora/1684_ruby_11273.zip");
    if !path.exists() {
        eprintln!("Skipping: fixture not found at {}", path.display());
        return;
    }
    let doc = parse_zip_txt(path).expect("parse 1684");
    dump("1684 柿の種", &doc);
    assert_eq!(doc.title, "柿の種");
    assert_eq!(doc.author, "寺田寅彦");
    let leftover = doc.body_xhtml.matches("［＃").count();
    assert_eq!(leftover, 0, "{} ［＃ markers left in body", leftover);
}

#[test]
fn parses_wagahai() {
    // 吾輩は猫である — long, multi-chapter, expected yokogumi blocks.
    let path = Path::new("../books/aozora/789_ruby_5639.zip");
    if !path.exists() {
        eprintln!("Skipping: fixture not found at {}", path.display());
        return;
    }
    let doc = parse_zip_txt(path).expect("parse 789");
    dump("789 吾輩は猫である", &doc);
    assert_eq!(doc.title, "吾輩は猫である");
    assert_eq!(doc.author, "夏目漱石");
    let leftover = doc.body_xhtml.matches("［＃").count();
    assert_eq!(leftover, 0, "{} ［＃ markers left in body", leftover);
}

/// End-to-end: parse 黒死館 zip → build EPUB bytes → re-open via the
/// existing `EpubImporter` → assert metadata + spine + TOC reachable.
/// This is the integration test for Phase 1 + Phase 3.
#[test]
fn builds_loadable_epub_from_kokushikan() {
    let path = Path::new("../books/aozora/1317_ruby_22263.zip");
    if !path.exists() {
        eprintln!("Skipping: fixture not found at {}", path.display());
        return;
    }
    let contents = read_zip(path).expect("read fixture zip");
    // For end-to-end test we use a tiny placeholder cover JPEG (Phase 4
    // will replace this with the real resvg-rendered cover). Inlined
    // rather than checked in as a fixture file.
    let cover = tiny_placeholder_jpeg();
    let epub_bytes = boko::aozora::build_epub(boko::aozora::EpubInput {
        document: &contents.document,
        images: &contents.images,
        cover_jpeg: &cover,
    })
    .expect("build epub");

    // Persist for visual inspection on the user's filesystem.
    let out_path = path.with_extension("out.epub");
    std::fs::write(&out_path, &epub_bytes).expect("write out epub");
    eprintln!("Wrote {} ({} bytes)", out_path.display(), epub_bytes.len());

    // Re-open via the existing EpubImporter.
    let book = boko::Book::from_bytes(&epub_bytes, boko::Format::Epub).expect("open epub");
    assert_eq!(book.metadata().title, "黒死館殺人事件");
    assert_eq!(book.metadata().authors.first().map(String::as_str), Some("小栗虫太郎"));
    assert!(!book.spine().is_empty(), "spine must be non-empty");
    assert!(!book.toc().is_empty(), "toc must be non-empty");
}
