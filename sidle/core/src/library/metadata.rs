//! What a metadata edit means, independent of who asked for it.
//!
//! A patch arrives as whatever the editor (or a script) typed; what lands in the
//! row is canonical: authors split and re-joined on the display separator, a
//! harmonized language code, a reading layout and a page direction that agree,
//! romaji that regenerate from a blank, deduped lowercase tags. Product truth,
//! not presentation: every front end reads the same answer here.

use rusqlite::Connection;

use crate::library::db::{self, BookRow, BulkMetadataPatch, MetadataPatch};
use crate::library::paths::LibraryPaths;
use crate::library::{authors, lang, rename, romaji};

/// Canonicalize and validate a full-replacement patch, in place. Every field
/// arrives on each submit, and none carries an "unchanged" state.
pub fn normalize(patch: &mut MetadataPatch) -> anyhow::Result<()> {
    patch.title = patch.title.trim().to_string();
    // Canonicalize authors: split the field on `&`/「、」 (never a plain comma —
    // that's the intra-name "Surname, Given" separator), flip Western names to
    // natural order, and re-join with the unambiguous display separator.
    patch.author = authors::join_display(&authors::parse_input(&patch.author));
    // Harmonize to a canonical code (en-US → en, eng → en, zh-TW → zh-Hant) so a
    // hand-edit stays consistent with what import stores.
    patch.language = lang::normalize(&patch.language);
    // Page progression direction: only "rtl"/"ltr" are meaningful; blank or
    // anything else clears it to None (Auto = device/source default).
    patch.ppd = normalize_ppd(patch.ppd.take());
    // `writing_mode` canonicalizes to one of the four primary-writing-mode
    // values, or `None`. A stated one drives `ppd`, keeping both columns in
    // agreement: a `-rl` layout turns right-to-left.
    patch.writing_mode = normalize_writing_mode(patch.writing_mode.take());
    if let Some(wm) = &patch.writing_mode {
        patch.ppd = Some(ppd_of_writing_mode(wm).to_string());
    }
    // The editable search fields trim and lowercase; a blank one re-renders
    // from the canonicalized title or author. No source file is open here, and
    // the render carries no yomi.
    patch.title_romaji = normalize_romaji(&patch.title_romaji, &patch.title, &patch.language);
    patch.author_romaji = normalize_romaji(&patch.author_romaji, &patch.author, &patch.language);
    trim_to_none(&mut patch.publisher);
    trim_to_none(&mut patch.published_at);
    trim_to_none(&mut patch.series_name);

    if patch.title.is_empty() {
        anyhow::bail!("title cannot be empty");
    }
    if let Some(idx) = patch.series_index
        && (!idx.is_finite() || idx < 0.0)
    {
        anyhow::bail!("series_index must be a non-negative number");
    }
    // A series index without a name has no meaning — drop it so the row stays
    // self-consistent.
    if patch.series_name.is_none() {
        patch.series_index = None;
    }
    patch.tags = canonicalize_tags(std::mem::take(&mut patch.tags));
    Ok(())
}

/// Canonicalize and validate a sparse bulk patch, in place. Only the fields the
/// caller filled in change; tags are additive. See [`BulkMetadataPatch`].
pub fn normalize_bulk(patch: &mut BulkMetadataPatch) -> anyhow::Result<()> {
    trim_to_none(&mut patch.author);
    // Canonicalize the bulk author the same way as a single edit; an empty
    // result clears it back to None.
    if let Some(a) = patch.author.take() {
        let canon = authors::join_display(&authors::parse_input(&a));
        patch.author = (!canon.is_empty()).then_some(canon);
    }
    trim_to_none(&mut patch.language);
    if let Some(l) = patch.language.take() {
        let canon = lang::normalize(&l);
        patch.language = (!canon.is_empty()).then_some(canon);
    }
    // Page direction: lowercase + validate. Bulk can only set rtl/ltr — the
    // sparse "leave unchanged" semantics can't express "clear to Auto".
    if let Some(p) = patch.ppd.take() {
        let p = p.trim().to_ascii_lowercase();
        patch.ppd = match p.as_str() {
            "rtl" | "ltr" => Some(p),
            "" => None,
            _ => anyhow::bail!("page direction must be 'rtl' or 'ltr'"),
        };
    }
    // Reading layout / writing mode: canonicalize like a single edit; when set,
    // it's authoritative for the page direction, so derive `ppd` from it.
    patch.writing_mode = normalize_writing_mode(patch.writing_mode.take());
    if let Some(wm) = &patch.writing_mode {
        patch.ppd = Some(ppd_of_writing_mode(wm).to_string());
    }
    trim_to_none(&mut patch.publisher);
    trim_to_none(&mut patch.published_at);
    trim_to_none(&mut patch.series_name);

    if let Some(idx) = patch.series_index
        && (!idx.is_finite() || idx < 0.0)
    {
        anyhow::bail!("series_index must be a non-negative number");
    }
    patch.add_tags = canonicalize_tags(std::mem::take(&mut patch.add_tags));
    patch.remove_tags = canonicalize_tags(std::mem::take(&mut patch.remove_tags));
    Ok(())
}

