//! Surgical in-place metadata edit for a PDF's `/Info` dictionary.
//!
//! The PDF sibling of [`crate::formats::kfx::metadata_edit`] and
//! [`crate::formats::epub::metadata_edit`], with the same contract across all three: a
//! [`MetadataPatch`] of optional fields, where `None` leaves a field untouched
//! (v1 sets, it does not clear) and an empty patch returns the input unchanged.
//!
//! The edit rides the [`PdfPackage`] harness, so it costs one appended `/Info`
//! generation — the page content is never rewritten.
//!
//! ## Field mapping
//!
//! The editor's fields are format-neutral (they mirror the library's metadata
//! form), and `/Info` (PDF 32000-1 §14.3.3) is a much thinner schema than an
//! OPF's Dublin Core, so three fields map with a caveat:
//!
//! | patch field | `/Info` key | note |
//! |---|---|---|
//! | `title` | `/Title` | |
//! | `authors` | `/Author` | `/Author` is one text string, so a multi-author list is joined with `", "` — the convention `probe_pdf` already reads back |
//! | `publisher` | — | `/Info` has no publisher key; carried in XMP instead, so it is **ignored** here |
//! | `date` | `/CreationDate` | converted to a PDF date string when it parses as `YYYY-MM-DD`; otherwise skipped |
//! | `language` | — | not an `/Info` key (it's catalog `/Lang`); **ignored** here |
//!
//! Fields this cannot express are dropped rather than smuggled into a
//! non-standard `/Info` key a reader would never look at. The library DB row
//! remains the source of truth for those fields; the edit makes the values
//! durable in the artifact for the fields PDF *has*.
//!
//! ## Scope (v1)
//!
//! `/Info` only, not XMP (`/Metadata`). A PDF carrying both can therefore end up
//! with an XMP title disagreeing with the `/Info` title, and PDF 2.0 readers
//! prefer XMP. This crate's own consumer (`probe_pdf`) reads `/Info`, so the
//! edit is authoritative everywhere bokai reads it back; full XMP sync is a
//! later tier.

use lopdf::Object;

use super::doc::encode_pdf_string;
use super::edit::PdfPackage;

/// Which `/Info` fields to set. `None` leaves a field untouched.
///
/// Field-for-field parallel with the KFX and EPUB patches so the app layer can
/// build one patch and dispatch by source format; see the module docs for the
/// fields PDF's `/Info` cannot express.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// Joined with `", "` into the single `/Author` string.
    pub authors: Option<Vec<String>>,
    /// Not representable in `/Info`; accepted for cross-format symmetry and
    /// ignored.
    pub language: Option<String>,
    /// Not representable in `/Info`; accepted for cross-format symmetry and
    /// ignored.
    pub publisher: Option<String>,
    /// Publication date → `/CreationDate`, when it parses as `YYYY-MM-DD`.
    pub date: Option<String>,
}

impl MetadataPatch {
    /// True if the patch sets no field that `/Info` can represent — then
    /// [`edit_metadata`] returns the input unchanged. Note a patch carrying only
    /// `language`/`publisher` is "empty" here: neither has an `/Info` home, so
    /// there is nothing to write.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.authors.is_none() && self.date.is_none()
    }
}

/// Apply `patch` to a PDF's `/Info`, returning the edited bytes.
///
/// The original bytes are preserved verbatim as a prefix; only a new `/Info`
/// generation is appended. Returns the input unchanged for a patch with nothing
/// `/Info` can hold. Errors if the bytes aren't a readable PDF, or if the PDF is
/// encrypted (see [`PdfPackage::parse`]).
pub fn edit_metadata(pdf_bytes: &[u8], patch: &MetadataPatch) -> std::io::Result<Vec<u8>> {
    if patch.is_empty() {
        return Ok(pdf_bytes.to_vec());
    }
    let mut pkg = PdfPackage::parse(pdf_bytes)?;
    {
        let info = pkg.info_dict()?;
        if let Some(t) = &patch.title {
            info.set("Title", encode_pdf_string(t));
        }
        if let Some(authors) = &patch.authors {
            info.set("Author", encode_pdf_string(&authors.join(", ")));
        }
        if let Some(d) = &patch.date
            && let Some(pdf_date) = to_pdf_date(d)
        {
            info.set("CreationDate", Object::string_literal(pdf_date));
        }
    }
    pkg.into_bytes()
}

