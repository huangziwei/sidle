//! What a MOBI/KF8 file's headers say about the book.
//!
//! Both importers — MOBI 6's single text stream and KF8's skeleton/chunk
//! layout — read the same PDB name, MOBI header and EXTH records. The
//! KF8-only records (fixed layout, book type, page resolution) are absent
//! from a MOBI 6 file.

use crate::formats::mobi::{ExthHeader, MobiHeader, PdbInfo};
use crate::model::{CollectionInfo, Metadata, OrientationLock};

/// Read the book's metadata out of the headers.
///
/// The title comes from the best of three: the EXTH record the store updated,
/// the MOBI header's own, and the PDB database name — which is truncated to 31
/// bytes and is the last resort for that reason.
pub fn from_headers(pdb: &PdbInfo, mobi: &MobiHeader, exth: &Option<ExthHeader>) -> Metadata {
    let title = exth
        .as_ref()
        .and_then(|e| e.title.clone())
        .or_else(|| {
            if !mobi.title.is_empty() {
                Some(mobi.title.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| pdb.name.clone());

    let mut metadata = Metadata {
        title,
        ..Default::default()
    };
    if let Some(exth) = exth {
        apply_exth(&mut metadata, exth);
    }
    metadata.periodical = periodical_kind(mobi, exth);
    if metadata.periodical.is_some() {
        metadata.title = issue_title(&metadata.title, metadata.date.as_deref());
    }
    metadata
}

/// Name the *issue*, not just the publication.
///
/// A periodical's title field holds the publication, the same string on every
/// issue; the catalogue groups issues under it and separates them by date. A
/// sideload gets no such grouping (see `Metadata::periodical`).
///
/// The date goes on in ISO form, keeping a plain title sort chronological.
/// Idempotent: a title ending with the date is returned unchanged.
fn issue_title(title: &str, date: Option<&str>) -> String {
    let Some(date) = date
        .map(crate::util::truncate_to_date)
        .filter(|d| !d.is_empty())
    else {
        return title.to_string();
    };
    if title.trim_end().ends_with(&date) {
        return title.to_string();
    }
    format!("{}{}{}", title.trim_end(), ISSUE_DATE_SEPARATOR, date)
}

/// What [`issue_title`] joins the publication and the date with.
const ISSUE_DATE_SEPARATOR: &str = " — ";

/// [`issue_title`] undone: the publication name inside an issue title.
///
/// A masthead shows the date in its own right and wants the publication.
pub(crate) fn publication_title<'a>(title: &'a str, date: Option<&str>) -> &'a str {
    date.map(crate::util::truncate_to_date)
        .filter(|d| !d.is_empty())
        .and_then(|d| title.strip_suffix(&format!("{ISSUE_DATE_SEPARATOR}{d}")))
        .unwrap_or(title)
}

/// Is this an issue of a periodical, and of what kind?
///
/// Two independent declarations say so, and either alone is enough: the MOBI
/// header type and EXTH 501. A file rebuilt by a third-party tool can carry
/// one without the other.
///
/// The NCX's own tag-5 `kind` string lives in the index, not the headers. An
/// importer that has read the index can override this.
fn periodical_kind(
    mobi: &MobiHeader,
    exth: &Option<ExthHeader>,
) -> Option<crate::model::PeriodicalKind> {
    use crate::model::PeriodicalKind;
    PeriodicalKind::from_mobi_type(mobi.mobi_type).or_else(|| {
        exth.as_ref()
            .and_then(|e| e.cde_type.as_deref())
            .and_then(PeriodicalKind::from_cde_type)
    })
}

/// Everything the EXTH records say beyond the title.
fn apply_exth(metadata: &mut Metadata, exth: &ExthHeader) {
    metadata.authors = exth.authors.clone();
    metadata.title_sort = exth.title_pronunciation.clone();
    metadata.author_sorts = exth.author_pronunciations.clone();
    metadata.publisher = exth.publisher.clone();
    metadata.description = exth.description.clone();
    metadata.subjects = exth.subjects.clone();
    metadata.date = exth.pub_date.clone();
    metadata.rights = exth.rights.clone();
    metadata.language = exth.language.clone().unwrap_or_default();
    metadata.identifier = exth
        .isbn
        .clone()
        .or_else(|| exth.asin.clone())
        .or_else(|| exth.source.clone())
        .unwrap_or_default();
    // EXTH 113 nominally holds an ASIN, but calibre's exporter writes a
    // freshly-minted UUID there. Only promote to `metadata.asin` when the value
    // actually looks like an Amazon ASIN (10-char alphanumeric starting with B
    // for ebooks).
    metadata.asin = exth.asin.as_ref().filter(|s| looks_like_asin(s)).cloned();
    // The series a store title states inline (EXTH 503) — the format has no
    // field of its own for it. No position comes with it: the annotation names
    // the series and nothing else.
    metadata.collection = exth.series.as_ref().map(|name| CollectionInfo {
        name: name.clone(),
        collection_type: Some("series".to_string()),
        position: None,
    });
    // Writing-mode signals (EXTH 525 / 527). Both calibre-exported and native
    // Amazon files carry these; no fallback to inline HTML class needed.
    // Calibre's `reader/headers.py:96-108` is the spec.
    metadata.primary_writing_mode = exth.primary_writing_mode.clone();
    metadata.page_progression_direction = exth
        .page_progression_direction
        .clone()
        // Calibre derives PPD from writing-mode when EXTH 527 is absent:
        // anything ending `-rl` is RTL pagination.
        .or_else(|| {
            exth.primary_writing_mode.as_deref().and_then(|pwm| {
                if pwm.ends_with("-rl") {
                    Some("rtl".to_string())
                } else if pwm.ends_with("-lr") {
                    Some("ltr".to_string())
                } else {
                    None
                }
            })
        });

    // Any of the three FXL EXTH records marks the book pre-paginated.
    let book_type = exth.book_type.clone().filter(|s| !s.is_empty());
    let is_comic = book_type.as_deref() == Some("comic");
    metadata.fixed_layout = exth.fixed_layout.as_deref() == Some("true")
        || book_type.is_some()
        || exth.original_resolution.is_some();
    metadata.book_type = book_type;
    metadata.default_viewport = exth
        .original_resolution
        .as_deref()
        .and_then(parse_resolution);
    metadata.orientation_lock = exth
        .orientation_lock
        .as_deref()
        .and_then(OrientationLock::parse);
    // `book-type: comic` implies facing-page (landscape) spreads.
    if metadata.fixed_layout && is_comic {
        metadata.rendition_spread = Some("landscape".to_string());
    }
}

/// Amazon ASIN format: exactly 10 ASCII alphanumeric characters, typically
/// starting with `B` for ebook listings. Used to disambiguate EXTH 113 from
/// the UUID calibre's exporter occasionally writes into the same slot.
fn looks_like_asin(s: &str) -> bool {
    s.len() == 10 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Parse a KF8 `original-resolution` value (`"1444x2048"`) into `(w, h)`.
fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::mobi::NULL_INDEX;
    use crate::model::PeriodicalKind;

    /// What the EXTH records make of a book, starting from nothing.
    fn read(exth: ExthHeader) -> Metadata {
        let mut metadata = Metadata::default();
        apply_exth(&mut metadata, &exth);
        metadata
    }

    #[test]
    fn a_series_stated_in_the_title_becomes_the_books_collection() {
        let meta = read(ExthHeader {
            title: Some("灯台守の日々 3".to_string()),
            series: Some("灯台守の日々".to_string()),
            ..Default::default()
        });
        let collection = meta.collection.expect("the series the title stated");
        assert_eq!(collection.name, "灯台守の日々");
        assert_eq!(collection.collection_type.as_deref(), Some("series"));
        // The annotation carries no number, and guessing one would be worse
        // than leaving it to whoever knows.
        assert_eq!(collection.position, None);
    }

    #[test]
    fn a_book_that_states_no_series_belongs_to_none() {
        let meta = read(ExthHeader {
            title: Some("架空太郎全集".to_string()),
            ..Default::default()
        });
        assert!(meta.collection.is_none());
    }

    #[test]
    fn a_fixed_layout_record_survives_the_reading_both_importers_share() {
        // Both importers share one reading of the KF8-only records, so neither
        // route can drop what the other keeps.
        let meta = read(ExthHeader {
            book_type: Some("comic".to_string()),
            original_resolution: Some("1444x2048".to_string()),
            ..Default::default()
        });
        assert!(meta.fixed_layout);
        assert_eq!(meta.book_type.as_deref(), Some("comic"));
        assert_eq!(meta.default_viewport, Some((1444, 2048)));
        assert_eq!(meta.rendition_spread.as_deref(), Some("landscape"));
    }

    /// EXTH 124 becomes `orientation_lock`.
    #[test]
    fn an_orientation_lock_survives_the_headers() {
        let meta = read(ExthHeader {
            book_type: Some("comic".to_string()),
            original_resolution: Some("1280x907".to_string()),
            orientation_lock: Some("landscape".to_string()),
            ..Default::default()
        });
        assert_eq!(meta.orientation_lock, Some(OrientationLock::Landscape));

        let unlocked = read(ExthHeader {
            orientation_lock: Some("none".to_string()),
            ..Default::default()
        });
        assert_eq!(unlocked.orientation_lock, Some(OrientationLock::Auto));

        assert_eq!(read(ExthHeader::default()).orientation_lock, None);
    }

    #[test]
    fn a_page_resolution_reads_as_a_viewport() {
        assert_eq!(parse_resolution("1444x2048"), Some((1444, 2048)));
        assert_eq!(parse_resolution(" 800 X 600 "), Some((800, 600)));
        assert_eq!(parse_resolution("wide"), None);
    }

    /// What a periodical looks like from the headers alone.
    fn kind(mobi_type: u32, cde_type: Option<&str>) -> Option<PeriodicalKind> {
        let mobi = MobiHeader {
            mobi_type,
            ..header()
        };
        let exth = cde_type.map(|t| ExthHeader {
            cde_type: Some(t.to_string()),
            ..Default::default()
        });
        periodical_kind(&mobi, &exth)
    }

    #[test]
    fn either_declaration_alone_marks_a_periodical() {
        // The real `.pobi` shape: both signals agree.
        assert_eq!(kind(259, Some("MAGZ")), Some(PeriodicalKind::Magazine));
        // Either alone is enough — a rebuild can keep one and drop the other.
        assert_eq!(kind(259, None), Some(PeriodicalKind::Magazine));
        assert_eq!(kind(2, Some("MAGZ")), Some(PeriodicalKind::Magazine));
        // The other two flavours, by header type and by cdetype.
        assert_eq!(kind(257, None), Some(PeriodicalKind::Newspaper));
        assert_eq!(kind(258, None), Some(PeriodicalKind::Blog));
        assert_eq!(kind(2, Some("nwpr")), Some(PeriodicalKind::Newspaper));
        assert_eq!(kind(2, Some("FEED")), Some(PeriodicalKind::Blog));
    }

    #[test]
    fn an_issue_is_titled_by_its_date() {
        // Every issue's title field says only the publication, so the date is
        // the only thing that tells two tiles apart.
        assert_eq!(
            issue_title("The New Yorker", Some("2014-12-14 23:00:00+00:00")),
            "The New Yorker — 2014-12-14"
        );
        // Idempotent — a converted file re-imported does not grow a second date.
        assert_eq!(
            issue_title("The New Yorker — 2014-12-14", Some("2014-12-14")),
            "The New Yorker — 2014-12-14"
        );
        // Nothing to add without a date.
        assert_eq!(issue_title("The New Yorker", None), "The New Yorker");
        assert_eq!(issue_title("The New Yorker", Some("")), "The New Yorker");
    }

    #[test]
    fn the_publication_can_be_recovered_from_the_issue_title() {
        let dated = issue_title("The New Yorker", Some("2014-12-14 23:00:00+00:00"));
        assert_eq!(
            publication_title(&dated, Some("2014-12-14 23:00:00+00:00")),
            "The New Yorker"
        );
        // A title that never carried a date, or a date that is not its suffix,
        // is returned whole.
        assert_eq!(
            publication_title("The New Yorker", Some("2014-12-14")),
            "The New Yorker"
        );
        assert_eq!(
            publication_title("2014-12-14 in Review", Some("2014-12-14")),
            "2014-12-14 in Review"
        );
        assert_eq!(publication_title(&dated, None), &dated);
    }

    #[test]
    fn an_ordinary_book_is_not_a_periodical() {
        // Type 2 is `Book`; EBOK and PDOC are the book cdetypes.
        assert_eq!(kind(2, None), None);
        assert_eq!(kind(2, Some("EBOK")), None);
        assert_eq!(kind(2, Some("PDOC")), None);
        assert_eq!(kind(2, Some("")), None);
    }

    /// A `MobiHeader` with nothing set — only `mobi_type` matters here.
    fn header() -> MobiHeader {
        MobiHeader {
            compression: crate::formats::mobi::Compression::None,
            text_record_count: 0,
            text_record_size: 0,
            encryption: 0,
            mobi_type: 2,
            encoding: crate::formats::mobi::Encoding::Utf8,
            mobi_version: 6,
            first_image_index: NULL_INDEX,
            title: String::new(),
            language: 0,
            exth_flags: 0,
            extra_data_flags: 0,
            huff_record_index: NULL_INDEX,
            huff_record_count: 0,
            skel_index: NULL_INDEX,
            div_index: NULL_INDEX,
            oth_index: NULL_INDEX,
            fdst_index: NULL_INDEX,
            fdst_count: 0,
            ncx_index: NULL_INDEX,
            header_length: 232,
        }
    }
}
