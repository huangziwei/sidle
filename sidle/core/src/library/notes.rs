//! Which note belongs to which highlight.

use super::db::AnnotationRow;

/// Start of a row's span, as the pair the format itself addresses by.
fn start(a: &AnnotationRow) -> Option<(i64, i64)> {
    Some((a.eid_start?, a.off_start.unwrap_or(0)))
}

/// End of a row's span. A row with no end anchor is a point at its start, which
/// is how a device writes a bookmark and how it writes a note attached to a
/// highlight.
fn end(a: &AnnotationRow) -> Option<(i64, i64)> {
    let s = start(a)?;
    Some((a.eid_end.unwrap_or(s.0), a.off_end.unwrap_or(s.1)))
}

/// Whether `note` sits inside `hl`.
fn contains(hl: &AnnotationRow, note: &AnnotationRow) -> bool {
    let (Some(hs), Some(he)) = (start(hl), end(hl)) else {
        return false;
    };
    let (Some(ns), Some(ne)) = (start(note), end(note)) else {
        return false;
    };
    hs <= ns && ne <= he
}

/// How wide a span is, for picking the tightest enclosing highlight. Only
/// meaningful for comparing spans that share an element; across elements the
/// element id dominates, which is the right order anyway.
fn width(a: &AnnotationRow) -> (i64, i64) {
    match (start(a), end(a)) {
        (Some(s), Some(e)) => (e.0 - s.0, e.1 - s.1),
        _ => (i64::MAX, i64::MAX),
    }
}

/// For each note in `rows`, the id of the highlight it annotates.
pub fn attachments(rows: &[AnnotationRow]) -> Vec<(i64, i64)> {
    let highlights: Vec<&AnnotationRow> = rows.iter().filter(|r| r.kind == "highlight").collect();
    rows.iter()
        .filter(|r| r.kind == "note")
        .filter_map(|note| {
            highlights
                .iter()
                .filter(|hl| contains(hl, note))
                .min_by_key(|hl| (width(hl), hl.id))
                .map(|hl| (note.id, hl.id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, kind: &str, es: i64, os: i64, ee: i64, oe: i64) -> AnnotationRow {
        AnnotationRow {
            id,
            dedup_hash: format!("h{id}"),
            book_id: Some(1),
            kind: kind.to_string(),
            eid_start: Some(es),
            off_start: Some(os),
            eid_end: Some(ee),
            off_end: Some(oe),
            loc_start: None,
            loc_end: None,
            linear_pos: None,
            text: String::new(),
            note_body: None,
            color: None,
            clip_title: None,
            clip_author: None,
            added_at: None,
            added_raw: None,
            imported_at: String::new(),
            source: "yjr".to_string(),
            hidden: false,
        }
    }

    /// The shape a Kindle writes when you add a note to an existing highlight:
    /// a zero-length point sitting on the highlight's end anchor.
    #[test]
    fn a_point_note_at_the_highlights_end_belongs_to_it() {
        let rows = vec![
            row(1, "highlight", 918, 311, 918, 327),
            row(2, "note", 918, 327, 918, 327),
        ];
        assert_eq!(attachments(&rows), vec![(2, 1)]);
    }

    /// The shape Sidle writes: a note spanning the whole highlight.
    #[test]
    fn a_note_spanning_the_highlight_belongs_to_it() {
        let rows = vec![
            row(1, "highlight", 918, 311, 918, 327),
            row(2, "note", 918, 311, 918, 327),
        ];
        assert_eq!(attachments(&rows), vec![(2, 1)]);
    }

    /// Both at once — the measured case: one highlight carrying two notes.
    #[test]
    fn one_highlight_carries_every_note_inside_it() {
        let rows = vec![
            row(1, "highlight", 918, 311, 918, 327),
            row(2, "note", 918, 311, 918, 327),
            row(3, "note", 918, 327, 918, 327),
        ];
        assert_eq!(attachments(&rows), vec![(2, 1), (3, 1)]);
    }

    #[test]
    fn a_note_outside_every_highlight_stands_alone() {
        let rows = vec![
            row(1, "highlight", 918, 311, 918, 327),
            row(2, "note", 918, 400, 918, 400),
            row(3, "note", 900, 0, 900, 5),
        ];
        assert!(attachments(&rows).is_empty());
    }

    /// Nested highlights: the note goes to the tightest one, so it reads against
    /// the passage actually marked rather than whatever encloses it.
    #[test]
    fn the_tightest_enclosing_highlight_wins() {
        let rows = vec![
            row(1, "highlight", 918, 0, 918, 500),
            row(2, "highlight", 918, 300, 918, 330),
            row(3, "note", 918, 310, 918, 320),
        ];
        assert_eq!(attachments(&rows), vec![(3, 2)]);
    }

    #[test]
    fn a_bookmark_neither_attaches_nor_adopts() {
        let rows = vec![
            row(1, "bookmark", 918, 311, 918, 311),
            row(2, "note", 918, 311, 918, 311),
        ];
        assert!(
            attachments(&rows).is_empty(),
            "a bookmark is not a highlight"
        );
    }

    /// An unanchored row can't be placed, so it can't be grouped either.
    #[test]
    fn an_unanchored_note_is_left_alone() {
        let mut note = row(2, "note", 918, 311, 918, 327);
        note.eid_start = None;
        let rows = vec![row(1, "highlight", 918, 0, 918, 500), note];
        assert!(attachments(&rows).is_empty());
    }
}
