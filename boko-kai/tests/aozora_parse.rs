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
    assert_eq!(
        doc.toc.len(),
        29,
        "黒死館殺人事件 should produce 29 TOC entries (regression guard)"
    );
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
    assert_eq!(
        doc.toc.len(),
        45,
        "柿の種 should produce 45 TOC entries (44 plain + 1 ruby-bearing)"
    );
    let ruby_h = doc
        .toc
        .iter()
        .find(|t| t.text == "最上川象潟以後")
        .expect("ruby postfix heading present");
    assert_eq!(ruby_h.level, 3);
    assert!(
        doc.body_xhtml
            .contains("<ruby>象潟<rp>（</rp><rt>きさかた</rt>"),
        "ruby-bearing heading should render with <ruby> markup, not stripped plain text"
    );
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
    assert_eq!(
        doc.toc.len(),
        11,
        "吾輩は猫である should produce 11 chapter TOC entries (一〜十一)"
    );
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
    let cover = boko::aozora::render_cover_jpeg(
        &contents.document.title,
        &contents.document.author,
    )
    .expect("render cover");
    // Persist the cover so we can eyeball it (`open <path>`).
    let cover_path = path.with_extension("cover.jpg");
    std::fs::write(&cover_path, &cover).expect("write cover");
    eprintln!("Wrote {} ({} bytes)", cover_path.display(), cover.len());

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
