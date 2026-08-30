//! A Kindle page, as the device laid it out.
//!
//! [`Capture::parse`] takes a header of `key:value` lines followed by one
//! brace-delimited block per [`Element`]. [`Fields`] holds every key of a
//! block, read by a method here or not.
//!
//! [`Score`] compares a page this crate laid out against one of those, box
//! for box.

use std::fmt;
use std::path::Path;
use std::{fs, io};

use crate::geom::Rect;

/// The `key:value` lines of one section, in the order the file gives them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fields(Vec<(String, String)>);

impl Fields {
    /// The first value for `key`, or `None` where the section has no such
    /// line.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every line, in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    fn number<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.get(key)?.trim().parse().ok()
    }
}

/// One element on the page: a run of text on one line, a picture, a rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Element {
    pub fields: Fields,
}

impl Element {
    /// What kind of element it is — `text`, `image`.
    pub fn kind(&self) -> &str {
        self.fields.get("typ").unwrap_or("")
    }

    /// The rectangle it was drawn in, in device dots from the top left of the
    /// panel.
    pub fn rect(&self) -> Option<Rect> {
        let mut parts = self.fields.get("box")?.split_whitespace();
        let mut next = || parts.next()?.parse::<f32>().ok();
        Some(Rect::new(next()?, next()?, next()?, next()?))
    }

    /// Each glyph's own rectangle, in the order they are drawn.
    ///
    /// A word broken across two lines carries the whole word's rectangles on
    /// both of its elements, spanning more lines than [`Element::rect`]. The
    /// glyph a break falls inside has no rectangle: the count is under the
    /// character count of [`Element::text`] by one for each break.
    pub fn glyph_rects(&self) -> Vec<Rect> {
        let Some(field) = self.fields.get("gbx") else {
            return Vec::new();
        };
        let numbers: Vec<f32> = field
            .split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect();
        numbers
            .chunks_exact(4)
            .map(|r| Rect::new(r[0], r[1], r[2], r[3]))
            .collect()
    }

    /// The span of reading positions the element covers, as the short
    /// numbering the page uses. Two elements of one word broken across lines
    /// carry the same span.
    pub fn span(&self) -> Option<(u32, u32)> {
        let mut parts = self.fields.get("sht")?.split_whitespace();
        let mut next = || parts.next()?.parse::<u32>().ok();
        Some((next()?, next()?))
    }

    /// The text drawn, where the element is text.
    pub fn text(&self) -> Option<&str> {
        self.fields.get("txt")
    }

    /// Which line of the page it sits on, counted from one.
    pub fn line(&self) -> Option<u32> {
        self.fields.number("ln#")
    }

    /// Whether the line it sits on runs down the page.
    pub fn is_vertical(&self) -> bool {
        self.fields.number::<u32>("wrm") == Some(1)
    }

    /// Its bidirectional embedding level, where the element is text. An even
    /// level runs left to right and an odd one right to left.
    pub fn bidi_level(&self) -> Option<u32> {
        self.fields.number("bdi")
    }

    /// The resource it draws, where the element is a picture.
    pub fn source(&self) -> Option<&str> {
        self.fields.get("src")
    }

    /// Its first reading position, as a base64 token. The `b64` field holds
    /// the first and the last.
    pub fn position(&self) -> Option<&str> {
        self.fields.get("b64")?.split_whitespace().next()
    }
}

/// One page of elements, and the header stated ahead of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capture {
    /// Everything stated ahead of the first element.
    pub header: Fields,
    pub elements: Vec<Element>,
}