/// Convert an ISO-ish `YYYY-MM-DD` (optionally with a trailing time we ignore)
/// into a PDF date string `D:YYYYMMDD000000Z` (PDF 32000-1 §7.9.4).
///
/// `None` for anything that isn't a plausible date — the library stores dates as
/// free text, and writing an unparseable value into `/CreationDate` would
/// produce a date field readers silently drop. Leaving the original is better
/// than corrupting it.
fn to_pdf_date(s: &str) -> Option<String> {
    let date = s.trim().get(..10)?;
    let mut parts = date.split('-');
    let y = parts.next()?;
    let m = parts.next()?;
    let d = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    if !(y.bytes().all(|b| b.is_ascii_digit())
        && m.bytes().all(|b| b.is_ascii_digit())
        && d.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let (mn, dn) = (m.parse::<u8>().ok()?, d.parse::<u8>().ok()?);
    if !(1..=12).contains(&mn) || !(1..=31).contains(&dn) {
        return None;
    }
    Some(format!("D:{y}{m}{d}000000Z"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::probe_pdf;

    const MINIMAL: &str = "tests/fixtures/minimal.pdf";

    fn minimal() -> Vec<u8> {
        std::fs::read(MINIMAL).expect("read minimal.pdf fixture")
    }

    /// Read the edit back through `probe_pdf` — the actual downstream consumer
    /// (it feeds `pdf_to_kfx`), not just our own writer's assumptions.
    fn probe(bytes: &[u8]) -> (Option<String>, Option<String>) {
        let d = probe_pdf(bytes.to_vec()).expect("probe edited PDF");
        (d.title, d.author)
    }

    #[test]
    fn edit_title_and_authors() {
        let pdf = minimal();
        assert_eq!(probe(&pdf).0.as_deref(), Some("Tiny Test PDF"));

        let patch = MetadataPatch {
            title: Some("New Title".into()),
            authors: Some(vec!["First Author".into(), "Second Author".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&pdf, &patch).expect("edit");

        let (title, author) = probe(&out);
        assert_eq!(title.as_deref(), Some("New Title"));
        assert_eq!(
            author.as_deref(),
            Some("First Author, Second Author"),
            "multiple authors join into the single /Author string"
        );
        assert!(out.starts_with(&pdf), "still append-only");
    }

    #[test]
    fn edit_single_field_leaves_the_rest() {
        let pdf = minimal();
        let patch = MetadataPatch {
            title: Some("Only The Title".into()),
            ..Default::default()
        };
        let out = edit_metadata(&pdf, &patch).expect("edit");

        let (title, author) = probe(&out);
        assert_eq!(title.as_deref(), Some("Only The Title"));
        assert_eq!(author.as_deref(), Some("A. Tester"), "author untouched");
    }

    #[test]
    fn unicode_title_survives() {
        let pdf = minimal();
        let patch = MetadataPatch {
            title: Some("人間失格".into()),
            authors: Some(vec!["太宰 治".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&pdf, &patch).expect("edit");
        let (title, author) = probe(&out);
        assert_eq!(title.as_deref(), Some("人間失格"));
        assert_eq!(author.as_deref(), Some("太宰 治"));
    }

    #[test]
    fn empty_patch_returns_input_unchanged() {
        let pdf = minimal();
        let out = edit_metadata(&pdf, &MetadataPatch::default()).expect("edit");
        assert_eq!(out, pdf, "an empty patch is a no-op");
    }

    /// A patch of only fields `/Info` can't hold writes nothing rather than
    /// inventing non-standard keys.
    #[test]
    fn unrepresentable_only_patch_is_a_noop() {
        let pdf = minimal();
        let patch = MetadataPatch {
            language: Some("ja".into()),
            publisher: Some("Acme".into()),
            ..Default::default()
        };
        assert!(patch.is_empty());
        assert_eq!(edit_metadata(&pdf, &patch).expect("edit"), pdf);
    }

    #[test]
    fn date_is_written_as_a_pdf_date_string() {
        let pdf = minimal();
        let patch = MetadataPatch {
            date: Some("1948-06-13".into()),
            ..Default::default()
        };
        let out = edit_metadata(&pdf, &patch).expect("edit");

        let doc = crate::formats::pdf::doc::load_pdf(&out).expect("reload");
        let info = doc.trailer.get(b"Info").expect("info ref");
        let dict = crate::formats::pdf::doc::deref(&doc, info)
            .and_then(|o| o.as_dict().ok())
            .expect("info dict");
        let created = dict.get(b"CreationDate").and_then(|o| o.as_str()).unwrap();
        assert_eq!(created, b"D:19480613000000Z");
    }

    #[test]
    fn pdf_date_conversion_accepts_dates_and_rejects_junk() {
        assert_eq!(
            to_pdf_date("1948-06-13").as_deref(),
            Some("D:19480613000000Z")
        );
        // A trailing time component is tolerated; we keep the date part.
        assert_eq!(
            to_pdf_date("1948-06-13T10:30:00Z").as_deref(),
            Some("D:19480613000000Z")
        );
        // Free-text values the library allows must not become bogus dates.
        assert_eq!(to_pdf_date("1948"), None);
        assert_eq!(to_pdf_date("Summer 1948"), None);
        assert_eq!(to_pdf_date("1948-13-01"), None, "month out of range");
        assert_eq!(to_pdf_date("1948-06-99"), None, "day out of range");
        assert_eq!(to_pdf_date(""), None);
        assert_eq!(to_pdf_date("19480613"), None);
    }

    /// An unparseable date writes nothing at all rather than a broken
    /// `/CreationDate` (but a title in the same patch still lands).
    #[test]
    fn unparseable_date_is_skipped_not_written() {
        let pdf = minimal();
        let patch = MetadataPatch {
            title: Some("Kept".into()),
            date: Some("sometime in 1948".into()),
            ..Default::default()
        };
        let out = edit_metadata(&pdf, &patch).expect("edit");
        assert_eq!(probe(&out).0.as_deref(), Some("Kept"));

        let doc = crate::formats::pdf::doc::load_pdf(&out).expect("reload");
        let info = doc.trailer.get(b"Info").expect("info ref");
        let dict = crate::formats::pdf::doc::deref(&doc, info)
            .and_then(|o| o.as_dict().ok())
            .expect("info dict");
        assert!(
            dict.get(b"CreationDate").is_err(),
            "no /CreationDate written for an unparseable value"
        );
    }

    #[test]
    fn non_pdf_bytes_error() {
        let patch = MetadataPatch {
            title: Some("x".into()),
            ..Default::default()
        };
        assert!(edit_metadata(b"not a pdf", &patch).is_err());
    }
}
