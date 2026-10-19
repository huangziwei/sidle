//! Move a book's annotations and reading positions onto a rebuilt copy of that book.

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

/// One `reading_position` row, as far as re-anchoring cares.
struct SavedPosition {
    source: String,
    device_serial: String,
    eid: i64,
    offset: i64,
    linear_pos: Option<i64>,
}

/// Move `book_id`'s saved reading positions from `before`, the build they were
/// taken on, onto its rebuild `after` by the text each opens on. A position whose
/// text `after` holds nowhere or more than once stays as it was.
pub fn reading_positions(
    conn: &Connection,
    book_id: i64,
    before: &BookIndex,
    after: &BookIndex,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        r#"SELECT source, device_serial, eid, "offset", linear_pos FROM reading_position
            WHERE book_id = ?1 AND eid IS NOT NULL"#,
    )?;
    let rows: Vec<SavedPosition> = stmt
        .query_map(params![book_id], |r| {
            Ok(SavedPosition {
                source: r.get(0)?,
                device_serial: r.get(1)?,
                eid: r.get(2)?,
                offset: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                linear_pos: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut moved = 0;
    for row in rows {
        let Some(span) = find_span(after, &text_from(before, row.eid, row.offset)) else {
            continue;
        };
        let (eid, found) = span.start;
        let lead = leading_whitespace(before, row.eid, row.offset);
        let offset = if whitespace_before(after, eid, found, lead) {
            found - lead
        } else {
            found
        };
        let linear_pos = rescaled(
            row.linear_pos,
            before,
            (row.eid, row.offset),
            after,
            (eid, offset),
        );
        if (eid, offset, linear_pos) == (row.eid, row.offset, row.linear_pos) {
            continue;
        }
        conn.execute(
            r#"UPDATE reading_position SET eid = ?4, "offset" = ?5, linear_pos = ?6
                WHERE book_id = ?1 AND source = ?2 AND device_serial = ?3"#,
            params![
                book_id,
                row.source,
                row.device_serial,
                eid,
                offset,
                linear_pos
            ],
        )?;
        moved += 1;
    }
    Ok(moved)
}

/// `stored` for a handle moved from `from` to `to`, on the scale it was written
/// on: a Location or a raw position. A value on neither, a page index, is kept.
fn rescaled(
    stored: Option<i64>,
    before: &BookIndex,
    from: (i64, i64),
    after: &BookIndex,
    to: (i64, i64),
) -> Option<i64> {
    let stored = stored?;
    let moved = if before.location(from.0, from.1) == Some(stored) {
        after.location(to.0, to.1)
    } else if before.position(from.0, from.1) == Some(stored) {
        after.position(to.0, to.1)
    } else {
        None
    };
    Some(moved.unwrap_or(stored))
}

/// How many whitespace characters open element `eid` from `offset`, ahead of its
/// next significant one; 0 when none follows.
fn leading_whitespace(index: &BookIndex, eid: i64, offset: i64) -> i64 {
    let skip = usize::try_from(offset).unwrap_or(0);
    index
        .text_of(eid)
        .unwrap_or("")
        .chars()
        .skip(skip)
        .position(|c| !c.is_whitespace())
        .map_or(0, |lead| lead as i64)
}

/// Whether the `count` characters ahead of `(eid, offset)` are all whitespace.
fn whitespace_before(index: &BookIndex, eid: i64, offset: i64, count: i64) -> bool {
    let (Ok(end), Ok(count)) = (usize::try_from(offset), usize::try_from(count)) else {
        return false;
    };
    count > 0
        && end >= count
        && index
            .text_of(eid)
            .unwrap_or("")
            .chars()
            .skip(end - count)
            .take(count)
            .all(char::is_whitespace)
}

/// The first [`HEAD_CHARS`] significant characters of `index` from `(eid, offset)` on.
fn text_from(index: &BookIndex, eid: i64, offset: i64) -> String {
    let order = index.reading_order();
    let Some(at) = order.iter().position(|&e| e == eid) else {
        return String::new();
    };
    let skip = usize::try_from(offset).unwrap_or(0);
    order[at..]
        .iter()
        .enumerate()
        .flat_map(|(i, &e)| {
            let text = index.text_of(e).unwrap_or("");
            text.chars().skip(if i == 0 { skip } else { 0 })
        })
        .filter(|c| !c.is_whitespace())
        .take(HEAD_CHARS)
        .collect()
}

/// Put `loc_start`, `loc_end` and `linear_pos` back on `index`'s scale,
/// reporting whether that changed anything.
fn refresh_positions(conn: &Connection, row: &Stored, index: &BookIndex) -> rusqlite::Result<bool> {
    let at = |eid: Option<i64>, off: Option<i64>| {
        eid.and_then(|eid| index.position(eid, off.unwrap_or(0)))
    };
    // An element the map does not place leaves the stored positions alone.
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

/// Significant characters of an annotation's text that have to line up for a
/// place to be a candidate.
const HEAD_CHARS: usize = 48;

/// How much of an annotation's head is compared against the element its handle
/// names.
const HEAD_MATCH: usize = 16;

/// The handles an annotation's text sits at: `(eid, offset)` for each end, the
/// end inclusive.
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

    // A second walk finds the annotation's last character; `window` holds only
    // the head.
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
    fn a_reading_position_follows_its_text_into_the_rebuild() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            r#"CREATE TABLE reading_position (
                   book_id INTEGER NOT NULL, eid INTEGER, "offset" INTEGER,
                   linear_pos INTEGER, source TEXT NOT NULL,
                   device_serial TEXT NOT NULL DEFAULT '', updated_at TEXT NOT NULL,
                   PRIMARY KEY (book_id, source, device_serial));
               INSERT INTO reading_position VALUES
                   (7, 11, 0, 23, 'sidle', '', '2026-01-01T00:00:00Z'),
                   (7, 11, 0, 5, 'device', 'G001', '2026-01-01T00:00:00Z'),
                   (7, 10, 0, 0, 'device', 'G000', '2026-01-01T00:00:00Z');"#,
        )
        .expect("schema");
        // Element 11 opens on an ideographic space, which matching skips; the
        // moved handle keeps pointing at it.
        let before = index(&[
            (10, "chapter one ends here. "),
            (
                11,
                "\u{3000}Afterword. I first heard the story in a lecture hall",
            ),
        ]);
        let after = index(&[
            (10, "chapter one ends here. "),
            (12, "a page the rebuild added"),
            (
                13,
                "\u{3000}Afterword. I first heard the story in a lecture hall",
            ),
        ]);
        assert_eq!(
            reading_positions(&conn, 7, &before, &after).expect("move"),
            2
        );

        let got: Vec<(String, i64, i64, i64, String)> = conn
            .prepare(
                r#"SELECT device_serial, eid, "offset", linear_pos, updated_at
                     FROM reading_position ORDER BY device_serial"#,
            )
            .expect("select")
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("read");
        let stamp = "2026-01-01T00:00:00Z".to_string();
        // `linear_pos` keeps its scale: 23 was element 11's position, 5 was on
        // neither. G000's head runs from element 10 into 11, which `after` splits.
        assert_eq!(
            got,
            [
                (
                    String::new(),
                    13,
                    0,
                    after.position(13, 0).unwrap(),
                    stamp.clone()
                ),
                ("G000".to_string(), 10, 0, 0, stamp.clone()),
                ("G001".to_string(), 13, 0, 5, stamp),
            ]
        );
    }

    #[test]
    fn text_in_two_places_is_left_alone() {
        // Text found twice moves nowhere.
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

    /// An `intact` row gets its cached positions put back on the rebuilt scale.
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