impl Capture {
    /// Read a `_element.txt`.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self::parse(&fs::read_to_string(path)?))
    }

    /// Parse the text of one. A line that is neither a brace nor a
    /// `key:value` pair is skipped.
    pub fn parse(text: &str) -> Self {
        let mut capture = Capture::default();
        let mut open: Option<Element> = None;

        for line in text.lines() {
            let line = line.trim_end_matches(['\r', '\n']);
            match line.trim() {
                "{" => open = Some(Element::default()),
                "}" => {
                    if let Some(element) = open.take() {
                        capture.elements.push(element);
                    }
                }
                _ => {
                    let Some((key, value)) = line.split_once(':') else {
                        continue;
                    };
                    let pair = (key.trim().to_string(), value.to_string());
                    match &mut open {
                        Some(element) => element.fields.0.push(pair),
                        None if capture.elements.is_empty() => capture.header.0.push(pair),
                        // A line after the last block belongs to no element.
                        None => {}
                    }
                }
            }
        }
        // A capture cut off mid-element keeps what that element had.
        if let Some(element) = open {
            capture.elements.push(element);
        }
        capture
    }

    /// How many elements the header says the page holds, which is not
    /// necessarily how many it carries.
    pub fn declared_count(&self) -> Option<usize> {
        self.header.number("cnt")
    }

    /// Whether the page runs down the panel. Any element reporting a vertical
    /// writing mode settles it: a vertical book's cover is one horizontal
    /// picture and says nothing about the book.
    pub fn is_vertical(&self) -> bool {
        self.elements.iter().any(Element::is_vertical)
    }

    /// Every text element, in file order.
    pub fn glyphs(&self) -> impl Iterator<Item = &Element> {
        self.elements.iter().filter(|e| e.kind() == "text")
    }

    /// Every text element once, dropping the second half of a word broken
    /// across two lines — which the page reports twice, under one span.
    pub fn runs(&self) -> Vec<&Element> {
        let mut seen: Vec<(u32, u32)> = Vec::new();
        let mut runs = Vec::new();
        for element in self.glyphs() {
            match element.span() {
                Some(span) if seen.contains(&span) => {}
                Some(span) => {
                    seen.push(span);
                    runs.push(element);
                }
                None => runs.push(element),
            }
        }
        runs
    }

    /// Every glyph rectangle on the page, in drawing order.
    pub fn glyph_rects(&self) -> Vec<Rect> {
        self.runs()
            .iter()
            .flat_map(|element| element.glyph_rects())
            .collect()
    }

    /// Each run's own rectangle, in drawing order. A word broken across two
    /// lines contributes the rectangle of its first line only.
    pub fn run_rects(&self) -> Vec<Rect> {
        self.runs().iter().filter_map(|e| e.rect()).collect()
    }

    /// Every text element's `txt`, joined in file order.
    pub fn text(&self) -> String {
        self.glyphs().filter_map(Element::text).collect()
    }

    /// The elements of one line, in file order.
    pub fn line(&self, number: u32) -> impl Iterator<Item = &Element> {
        self.elements
            .iter()
            .filter(move |e| e.line() == Some(number))
    }

    /// Every line number the page uses, in ascending order.
    pub fn lines(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = self.elements.iter().filter_map(Element::line).collect();
        numbers.sort_unstable();
        numbers.dedup();
        numbers
    }
}

/// How far a laid-out box may sit from a captured one, in dots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// The most a box's top-left corner may move.
    pub origin: f32,
    /// The most its width and height may differ.
    pub extent: f32,
}

impl Default for Budget {
    /// One dot each way, which is the resolution the page is reported at.
    fn default() -> Self {
        Self {
            origin: 1.0,
            extent: 1.0,
        }
    }
}

/// One box outside the [`Budget`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Divergence {
    /// Where in the drawing order it is.
    pub index: usize,
    pub device: Rect,
    pub laid: Rect,
    /// How far its top-left corner moved.
    pub origin: f32,
    /// How much its width and height differ.
    pub extent: f32,
}

/// What a page cost against a [`Capture`] of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Score {
    /// How many boxes were compared.
    pub compared: usize,
    /// How many boxes each side drew.
    pub device_count: usize,
    pub laid_count: usize,
    pub mean_origin: f32,
    pub max_origin: f32,
    pub mean_extent: f32,
    pub max_extent: f32,
    /// The first box outside the budget, which is the one to look at: every
    /// box after a wrong advance is displaced by it.
    pub first_over: Option<Divergence>,
}

