//! `My Clippings.txt` parser.
//!
//! Kindle appends every highlight / note / bookmark to a flat text log on the
//! device. Sidle uses it for two narrow roles (the `.yjr` + book is the
//! complete, precise source otherwise):
//!   1. **orphan archive** — entries whose book is no longer on the device;
//!      they carry only a coarse `Location`, so they land in the unlinked inbox;
//!   2. **validation oracle** — independent ground truth for the P0 gate.
//!
//! Record format (English UI — this is single-user software, so no locale
//! handling):
//! ```text
//! <title> (<author>)
//! - Your <Kind> on [page <P> | ]Location <L>[-<L2>] | Added on <when>
//!
//! <body…>
//! ==========
//! ```
//! Bookmarks have an empty body; highlights/notes carry their text. A leading
//! U+FEFF BOM prefixes each title line.

use std::path::Path;

use super::yjr::Kind;

/// Byte-order mark Kindle prefixes to title lines (not stripped by `trim`,
/// since U+FEFF isn't Unicode `White_Space`).
const BOM: char = '\u{feff}';
/// Record separator: a line of exactly ten `=`.
const SEPARATOR: &str = "==========";

/// One `My Clippings.txt` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipping {
    /// Book title with the trailing `(author)` group removed.
    pub title: String,
    /// Author, from the final parenthesised group of the title line.
    pub author: Option<String>,
    pub kind: Kind,
    /// `page N`, when present (some books expose only `Location`).
    pub page: Option<i64>,
    /// `Location` start; for a range (`L-L2`) this is `L`.
    pub loc_start: Option<i64>,
    /// `Location` range end (`L2`), when the entry spans a range.
    pub loc_end: Option<i64>,
    /// The raw `Added on …` text; ISO normalisation is deferred (matches
    /// `.yjr`, whose timestamps aren't decoded yet either).
    pub added_raw: Option<String>,
    /// Highlight / note text. Empty for a bookmark.
    pub text: String,
}

/// Parse a whole `My Clippings.txt` body into its records. Malformed records
/// (missing a metadata line) are skipped rather than aborting the parse.
pub fn parse(content: &str) -> Vec<Clipping> {
    content
        .split(SEPARATOR)
        .filter_map(parse_record)
        .collect()
}

/// Read and parse a `My Clippings.txt` file. UTF-8 is read lossily so a stray
/// byte can't sink the whole import.
pub fn parse_file(path: &Path) -> std::io::Result<Vec<Clipping>> {
    let bytes = std::fs::read(path)?;
    Ok(parse(&String::from_utf8_lossy(&bytes)))
}

fn parse_record(raw: &str) -> Option<Clipping> {
    // Lines, with CR and the BOM stripped; the record's own surrounding blank
    // lines are dropped below by indexing past them.
    let lines: Vec<&str> = raw
        .lines()
        .map(|l| l.trim_end_matches('\r').trim_start_matches(BOM))
        .collect();

    // First non-blank line is the title; the line after it is the metadata.
    let title_idx = lines.iter().position(|l| !l.trim().is_empty())?;
    let title_line = lines[title_idx].trim();
    let meta_line = lines.get(title_idx + 1)?.trim();
    let (kind, page, loc_start, loc_end, added_raw) = parse_meta(meta_line)?;

    let (title, author) = split_title_author(title_line);

    // Body = everything after the metadata line, with the structural blank line
    // dropped and trailing whitespace trimmed. Internal blank lines (multi-line
    // notes) are preserved.
    let body = lines
        .get(title_idx + 2..)
        .map(|rest| rest.join("\n").trim().to_string())
        .unwrap_or_default();

    Some(Clipping {
        title,
        author,
        kind,
        page,
        loc_start,
        loc_end,
        added_raw,
        text: body,
    })
}

/// Split `"Title (sub) (Author)"` into `("Title (sub)", Some("Author"))` — the
/// author is the final parenthesised group. Titles legitimately contain earlier
/// parens, so we key on the *last* `(`.
fn split_title_author(line: &str) -> (String, Option<String>) {
    let line = line.trim();
    if line.ends_with(')')
        && let Some(open) = line.rfind('(')
    {
        let author = line[open + 1..line.len() - 1].trim();
        let title = line[..open].trim();
        if !author.is_empty() && !title.is_empty() {
            return (title.to_string(), Some(author.to_string()));
        }
    }
    (line.to_string(), None)
}