/// Normalize, persist, and rename the book's files to match, keeping the
/// library folder and a force-reconvert's derived basename in step with the
/// metadata. Returns the refreshed row.
pub fn apply(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: i64,
    mut patch: MetadataPatch,
) -> anyhow::Result<BookRow> {
    normalize(&mut patch)?;
    db::update_metadata(conn, book_id, &patch)?;
    rename::rename_book_files(conn, paths, book_id)?
        .ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))
}

/// Normalize a sparse patch and apply it to every book in `book_ids`, returning
/// the refreshed rows. A row that vanished mid-run is skipped, not an error.
pub fn apply_bulk(
    conn: &Connection,
    book_ids: &[i64],
    mut patch: BulkMetadataPatch,
) -> anyhow::Result<Vec<BookRow>> {
    normalize_bulk(&mut patch)?;
    let mut rows = Vec::with_capacity(book_ids.len());
    for id in book_ids {
        db::apply_bulk_patch(conn, *id, &patch)?;
        if let Some(r) = db::get_book(conn, *id)? {
            rows.push(r);
        }
    }
    Ok(rows)
}

/// Record the Amazon catalogue id typed for a book, or clear it with `None`.
/// The value is validated, fetches the colour cover from `/images/P/<asin>`,
/// and reaches no file. Two books may not share one: it names an edition.
pub fn set_amazon_asin(
    conn: &Connection,
    book_id: i64,
    asin: Option<&str>,
) -> anyhow::Result<BookRow> {
    let asin = check_amazon_asin(conn, book_id, asin)?;
    db::set_amazon_asin(conn, book_id, asin.as_deref())?;
    // Curation moves `updated_at`, so a later merge keeps this edit. The bump
    // can't live in `db::set_amazon_asin` — the re-key path calls that
    // mechanically, and a mechanical rewrite is not an edit.
    db::set_book_updated_at(conn, book_id, &db::now_iso())?;
    db::get_book(conn, book_id)?.ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))
}

/// What [`set_amazon_asin`] stores, or the reason it cannot. A caller with
/// other work — writing the edit into the source file — refuses a bad value
/// ahead of starting.
pub fn check_amazon_asin(
    conn: &Connection,
    book_id: i64,
    asin: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Some(asin) = asin.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if !crate::library::cover_fetch::looks_like_real_amazon_asin(asin) {
        anyhow::bail!("an ASIN is 10 characters of A–Z and 0–9; got {asin:?}");
    }
    if let Some(other) = db::book_id_with_amazon_asin(conn, asin, book_id)? {
        anyhow::bail!("ASIN {asin} is already on book {other}");
    }
    Ok(Some(asin.to_string()))
}

/// Build a full-replacement patch out of a book's current values, for a caller
/// that wants to change one field and leave the rest alone.
pub fn patch_from(book: &BookRow) -> MetadataPatch {
    MetadataPatch {
        title: book.title.clone(),
        author: book.author.clone(),
        language: book.language.clone(),
        ppd: book.ppd.clone(),
        writing_mode: book.writing_mode.clone(),
        publisher: book.publisher.clone(),
        published_at: book.published_at.clone(),
        series_name: book.series_name.clone(),
        series_index: book.series_index,
        tags: book.tags.clone(),
        title_romaji: book.title_romaji.clone(),
        author_romaji: book.author_romaji.clone(),
    }
}