impl Score {
    /// Compare two pages box for box, in drawing order.
    pub fn of(device: &[Rect], laid: &[Rect], budget: Budget) -> Self {
        let mut score = Score {
            device_count: device.len(),
            laid_count: laid.len(),
            ..Score::default()
        };
        let (mut origin_total, mut extent_total) = (0.0, 0.0);

        for (index, (a, b)) in device.iter().zip(laid).enumerate() {
            let origin = (a.x - b.x).abs().max((a.y - b.y).abs());
            let extent = (a.width - b.width).abs().max((a.height - b.height).abs());
            score.compared += 1;
            origin_total += origin;
            extent_total += extent;
            score.max_origin = score.max_origin.max(origin);
            score.max_extent = score.max_extent.max(extent);
            if score.first_over.is_none() && (origin > budget.origin || extent > budget.extent) {
                score.first_over = Some(Divergence {
                    index,
                    device: *a,
                    laid: *b,
                    origin,
                    extent,
                });
            }
        }
        if score.compared > 0 {
            score.mean_origin = origin_total / score.compared as f32;
            score.mean_extent = extent_total / score.compared as f32;
        }
        score
    }

    /// Whether every box compared sat inside the budget and both pages drew
    /// the same number of them.
    pub fn within(&self, budget: Budget) -> bool {
        self.first_over.is_none()
            && self.device_count == self.laid_count
            && self.max_origin <= budget.origin
            && self.max_extent <= budget.extent
    }
}

