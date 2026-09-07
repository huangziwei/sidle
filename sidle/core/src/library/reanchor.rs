//! Move a book's annotations onto a rebuilt copy of that book.

use rusqlite::{Connection, params};

use super::anchor::BookIndex;

/// What one [`book`] pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Reanchored {
    /// Annotations whose handles already landed on their own text.
    pub intact: usize,
    /// Annotations moved onto the rebuilt book.
    pub moved: usize,
    /// Annotations whose text was not found, or found in several places. Left
    /// exactly as they were.
    pub stranded: usize,
}

/// One annotation's stored anchor, as far as re-anchoring cares.
struct Stored {
    id: i64,
    eid_start: Option<i64>,
    off_start: Option<i64>,
    eid_end: Option<i64>,
    off_end: Option<i64>,
    loc_start: Option<i64>,
    loc_end: Option<i64>,
    text: String,
}

/// Re-anchor every annotation on `book_id` against `index`, the book as it now
/// is.
pub fn book(conn: &Connection, book_id: i64, index: &BookIndex) -> rusqlite::Result<Reanchored> {
    // A book with no text index can only strand every annotation it is asked
    // about, and would report that as a finding. It is a container we cannot
    // read, not a book whose highlights moved.
    if index.is_empty() {
        return Ok(Reanchored::default());
    }
    let mut stmt = conn.prepare(
        // Text-less rows are selected too, so they are counted rather than passed over: a
        // bookmark with no text to search for sits on a handle the rebuild may have moved.
        "SELECT id, eid_start, off_start, eid_end, off_end, loc_start, loc_end, text
           FROM annotations WHERE book_id = ?1",
    )?;
    let rows: Vec<Stored> = stmt
        .query_map(params![book_id], |r| {
            Ok(Stored {
                id: r.get(0)?,
                eid_start: r.get(1)?,
                off_start: r.get(2)?,
                eid_end: r.get(3)?,
                off_end: r.get(4)?,
                loc_start: r.get(5)?,
                loc_end: r.get(6)?,
                text: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut out = Reanchored::default();
    for row in rows {
        if still_lands_on_its_text(&row, index) {
            refresh_positions(conn, &row, index)?;
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

/// Put `loc_start`, `loc_end` and `linear_pos` back on `index`'s scale for a
/// row whose handles it still lands on. A rebuild renumbers the position map
/// under handles that did not move.
fn refresh_positions(conn: &Connection, row: &Stored, index: &BookIndex) -> rusqlite::Result<()> {
    let at = |eid: Option<i64>, off: Option<i64>| {
        eid.and_then(|eid| index.position(eid, off.unwrap_or(0)))
    };
    let loc_start = at(row.eid_start, row.off_start);
    let loc_end = at(row.eid_end, row.off_end);
    if (loc_start, loc_end) == (row.loc_start, row.loc_end) {
        return Ok(());
    }
    conn.execute(
        "UPDATE annotations SET loc_start = ?2, loc_end = ?3, linear_pos = ?2 WHERE id = ?1",
        params![row.id, loc_start, loc_end],
    )?;
    Ok(())
}

/// How much of an annotation's text has to line up before a place is a
/// candidate. Long enough that ordinary prose is unique at this length, short
/// enough that the scan carries a fixed, tiny window instead of the book.
const HEAD_CHARS: usize = 48;

/// The handles an annotation's text now sits at: `(eid, offset)` for each end,
/// the end being inclusive as the device writes it.
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

/// Where `text` now lives in the book, or `None` when it is not there or is
/// there more than once.
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

    // Walk the same stream again from the start to find where the annotation's
    // last character now sits. A second pass rather than a remembered position,
    // because the window only ever held the head.
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

/// Whether the stored handle still points at the stored text.
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
    // Only the head has to match. An annotation can run past its first element,
    // and following it across the rest is what `search` already does — this is
    // the cheap test that decides whether to pay for it.
    let head: String = row.text.chars().take(16).collect();
    element
        .get(offset..)
        .is_some_and(|tail| tail.starts_with(&head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A two-element book. `pid_of` gives each element's start on the linear
    /// axis, as a KFX position map would.
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

    /// A rebuild renumbers the position map under handles that did not move, so
    /// a row counted `intact` still owes its cached positions.
    #[test]
    fn an_intact_handle_gets_its_positions_put_back_on_the_scale() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE annotations (
                 id INTEGER PRIMARY KEY, book_id INTEGER, eid_start INTEGER,
                 off_start INTEGER, eid_end INTEGER, off_end INTEGER,
                 loc_start INTEGER, loc_end INTEGER, linear_pos INTEGER,
                 text TEXT NOT NULL DEFAULT '');
             INSERT INTO annotations VALUES
                 (1, 7, 11, 4, 11, 21, 4, 21, 4, 'surface appearance');",
        )
        .expect("schema");

        // Element 11 sits 100 along the axis, where the stored 4/21 put it at 0.
        let idx = index(&[(10, "x".repeat(100).leak()), (11, "the surface appearance")]);
        let done = book(&conn, 7, &idx).expect("re-anchor");
        assert_eq!(
            done,
            Reanchored {
                intact: 1,
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
}