/// The page direction a reading layout implies: a `-rl` layout turns
/// right-to-left.
pub fn ppd_of_writing_mode(writing_mode: &str) -> &'static str {
    if writing_mode.ends_with("-rl") {
        "rtl"
    } else {
        "ltr"
    }
}

/// Canonicalize a page-progression value: `rtl`/`ltr`, or `None` for blank or
/// anything else (Auto = the device/source default).
pub fn normalize_ppd(v: Option<String>) -> Option<String> {
    match v.map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if s == "rtl" || s == "ltr" => Some(s),
        _ => None,
    }
}

/// Canonicalize a reading-layout / writing-mode value to one of the four
/// `primary-writing-mode` strings (hyphenated, lowercase), or `None` (Auto) for
/// anything else.
pub fn normalize_writing_mode(v: Option<String>) -> Option<String> {
    let v = v?.trim().to_ascii_lowercase().replace('_', "-");
    match v.as_str() {
        "horizontal-lr" | "horizontal-rl" | "vertical-rl" | "vertical-lr" => Some(v),
        _ => None,
    }
}

/// Trim, lowercase, drop empties, dedupe in-order. Lowercasing is a no-op for
/// CJK characters and gives consistent grouping for ASCII tags ("Sci-Fi" and
/// "sci-fi" merge).
pub fn canonicalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(tags.len());
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// Normalize an edited romaji field: trim and lowercase, and re-render a blank
/// one from `source` through the engine.
pub fn normalize_romaji(value: &str, source: &str, language: &str) -> String {
    let trimmed = value.trim().to_lowercase();
    if trimmed.is_empty() {
        romaji::romanize_field(source, None, language)
    } else {
        trimmed
    }
}