impl fmt::Display for Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of {}/{} boxes: origin mean {:.2} max {:.2}, extent mean {:.2} max {:.2}",
            self.compared,
            self.device_count,
            self.laid_count,
            self.mean_origin,
            self.max_origin,
            self.mean_extent,
            self.max_extent
        )?;
        if let Some(over) = &self.first_over {
            write!(
                f,
                "; first over at {}: device {:?} laid {:?}",
                over.index, over.device, over.laid
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for Capture {
    /// The capture in its own form, key order and all.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (key, value) in self.header.iter() {
            writeln!(f, "{key}:{value}")?;
        }
        for element in &self.elements {
            writeln!(f, "{{")?;
            for (key, value) in element.fields.iter() {
                writeln!(f, "{key}:{value}")?;
            }
            writeln!(f, "}}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COVER: &str = "\
ver:1
fmt:base64
pre:0
nxt:1
hod:1
fod:1
psp:1
pep:1
cnt:1
{
typ:image
b64:AQAAAAAAAAAA AQAAAAAAAAAA
sht:1 1
src:42
box:100 0 800 2000
ln#:1
wrm:0
}
";

    const TEXT: &str = "\
ver:1
fmt:base64
pre:0
nxt:0
hod:0
fod:0
psp:1
pep:14
cnt:4
pgp:1
{
typ:text
b64:AQAAAAAAAAAA AQAAAAADAAAA
sht:1 3
box:100 100 60 80
ln#:1
wrm:0
txt:aaa
gbx:100 90 20 100 120 90 20 100 140 90 20 100
bdi:0
}
{
typ:text
b64:AQAAAAAEAAAA AQAAAAAGAAAA
sht:5 7
box:200 100 60 80
ln#:1
wrm:0
txt:bbb
gbx:200 90 20 100 220 90 20 100 240 90 20 100
bdi:0
}
{
typ:text
b64:AQAAAAAIAAAA AQAAAAANAAAA
sht:9 14
box:300 100 60 80
ln#:1
wrm:0
txt:ccc
gbx:300 90 20 100 320 90 20 100 340 90 20 100 100 290 20 100 120 290 20 100
bdi:0
}
{
typ:text
b64:AQAAAAAIAAAA AQAAAAANAAAA
sht:9 14
box:100 300 40 80
ln#:2
wrm:0
txt:ddd
gbx:300 90 20 100 320 90 20 100 340 90 20 100 100 290 20 100 120 290 20 100
bdi:0
}
";

    #[test]
    fn a_cover_page_is_one_picture() {
        let capture = Capture::parse(COVER);

        assert_eq!(capture.declared_count(), Some(1));
        assert_eq!(capture.elements.len(), 1);
        assert_eq!(capture.elements[0].kind(), "image");
        assert_eq!(capture.elements[0].source(), Some("42"));
        assert_eq!(
            capture.elements[0].rect(),
            Some(Rect::new(100.0, 0.0, 800.0, 2000.0))
        );
        assert!(!capture.is_vertical());
    }

    #[test]
    fn a_position_token_is_the_first_of_the_pair() {
        let capture = Capture::parse(COVER);

        assert_eq!(capture.elements[0].position(), Some("AQAAAAAAAAAA"));
    }

    #[test]
    fn a_text_element_is_a_run_on_one_line() {
        let capture = Capture::parse(TEXT);

        assert_eq!(capture.glyphs().count(), 4);
        assert_eq!(capture.text(), "aaabbbcccddd");
        assert_eq!(capture.lines(), vec![1, 2]);
        assert_eq!(capture.line(1).count(), 3);
        assert_eq!(capture.elements[1].bidi_level(), Some(0));
        assert_eq!(capture.elements[1].span(), Some((5, 7)));
    }

    #[test]
    fn a_word_broken_across_lines_is_reported_once_per_line_and_counted_once() {
        let capture = Capture::parse(TEXT);

        // Both halves carry the same span and the same glyph rectangles.
        let halves: Vec<&str> = capture
            .glyphs()
            .filter(|e| e.span() == Some((9, 14)))
            .filter_map(Element::text)
            .collect();
        assert_eq!(halves, ["ccc", "ddd"]);
        assert_eq!(capture.runs().len(), 3);

        // Its rectangles reach both lines, and the glyph the break falls on
        // has none: fewer than the word has characters.
        let word = capture.glyphs().last().expect("the split run");
        let rects = word.glyph_rects();
        assert_eq!(rects.len(), 5);
        assert_eq!(rects[0].y, 90.0);
        assert_eq!(rects[4].y, 290.0);
    }

    #[test]
    fn the_lines_of_a_page_are_one_line_height_apart() {
        let capture = Capture::parse(TEXT);
        let line = |n| capture.line(n).next().and_then(Element::rect).unwrap().y;

        assert_eq!(line(2) - line(1), 200.0);
    }

    #[test]
    fn a_page_laid_out_the_same_way_scores_nothing() {
        let capture = Capture::parse(TEXT);
        let device = capture.run_rects();

        let score = Score::of(&device, &device, Budget::default());

        assert_eq!(score.compared, 3);
        assert_eq!(score.max_origin, 0.0);
        assert!(score.within(Budget::default()));
        assert!(score.first_over.is_none());
    }

    #[test]
    fn the_score_names_the_first_box_outside_the_budget() {
        let device = Capture::parse(TEXT).run_rects();
        let mut laid = device.clone();
        laid[1].x += 4.0;
        laid[2].x += 9.0;

        let score = Score::of(&device, &laid, Budget::default());

        assert_eq!(score.max_origin, 9.0);
        assert!(!score.within(Budget::default()));
        let over = score.first_over.expect("one box is over");
        assert_eq!(over.index, 1);
        assert_eq!(over.origin, 4.0);
    }

    #[test]
    fn a_page_with_fewer_boxes_than_the_device_drew_is_never_within_budget() {
        let device = Capture::parse(TEXT).run_rects();

        let score = Score::of(&device, &device[..2], Budget::default());

        assert_eq!(score.compared, 2);
        assert_eq!(score.max_origin, 0.0);
        assert!(!score.within(Budget::default()));
    }

    #[test]
    fn every_key_survives_a_round_trip() {
        // Every key survives, read by a method here or not.
        assert_eq!(Capture::parse(COVER).to_string(), COVER);
        assert_eq!(Capture::parse(TEXT).to_string(), TEXT);
    }

    #[test]
    fn a_capture_cut_off_mid_element_keeps_what_it_had() {
        let truncated = "ver:1\ncnt:9\n{\ntyp:text\nbox:1 2 3 4\n";
        let capture = Capture::parse(truncated);

        assert_eq!(capture.declared_count(), Some(9));
        assert_eq!(capture.elements.len(), 1);
        assert_eq!(
            capture.elements[0].rect(),
            Some(Rect::new(1.0, 2.0, 3.0, 4.0))
        );
    }

    #[test]
    fn a_glyph_of_a_brace_is_text_and_not_a_delimiter() {
        let capture = Capture::parse("cnt:1\n{\ntyp:text\ntxt:}\nbox:0 0 1 1\n}\n");

        assert_eq!(capture.elements.len(), 1);
        assert_eq!(capture.elements[0].text(), Some("}"));
    }
}
