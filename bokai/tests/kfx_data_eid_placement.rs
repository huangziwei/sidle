//! Where `data-eid` lands, not just which ids appear.
//!
//! A renderer resolves a stored `(element, offset)` handle by querying
//! `[data-eid="N"]` and walking text from the element it finds. Agreeing on the
//! *set* of ids is not enough — the two routes have to stamp them on the same
//! element, enclosing the same text, or a highlight lands on the wrong words.
//!
//! `kfx_source_elements.rs` pins the id lists; this pins their placement.

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

fn assert_placement_matches_port(path: &str) {
    let Ok(kfx) = std::fs::read(path) else {
        return; // fixture not present in this checkout
    };

    let mut book = Book::from_bytes(&kfx, Format::Kfx).expect("import the fixture");
    let content = bokai::export::normalize_book_with(&mut book, SourceElements::Mark)
        .expect("normalize with source elements");
    let mine: Vec<_> = content
        .chapters
        .iter()
        .map(|c| stamps(&c.document))
        .collect();

    let (port, _store) =
        bokai::kfx_to_epub::kfx_to_reader_book_lazy(&kfx).expect("port reader book");
    let theirs: Vec<_> = port.sections.iter().map(|s| stamps(&s.html)).collect();

    assert_eq!(
        mine.len(),
        theirs.len(),
        "{path}: document count {} vs port {}",
        mine.len(),
        theirs.len()
    );
    assert!(
        mine.iter().any(|m| !m.is_empty()),
        "{path}: nothing was stamped — the comparison would pass vacuously"
    );

    for (i, (m, t)) in mine.iter().zip(&theirs).enumerate() {
        for (eid, (port_tag, port_text)) in t {
            let (my_tag, my_text) = m
                .get(eid)
                .unwrap_or_else(|| panic!("{path}: section {i} eid {eid} not stamped"));
            assert_eq!(
                my_tag, port_tag,
                "{path}: section {i} eid {eid} landed on <{my_tag}>, port used <{port_tag}>"
            );
            assert_eq!(
                my_text, port_text,
                "{path}: section {i} eid {eid} encloses different text"
            );
        }
        let extra: Vec<_> = m.keys().filter(|e| !t.contains_key(e)).collect();
        assert!(
            extra.is_empty(),
            "{path}: section {i} stamped ids the port does not: {extra:?}"
        );
    }
}

#[test]
fn reflowable_kfx_stamps_land_where_the_mechanical_route_stamps() {
    assert_placement_matches_port(REFLOWABLE);
}

#[test]
fn short_kfx_stamps_land_where_the_mechanical_route_stamps() {
    assert_placement_matches_port(SHORT);
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
