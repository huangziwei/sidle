//! Where `data-eid` lands, not just which ids appear.

mod common;

use std::collections::BTreeMap;

use bokai::Book;
use bokai::export::SourceElements;
use bokai::model::Format;

const REFLOWABLE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
const SHORT: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

/// The tag each stamped id landed on, and the text that element encloses.
fn stamps(html: &str) -> BTreeMap<i64, (String, String)> {
    let mut out = BTreeMap::new();
    let mut rest = html;
    while let Some(at) = rest.find("data-eid=\"") {
        let after = &rest[at + "data-eid=\"".len()..];
        let Some(close) = after.find('"') else { break };
        let Ok(eid) = after[..close].parse::<i64>() else {
            rest = after;
            continue;
        };
        let before = &rest[..at];
        let tag = before
            .rfind('<')
            .map(|lt| {
                before[lt + 1..]
                    .split([' ', '\t', '\n', '>'])
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default();
        let tail = &after[close + 1..];
        let text = tail
            .find('>')
            .map(|gt| {
                let body = &tail[gt + 1..];
                let end = body.find(&format!("</{tag}>")).unwrap_or(body.len());
                strip_markup(&body[..end])
            })
            .unwrap_or_default();
        out.insert(eid, (tag, text));
        rest = after;
    }
    out
}

/// Drop tags and collapse whitespace — the comparison is about the words a
/// reader would highlight, not the markup around them.
fn strip_markup(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where every stamp landed: section, id, the tag it marked, and the text that
fn placements(kfx: &[u8]) -> (usize, usize, u64) {
    let mut book = Book::from_bytes(kfx, Format::Kfx).expect("import the fixture");
    let content = bokai::export::normalize_book_with(&mut book, SourceElements::Mark)
        .expect("normalize with source elements");
    let all: Vec<_> = content
        .chapters
        .iter()
        .map(|c| stamps(&c.document))
        .collect();
    assert!(
        all.iter().any(|m| !m.is_empty()),
        "nothing was stamped — the digest would pin nothing"
    );
    let total: usize = all.iter().map(|m| m.len()).sum();
    let lines = all.iter().enumerate().flat_map(|(i, m)| {
        m.iter()
            .map(move |(eid, (tag, text))| format!("{i}\t{eid}\t{tag}\t{text}"))
    });
    (all.len(), total, common::digest_lines(lines))
}

#[test]
fn reflowable_kfx_stamp_placement_is_pinned() {
    let Ok(kfx) = std::fs::read(REFLOWABLE) else {
        return; // fixture not present in this checkout
    };
    let (sections, stamped, digest) = placements(&kfx);
    assert_eq!((sections, stamped), (22, 1918), "stamp shape");
    assert_eq!(digest, 0x9106_9e05_f7da_8f7c, "stamp placement moved");
}

#[test]
fn short_kfx_stamp_placement_is_pinned() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let (sections, stamped, digest) = placements(&kfx);
    assert_eq!((sections, stamped), (9, 881), "stamp shape");
    assert_eq!(digest, 0xbb35_524d_3ca9_9676, "stamp placement moved");
}

#[test]
fn a_shipped_container_carries_no_source_element_ids() {
    let Ok(kfx) = std::fs::read(SHORT) else {
        return;
    };
    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let content = bokai::export::normalize_book(&mut book).expect("normalize for export");
    let stamped: usize = content
        .chapters
        .iter()
        .map(|c| c.document.matches("data-eid").count())
        .sum();
    assert_eq!(
        stamped, 0,
        "an exported EPUB must not leak the source format's element ids"
    );
}