/// Trim an optional string in place; a now-empty value collapses to `None`
/// ("leave unchanged" in bulk semantics, "absent" in a full patch).
fn trim_to_none(s: &mut Option<String>) {
    if let Some(v) = s {
        let t = v.trim().to_string();
        if t.is_empty() {
            *s = None;
        } else {
            *v = t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch() -> MetadataPatch {
        MetadataPatch {
            title: "  A Title  ".into(),
            author: "Writer, Ann & bob maker".into(),
            language: "en-US".into(),
            ppd: None,
            writing_mode: None,
            publisher: Some("  ".into()),
            published_at: Some(" 2021 ".into()),
            series_name: None,
            series_index: Some(3.0),
            tags: vec!["Sci-Fi".into(), "sci-fi".into(), "  ".into()],
            title_romaji: String::new(),
            author_romaji: "  MIXED Case ".into(),
        }
    }

    /// A library holding one row per title, enough to exercise the catalogue-id
    /// rule (which is about the row, not about any file).
    fn library_with(titles: &[&str]) -> (tempfile::TempDir, Connection, Vec<i64>) {
        let tmp = tempfile::tempdir().unwrap();
        let conn = db::open(&tmp.path().join("library.db")).unwrap();
        let now = db::now_iso();
        let ids = titles
            .iter()
            .map(|title| {
                db::insert_book(
                    &conn,
                    &db::NewBook {
                        sha256: title,
                        title,
                        author: "",
                        language: "en",
                        ppd: None,
                        epub_path: None,
                        cover_path: None,
                        kfx_path: None,
                        kfx_sha256: None,
                        pdf_path: None,
                        file_size: 0,
                        imported_at: &now,
                        asin: None,
                        amazon_asin: None,
                        publisher: None,
                        published_at: None,
                        series_name: None,
                        series_index: None,
                        tags: &[],
                        title_romaji: "",
                        author_romaji: "",
                        source_format: None,
                    },
                )
                .unwrap()
            })
            .collect();
        (tmp, conn, ids)
    }

    #[test]
    fn a_catalogue_id_is_validated_before_it_is_stored() {
        let (_tmp, conn, ids) = library_with(&["A", "B"]);
        let (a, b) = (ids[0], ids[1]);

        // Free text fetches nothing, so it never lands as if it might.
        assert!(set_amazon_asin(&conn, a, Some("not an asin")).is_err());
        // Neither does a file's own identity: it names no item in the catalogue.
        assert!(set_amazon_asin(&conn, a, Some("LSEAIPOJGKOLNRDWIOODBTDTEPBWBTFR")).is_err());

        let row = set_amazon_asin(&conn, a, Some("  B07PXGQC1Q ")).unwrap();
        assert_eq!(row.amazon_asin.as_deref(), Some("B07PXGQC1Q"));

        // One id names one edition, and one cover.
        assert!(set_amazon_asin(&conn, b, Some("B07PXGQC1Q")).is_err());

        // Blank clears, which is the way out of a wrong paste.
        assert_eq!(
            set_amazon_asin(&conn, a, Some(" ")).unwrap().amazon_asin,
            None
        );
        assert_eq!(set_amazon_asin(&conn, a, None).unwrap().amazon_asin, None);
    }

    #[test]
    fn a_full_patch_is_canonicalized() {
        let mut p = patch();
        normalize(&mut p).unwrap();
        assert_eq!(p.title, "A Title");
        assert_eq!(p.author, "Ann Writer & bob maker");
        assert_eq!(p.language, "en");
        assert_eq!(p.publisher, None, "a whitespace-only field clears");
        assert_eq!(p.published_at.as_deref(), Some("2021"));
        assert_eq!(p.tags, vec!["sci-fi".to_string()], "deduped, lowercased");
        assert_eq!(p.author_romaji, "mixed case");
        assert!(!p.title_romaji.is_empty(), "a blank romaji regenerates");
        assert_eq!(
            p.series_index, None,
            "an index without a series name is meaningless"
        );
    }

    #[test]
    fn a_vertical_layout_sets_the_page_direction_with_it() {
        let mut p = patch();
        p.writing_mode = Some("VERTICAL_RL".into());
        p.ppd = Some("ltr".into());
        normalize(&mut p).unwrap();
        assert_eq!(p.writing_mode.as_deref(), Some("vertical-rl"));
        assert_eq!(
            p.ppd.as_deref(),
            Some("rtl"),
            "the layout is authoritative over a stale direction"
        );
    }

    #[test]
    fn an_unknown_layout_clears_to_auto() {
        assert_eq!(normalize_writing_mode(Some("sideways".into())), None);
        assert_eq!(normalize_ppd(Some("sideways".into())), None);
        assert_eq!(normalize_ppd(Some(" RTL ".into())).as_deref(), Some("rtl"));
    }

    #[test]
    fn an_empty_title_is_refused() {
        let mut p = patch();
        p.title = "   ".into();
        assert!(normalize(&mut p).is_err());
    }

    #[test]
    fn a_negative_series_index_is_refused() {
        let mut p = patch();
        p.series_name = Some("Saga".into());
        p.series_index = Some(-1.0);
        assert!(normalize(&mut p).is_err());
    }

    #[test]
    fn tags_keep_the_order_they_were_first_written_in() {
        let got = canonicalize_tags(vec!["bbb".into(), "aaa".into(), "BBB".into()]);
        assert_eq!(got, vec!["bbb", "aaa"]);
    }

    #[test]
    fn cjk_tags_pass_through_unchanged() {
        // CJK has no case: lowercase is a no-op, trim applies, and the
        // trimmed duplicate collapses.
        let got = canonicalize_tags(vec![" 小説 ".into(), "ライトノベル".into(), "小説".into()]);
        assert_eq!(got, vec!["小説", "ライトノベル"]);
        // A mixed tag lowercases only the half that has a case.
        let mixed = canonicalize_tags(vec!["ライトSciFi".into(), "ライトscifi".into()]);
        assert_eq!(mixed, vec!["ライトscifi"]);
    }

    #[test]
    fn a_bulk_patch_refuses_a_free_text_direction() {
        let mut p = BulkMetadataPatch {
            ppd: Some("sideways".into()),
            ..Default::default()
        };
        assert!(normalize_bulk(&mut p).is_err());
    }
}
