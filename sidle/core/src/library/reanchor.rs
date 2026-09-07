//! Move a book's annotations onto a rebuilt copy of that book.

use rusqlite::{Connection, params};

use super::anchor::BookIndex;

/// What one [`book`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Reanchored {
    /// Annotations whose handles land on their own text.
    pub intact: usize,
    /// Annotations moved onto the rebuilt book.
    pub moved: usize,
    /// Annotations whose text was not found, or found in several places. Left
    /// exactly as they were.
    pub stranded: usize,
    /// Of the intact ones, those whose stored positions were put back on the
    /// book's current scale.
    pub refreshed: usize,
}

/// One annotation's stored anchor, as far as re-anchoring cares.
struct Stored {
    id: i64,
    /// `bookmark`, `highlight` or `note`. A bookmark's `text` is the whole
    /// element it sits in, from that element's first character.
    kind: String,
    eid_start: Option<i64>,
    off_start: Option<i64>,
    eid_end: Option<i64>,
    off_end: Option<i64>,
    loc_start: Option<i64>,
    loc_end: Option<i64>,
    text: String,
}

/// Re-anchor every annotation on `book_id` against `index`.
pub fn book(conn: &Connection, book_id: i64, index: &BookIndex) -> rusqlite::Result<Reanchored> {
    // An empty index strands every annotation, and reports nothing.
    if index.is_empty() {
        return Ok(Reanchored::default());
    }
    let mut stmt = conn.prepare(
        // Text-less rows are selected too: a bookmark's handle can move with a rebuild.
        "SELECT id, kind, eid_start, off_start, eid_end, off_end, loc_start, loc_end, text
           FROM annotations WHERE book_id = ?1",
    )?;
    let rows: Vec<Stored> = stmt
        .query_map(params![book_id], |r| {
            Ok(Stored {
                id: r.get(0)?,
                kind: r.get(1)?,
                eid_start: r.get(2)?,
                off_start: r.get(3)?,
                eid_end: r.get(4)?,
                off_end: r.get(5)?,
                loc_start: r.get(6)?,
                loc_end: r.get(7)?,
                text: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut out = Reanchored::default();
    for row in rows {
        if still_lands_on_its_text(&row, index) {
            out.refreshed += usize::from(refresh_positions(conn, &row, index)?);
            out.intact += 1;
            continue;
        }
        let Some(span) = find_span(index, &row.text) else {
            out.stranded += 1;
            continue;
        };
        conn.execute(
            "UPDATE annotations
                SET eid_start = ?2, off_start = ?3, eid_end = ?4, off_end = ?5,
                    loc_start = ?6, loc_end = ?7, linear_pos = ?8
              WHERE id = ?1",
            params![
                row.id,
                span.start.0,
                span.start.1,
                span.end.0,
                span.end.1,
                index.position(span.start.0, span.start.1),
                index.position(span.end.0, span.end.1),
                index.position(span.start.0, span.start.1),
            ],
        )?;
        out.moved += 1;
    }
    Ok(out)
}

/// Put `loc_start`, `loc_end` and `linear_pos` back on `index`'s scale,
/// reporting whether that changed anything. A rebuild renumbers the position
/// map under handles that did not move.
fn refresh_positions(conn: &Connection, row: &Stored, index: &BookIndex) -> rusqlite::Result<bool> {
    let at = |eid: Option<i64>, off: Option<i64>| {
        eid.and_then(|eid| index.position(eid, off.unwrap_or(0)))
    };
    // An element the map does not place leaves the stored positions alone: a
    // stale coordinate is worth more than a null one.
    let Some(loc_start) = at(row.eid_start, row.off_start) else {
        return Ok(false);
    };
    let loc_end = at(row.eid_end, row.off_end).or(row.loc_end);
    if (Some(loc_start), loc_end) == (row.loc_start, row.loc_end) {
        return Ok(false);
    }
    conn.execute(
        "UPDATE annotations SET loc_start = ?2, loc_end = ?3, linear_pos = ?2 WHERE id = ?1",
        params![row.id, loc_start, loc_end],
    )?;
    Ok(true)
}

/// How much of an annotation's text has to line up before a place is a
/// candidate: long enough for ordinary prose to be unique, short enough for a
/// fixed window.
const HEAD_CHARS: usize = 48;

/// How much of an annotation's head is compared against the element its handle
/// names.
const HEAD_MATCH: usize = 16;

/// The handles an annotation's text sits at: `(eid, offset)` for each end, the
/// end inclusive as the device writes it.
struct Span {
    start: (i64, i64),
    end: (i64, i64),
}

/// One significant character and where it lives.
type Sig = (char, i64, i64);

/// Every non-whitespace character of the book, lowercased, in reading order,
/// each paired with the element and offset it came from.
fn significant(index: &BookIndex) -> impl Iterator<Item = Sig> + '_ {
    index.reading_order().iter().flat_map(move |&eid| {
        index
            .text_of(eid)
            .unwrap_or("")
            .chars()
            .enumerate()
            .filter(|(_, c)| !c.is_whitespace())
            .map(move |(off, c)| (c.to_ascii_lowercase(), eid, off as i64))
    })
}

/// Where `text` lives in the book, or `None` when it is absent or in several
/// places.
fn find_span(index: &BookIndex, text: &str) -> Option<Span> {
    let needle: Vec<char> = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if needle.is_empty() {
        return None;
    }
    let head = &needle[..needle.len().min(HEAD_CHARS)];

    let mut window: std::collections::VecDeque<Sig> = Default::default();
    let mut found: Option<Sig> = None;
    for sig in significant(index) {
        window.push_back(sig);
        if window.len() > head.len() {
            window.pop_front();
        }
        if window.len() == head.len() && window.iter().map(|(c, ..)| *c).eq(head.iter().copied()) {
            if found.is_some() {
                return None;
            }
            found = window.front().copied();
        }
    }
    let (_, start_eid, start_off) = found?;

    // Walk the stream again for the annotation's last character: `window` only
    // ever held the head.
    let mut seen = 0usize;
    let mut end = None;
    for (_, eid, off) in
        significant(index).skip_while(|&(_, e, o)| (e, o) != (start_eid, start_off))
    {
        seen += 1;
        end = Some((eid, off));
        if seen == needle.len() {
            break;
        }
    }
    Some(Span {
        start: (start_eid, start_off),
        end: end?,
    })
}

/// Whether the stored handle points at the stored text.
fn still_lands_on_its_text(row: &Stored, index: &BookIndex) -> bool {
    let (Some(eid), Some(offset)) = (row.eid_start, row.off_start) else {
        return false;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return false;
    };
    let Some(element) = index.text_of(eid) else {
        return false;
    };
    // A bookmark's text is the whole element, taken from its first character
    // whatever `off_start` names inside it.
    if row.kind == "bookmark" {
        return element == row.text;
    }
    // Only the head has to match, and only as far as this element runs: an
    // annotation can open near the end of one and carry the rest into the next.
    // Both offsets count characters.
    let head: Vec<char> = row.text.chars().take(HEAD_MATCH).collect();
    if head.is_empty() {
        return true;
    }
    let tail: Vec<char> = element.chars().skip(offset).take(HEAD_MATCH).collect();
    !tail.is_empty() && tail.iter().zip(&head).all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A book of `parts`, each element placed on the linear axis at the
    /// running character total ahead of it.
    fn index(parts: &[(i64, &str)]) -> BookIndex {
        let text: HashMap<i64, String> =
            parts.iter().map(|(e, t)| (*e, (*t).to_string())).collect();
        let mut pid = HashMap::new();
        let mut at = 0i64;
        for (eid, t) in parts {
            pid.insert(*eid, at);
            at += t.chars().count() as i64;
        }
        BookIndex::from_parts(text, pid)
    }

    #[test]
    fn a_span_is_found_across_an_element_boundary() {
        let idx = index(&[
            (10, "the surface appearance "),
            (11, "of reality breaks down"),
        ]);
        let span = find_span(&idx, "appearance of reality").expect("found");
        assert_eq!(span.start, (10, 12));
        // The inclusive last character, in the element the text runs into.
        assert_eq!(span.end, (11, 9));
    }

    #[test]
    fn spacing_the_converter_changed_does_not_strand_an_annotation() {
        // Stored when the build fused the words; the rebuild separates them.
        let idx = index(&[(10, "The Man In the High Castle made")]);
        let span = find_span(&idx, "In theHigh Castle").expect("found");
        assert_eq!(span.start, (10, 8));
        assert_eq!(span.end, (10, 25));
    }

    #[test]
    fn text_in_two_places_is_left_alone() {
        // Moving a highlight to the wrong occurrence reads as correct forever
        // after; leaving it stale is visible.
        let idx = index(&[(10, "he said. "), (11, "he said. ")]);
        assert!(find_span(&idx, "he said").is_none());
    }

    #[test]
    fn an_untouched_book_moves_nothing() {
        let idx = index(&[(10, "the surface appearance of reality")]);
        let row = Stored {
            id: 1,
            kind: "highlight".into(),
            eid_start: Some(10),
            off_start: Some(4),
            eid_end: Some(10),
            off_end: Some(21),
            loc_start: Some(4),
            loc_end: Some(21),
            text: "surface appearance".into(),
        };
        assert!(still_lands_on_its_text(&row, &idx));
    }

    /// A rebuild renumbers the position map under handles that did not move. A
    /// row counted `intact` owes its cached positions.
    #[test]
    fn an_intact_handle_gets_its_positions_put_back_on_the_scale() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE annotations (
                 id INTEGER PRIMARY KEY, book_id INTEGER, kind TEXT NOT NULL,
                 eid_start INTEGER, off_start INTEGER, eid_end INTEGER,
                 off_end INTEGER, loc_start INTEGER, loc_end INTEGER,
                 linear_pos INTEGER, text TEXT NOT NULL DEFAULT '');
             INSERT INTO annotations VALUES
                 (1, 7, 'highlight', 11, 4, 11, 21, 4, 21, 4, 'surface appearance');",
        )
        .expect("schema");

        // Element 11 sits 100 along the axis, where the stored 4/21 put it at 0.
        let idx = index(&[(10, "x".repeat(100).leak()), (11, "the surface appearance")]);
        let done = book(&conn, 7, &idx).expect("re-anchor");
        assert_eq!(
            done,
            Reanchored {
                intact: 1,
                refreshed: 1,
                ..Default::default()
            }
        );

        let got: (i64, i64, i64) = conn
            .query_row(
                "SELECT loc_start, loc_end, linear_pos FROM annotations WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("row");
        assert_eq!(got, (104, 121, 104));
    }

    /// An annotation opening near the end of an element carries the rest of
    /// its head in the next one, and the handle lands.
    #[test]
    fn a_head_running_into_the_next_element_still_lands() {
        let idx = index(&[(10, "the surface"), (11, " appearance of reality")]);
        let row = Stored {
            id: 1,
            kind: "highlight".into(),
            eid_start: Some(10),
            off_start: Some(4),
            eid_end: Some(11),
            off_end: Some(11),
            loc_start: Some(4),
            loc_end: Some(22),
            text: "surface appearance".into(),
        };
        assert!(still_lands_on_its_text(&row, &idx));
    }

    /// A handle whose element holds something else does not land, however
    /// little of it is left to compare.
    #[test]
    fn a_handle_on_the_wrong_text_does_not_land() {
        let idx = index(&[(10, "the surface"), (11, " appearance of reality")]);
        let row = Stored {
            id: 1,
            kind: "highlight".into(),
            eid_start: Some(10),
            off_start: Some(4),
            eid_end: Some(10),
            off_end: Some(10),
            loc_start: Some(4),
            loc_end: Some(10),
            text: "appearance".into(),
        };
        assert!(!still_lands_on_its_text(&row, &idx));
    }

    /// `off_start` counts characters, not bytes: a multibyte element's handle
    /// lands on the text the offset names.
    #[test]
    fn an_offset_past_multibyte_text_still_lands() {
        let idx = index(&[(10, "　その頃の、家族たちと一緒にうつした写真")]);
        let row = Stored {
            id: 1,
            kind: "highlight".into(),
            eid_start: Some(10),
            off_start: Some(6),
            eid_end: Some(10),
            off_end: Some(12),
            loc_start: Some(6),
            loc_end: Some(12),
            text: "家族たちと一緒に".into(),
        };
        assert!(still_lands_on_its_text(&row, &idx));
    }

    /// An element the position map does not place keeps the row's stored
    /// positions.
    #[test]
    fn an_unplaced_element_keeps_the_stored_positions() {
        let text = HashMap::from([(11, "the surface appearance".to_string())]);
        let idx = BookIndex::from_parts(text, HashMap::new());
        let row = Stored {
            id: 1,
            kind: "highlight".into(),
            eid_start: Some(11),
            off_start: Some(4),
            eid_end: Some(11),
            off_end: Some(21),
            loc_start: Some(4),
            loc_end: Some(21),
            text: "surface appearance".into(),
        };
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE annotations (id INTEGER PRIMARY KEY);")
            .expect("schema");
        // The absent columns are never reached: no UPDATE runs.
        refresh_positions(&conn, &row, &idx).expect("left alone");
    }
}
