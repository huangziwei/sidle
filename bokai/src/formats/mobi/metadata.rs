//! What a MOBI/KF8 file's headers say about the book.

use crate::formats::mobi::{ExthHeader, MobiHeader, PdbInfo};
use crate::model::{CollectionInfo, Contributor, Metadata, OrientationLock};

/// Read the book's metadata out of the headers.
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
pub(crate) fn publication_title<'a>(title: &'a str, date: Option<&str>) -> &'a str {
    date.map(crate::util::truncate_to_date)
        .filter(|d| !d.is_empty())
        .and_then(|d| title.strip_suffix(&format!("{ISSUE_DATE_SEPARATOR}{d}")))
        .unwrap_or(title)
}

/// Is this an issue of a periodical, and of what kind?
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
    // `author_pronunciations` keys `authors` by position.
    metadata.creators = exth
        .authors
        .iter()
        .enumerate()
        .map(|(i, name)| Contributor {
            file_as: exth.author_pronunciations.get(i).cloned(),
            ..Contributor::named(name.clone())
        })
        .collect();
    metadata.title_sort = exth.title_pronunciation.clone();
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
    // `looks_like_asin` keeps the UUIDs EXTH 113 also carries out of `metadata.asin`.
    metadata.asin = exth.asin.as_ref().filter(|s| looks_like_asin(s)).cloned();
    // The series a store title states inline (EXTH 503), with no position.
    metadata.collection = exth.series.as_ref().map(|name| CollectionInfo {
        name: name.clone(),
        collection_type: Some("series".to_string()),
        position: None,
    });
    // Writing-mode signals: EXTH 525 and 527.
    metadata.primary_writing_mode = exth.primary_writing_mode.clone();
    metadata.page_progression_direction = exth
        .page_progression_direction
        .clone()
        // `rtl` for a writing mode ending `-rl` when EXTH 527 states no direction.
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

    // `fixed_layout` holds for any of the three FXL EXTH records.
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

/// An ASIN: exactly 10 ASCII alphanumeric characters, unlike the UUIDs EXTH 113 also carries.
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
        // `position` stays unset: the annotation carries no number.
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
        // `kind` needs either signal alone.
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
        // `issue_title` appends the date, the one field telling two issues apart.
        assert_eq!(
            issue_title("The New Yorker", Some("2014-12-14 23:00:00+00:00")),
            "The New Yorker — 2014-12-14"
        );
        // `issue_title` adds no second date to a title ending in it.
        assert_eq!(
            issue_title("The New Yorker — 2014-12-14", Some("2014-12-14")),
            "The New Yorker — 2014-12-14"
        );
        // `issue_title` returns a title as it is without a date.
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
        // `publication_title` returns whole a title that never carried a date, or whose
        // suffix is another date.
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
            text_length: 0,
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
