//! Visual-check helper: parse a fixture and dump samples of the produced
//! XHTML around interesting features (ruby, sesame, postfix bold, image
//! refs). Not a test — diagnostic only.
//!
//! Usage: `cargo run --example aozora_dump_sample`

use std::io::Read;
use std::path::Path;

use boko::aozora;

fn main() {
    let path = Path::new("books/aozora/1317_ruby_22263.zip");
    let cwd = std::env::current_dir().unwrap();
    let resolved = if path.exists() {
        path.to_path_buf()
    } else {
        cwd.parent().unwrap().join(path)
    };
    let file = std::fs::File::open(&resolved).expect("open zip");
    let mut archive = zip::ZipArchive::new(file).expect("read zip");
    let mut buf = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        if entry.is_file() && entry.name().ends_with(".txt") {
            entry.read_to_end(&mut buf).unwrap();
            break;
        }
    }
    let text = aozora::parser_txt::decode_bytes(&buf);
    let doc = aozora::parse_txt(&text);
    for needle in ["<ruby", "sesame", "<strong", "<img", "underline", "yokogumi"] {
        match doc.body_xhtml.find(needle) {
            Some(idx) => {
                let start = floor_char_boundary(&doc.body_xhtml, idx.saturating_sub(80));
                let end = ceil_char_boundary(&doc.body_xhtml, (idx + 400).min(doc.body_xhtml.len()));
                println!("--- {needle} sample ---\n{}\n", &doc.body_xhtml[start..end]);
            }
            None => println!("--- {needle}: not found ---"),
        }
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