type Meta = (Kind, Option<i64>, Option<i64>, Option<i64>, Option<String>);

/// Parse `"- Your <Kind> on page N | Location L-L2 | Added on <when>"`. Tolerant
/// of the `page` segment being absent and of segment order.
fn parse_meta(meta: &str) -> Option<Meta> {
    let body = meta.trim().strip_prefix("- ")?.strip_prefix("Your ")?;
    // `<Kind> on <rest…>`
    let (kind_word, rest) = body.split_once(" on ")?;
    let kind = Kind::parse(&kind_word.trim().to_ascii_lowercase());

    let (mut page, mut loc_start, mut loc_end, mut added_raw) = (None, None, None, None);
    for seg in rest.split('|') {
        let seg = seg.trim();
        if let Some(p) = seg.strip_prefix("page ") {
            page = p.trim().parse().ok();
        } else if let Some(loc) = seg.strip_prefix("Location ") {
            let (a, b) = parse_location(loc.trim());
            loc_start = a;
            loc_end = b;
        } else if let Some(when) = seg.strip_prefix("Added on ") {
            added_raw = Some(when.trim().to_string());
        }
    }
    Some((kind, page, loc_start, loc_end, added_raw))
}

/// `"90-91"` → `(Some(90), Some(91))`; `"90"` → `(Some(90), None)`.
fn parse_location(s: &str) -> (Option<i64>, Option<i64>) {
    match s.split_once('-') {
        Some((a, b)) => (a.trim().parse().ok(), b.trim().parse().ok()),
        None => (s.trim().parse().ok(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A self-contained fixture covering every shape seen in the real corpus:
    // bookmark (empty body, page+Location), highlight with a range and no page,
    // note (single Location, page), and a title whose own text has parens.
    const FIXTURE: &str = "\u{feff}滅亡国家のやり直し (デジタル版) (ひろしたよだか)\n\
        - Your Bookmark on page 12 | Location 75 | Added on Monday, May 4, 2026 7:52:54 PM\n\
        \n\
        \n\
        ==========\n\
        \u{feff}ゲーマーズ！ (葵 せきな)\n\
        - Your Highlight on Location 36-37 | Added on Tuesday, May 5, 2026 12:25:50 AM\n\
        \n\
        平凡な日常を愛する平凡な主人公\n\
        ==========\n\
        \u{feff}滅亡国家のやり直し (デジタル版) (ひろしたよだか)\n\
        - Your Note on page 13 | Location 90 | Added on Monday, May 4, 2026 7:56:26 PM\n\
        \n\
        Digit normalization required\n\
        ==========\n";

    #[test]
    fn parses_all_record_shapes() {
        let c = parse(FIXTURE);
        assert_eq!(c.len(), 3);

        // Bookmark: page + single Location, empty body, parens-in-title.
        assert_eq!(c[0].kind, Kind::Bookmark);
        assert_eq!(c[0].title, "滅亡国家のやり直し (デジタル版)");
        assert_eq!(c[0].author.as_deref(), Some("ひろしたよだか"));
        assert_eq!(c[0].page, Some(12));
        assert_eq!(c[0].loc_start, Some(75));
        assert_eq!(c[0].loc_end, None);
        assert_eq!(c[0].text, "");
        assert_eq!(
            c[0].added_raw.as_deref(),
            Some("Monday, May 4, 2026 7:52:54 PM")
        );

        // Highlight: no page, Location range, body present.
        assert_eq!(c[1].kind, Kind::Highlight);
        assert_eq!(c[1].author.as_deref(), Some("葵 せきな"));
        assert_eq!(c[1].page, None);
        assert_eq!(c[1].loc_start, Some(36));
        assert_eq!(c[1].loc_end, Some(37));
        assert_eq!(c[1].text, "平凡な日常を愛する平凡な主人公");

        // Note: page + single Location, body is the note text.
        assert_eq!(c[2].kind, Kind::Note);
        assert_eq!(c[2].loc_start, Some(90));
        assert_eq!(c[2].text, "Digit normalization required");
    }

    #[test]
    fn title_without_author_parens_keeps_whole_title() {
        let (title, author) = split_title_author("A Plain Title");
        assert_eq!(title, "A Plain Title");
        assert_eq!(author, None);
    }

    #[test]
    fn empty_input_yields_no_records() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }
}
