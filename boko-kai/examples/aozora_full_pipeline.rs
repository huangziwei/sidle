//! Run the complete Aozora pipeline on every `.zip` under
//! `books/aozora/`: decode → parse → render cover → build EPUB → write
//! both the cover JPEG and the EPUB next to the source zip.
//!
//! Usage from the repo root:
//!
//! ```
//! cargo run --example aozora_full_pipeline
//! ```
//!
//! Outputs land at `books/aozora/{stem}.out.epub` and
//! `books/aozora/{stem}.cover.jpg`. The `books/` dir is gitignored.

use std::io::Read;
use std::path::{Path, PathBuf};

use boko::aozora;

fn main() {
    let dir = locate_books_aozora();
    let zips: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read books/aozora dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "zip")
                && !p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with(".out") || s.ends_with(".cover"))
        })
        .collect();

    println!("Found {} zip(s) under {}", zips.len(), dir.display());
    for z in zips {
        process(&z);
    }
}

fn locate_books_aozora() -> PathBuf {
    // Try cwd-relative first (works when run from boko-kai/), then parent.
    let cwd = std::env::current_dir().expect("cwd");
    for candidate in [cwd.join("books/aozora"), cwd.join("../books/aozora")] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("books/aozora directory not found");
}

fn process(zip_path: &Path) {
    println!("\n--- {} ---", zip_path.file_name().unwrap().to_string_lossy());

    let file = match std::fs::File::open(zip_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  open: {e}");
            return;
        }
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("  zip: {e}");
            return;
        }
    };

    let mut txt_bytes: Option<Vec<u8>> = None;
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).expect("zip entry");
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf).expect("read entry");
        if lower.ends_with(".txt") {
            txt_bytes = Some(buf);
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

    let Some(bytes) = txt_bytes else {
        eprintln!("  no .txt entry");
        return;
    };
    let text = aozora::parser_txt::decode_bytes(&bytes);
    let doc = aozora::parse_txt(&text);
    println!(
        "  parsed: title={:?} author={:?} body={}B toc={} images={}",
        doc.title,
        doc.author,
        doc.body_xhtml.len(),
        doc.toc.len(),
        doc.referenced_images.len(),
    );

    let cover = aozora::render_cover_jpeg(&doc.title, &doc.author).expect("render cover");
    let cover_path = zip_path.with_extension("cover.jpg");
    std::fs::write(&cover_path, &cover).expect("write cover");
    println!("  cover: {} ({} bytes, 1050×1500)", cover_path.display(), cover.len());

    let epub_bytes = aozora::build_epub(aozora::EpubInput {
        document: &doc,
        images: &images,
        cover_jpeg: &cover,
    })
    .expect("build epub");
    let epub_path = zip_path.with_extension("out.epub");
    std::fs::write(&epub_path, &epub_bytes).expect("write epub");
    println!("  epub:  {} ({} bytes)", epub_path.display(), epub_bytes.len());
}
