//! Surgical in-place metadata edit for a PDF's `/Info` dictionary.

use lopdf::{Document, Object};

use super::doc::{decode_pdf_string, deref, encode_pdf_string};
use super::edit::PdfPackage;

/// Which `/Info` fields to set. `None` leaves a field untouched.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// Joined by [`crate::model::join_names`] into `/Author`; an empty list removes it.
    pub authors: Option<Vec<String>>,
    /// Not representable in `/Info`; ignored.
    pub language: Option<String>,
    /// Not representable in `/Info`; ignored.
    pub publisher: Option<String>,
    /// Publication date → `/CreationDate`, when it parses as `YYYY-MM-DD`.
    pub date: Option<String>,
}

impl MetadataPatch {
    /// True if the patch sets no field `/Info` can represent.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.authors.is_none() && self.date.is_none()
    }
}

/// Apply `patch` to `/Info` as an appended incremental update; returns
/// `pdf_bytes` itself when `/Info` holds every value.
pub fn edit_metadata(pdf_bytes: &[u8], patch: &MetadataPatch) -> std::io::Result<Vec<u8>> {
    if patch.is_empty() {
        return Ok(pdf_bytes.to_vec());
    }
    let mut pkg = PdfPackage::parse(pdf_bytes)?;
    let current = |key: &[u8]| info_bytes(pkg.original(), key);

    // (key, new value); `None` removes the key.
    let mut sets: Vec<(&str, Option<Object>)> = Vec::new();
    if let Some(t) = &patch.title
        && current(b"Title").map(|b| decode_pdf_string(&b)).as_deref() != Some(t.as_str())
    {
        sets.push(("Title", Some(encode_pdf_string(t))));
    }
    if let Some(authors) = &patch.authors {
        let joined = crate::model::join_names(authors);
        let now = current(b"Author").map(|b| decode_pdf_string(&b));
        if joined.is_empty() {
            if now.is_some() {
                sets.push(("Author", None));
            }
        } else if now.as_deref() != Some(joined.as_str()) {
            sets.push(("Author", Some(encode_pdf_string(&joined))));
        }
    }
    if let Some(pdf_date) = patch.date.as_deref().and_then(to_pdf_date)
        && current(b"CreationDate").as_deref() != Some(pdf_date.as_bytes())
    {
        sets.push(("CreationDate", Some(Object::string_literal(pdf_date))));
    }
    if sets.is_empty() {
        return Ok(pdf_bytes.to_vec());
    }

    let info = pkg.info_dict()?;
    for (key, value) in sets {
        match value {
            Some(v) => info.set(key, v),
            None => {
                info.remove(key.as_bytes());
            }
        }
    }
    pkg.into_bytes()
}

/// The raw string bytes of `/Info`'s `key`, if it holds a string.
fn info_bytes(doc: &Document, key: &[u8]) -> Option<Vec<u8>> {
    let info = deref(doc, doc.trailer.get(b"Info").ok()?)?.as_dict().ok()?;
    deref(doc, info.get(key).ok()?)?
        .as_str()
        .ok()
        .map(<[u8]>::to_vec)
}

/// Convert an ISO `YYYY-MM-DD`, any trailing time dropped, into a PDF date string
/// `D:YYYYMMDD000000Z` (PDF 32000-1 §7.9.4).
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

    /// `(title, author)` as `probe_pdf` reads them back.
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
            Some("First Author & Second Author"),
            "multiple authors join into the single /Author string"
        );
        assert!(out.starts_with(&pdf), "still append-only");
    }

    #[test]
    fn values_info_already_holds_append_nothing() {
        let pdf = minimal();
        let patch = MetadataPatch {
            title: Some("Tiny Test PDF".into()),
            authors: Some(vec!["A. Tester".into()]),
            ..Default::default()
        };
        assert_eq!(edit_metadata(&pdf, &patch).expect("edit"), pdf);

        let dated = edit_metadata(
            &pdf,
            &MetadataPatch {
                date: Some("1948-06-13".into()),
                ..Default::default()
            },
        )
        .expect("edit");
        let again = MetadataPatch {
            date: Some("1948-06-13".into()),
            ..Default::default()
        };
        assert_eq!(edit_metadata(&dated, &again).expect("edit"), dated);
    }

    #[test]
    fn an_empty_author_list_removes_the_author() {
        let pdf = minimal();
        let patch = MetadataPatch {
            authors: Some(Vec::new()),
            ..Default::default()
        };
        let out = edit_metadata(&pdf, &patch).expect("edit");
        assert_eq!(probe(&out).1, None);
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
    fn cjk_authors_join_with_the_ideographic_comma() {
        let patch = MetadataPatch {
            authors: Some(vec!["春野一郎".into(), "秋山次郎".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&minimal(), &patch).expect("edit");
        assert_eq!(probe(&out).1.as_deref(), Some("春野一郎、秋山次郎"));
    }

    #[test]
    fn empty_patch_returns_input_unchanged() {
        let pdf = minimal();
        let out = edit_metadata(&pdf, &MetadataPatch::default()).expect("edit");
        assert_eq!(out, pdf, "an empty patch is a no-op");
    }

    /// `language` and `publisher` alone write nothing.
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
        // A trailing time is dropped, the date part kept.
        assert_eq!(
            to_pdf_date("1948-06-13T10:30:00Z").as_deref(),
            Some("D:19480613000000Z")
        );
        // Free text and malformed dates convert to `None`.
        assert_eq!(to_pdf_date("1948"), None);
        assert_eq!(to_pdf_date("Summer 1948"), None);
        assert_eq!(to_pdf_date("1948-13-01"), None, "month out of range");
        assert_eq!(to_pdf_date("1948-06-99"), None, "day out of range");
        assert_eq!(to_pdf_date(""), None);
        assert_eq!(to_pdf_date("19480613"), None);
    }

    /// An unparseable date writes no `/CreationDate`, and a title beside it lands.
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
