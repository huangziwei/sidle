//! A metadata edit: the row, and the book's files, which state the same title,
//! credits, language, publisher and date.

use std::path::Path;

use anyhow::Context;
use bokai::formats::epub::metadata_edit as epub_meta;
use bokai::formats::kfx::metadata_edit as kfx_meta;
use bokai::formats::pdf::metadata_edit as pdf_meta;
use rusqlite::Connection;
use serde::Serialize;

use crate::library::credits::{self, Credit};
use crate::library::db::{self, BookRow, BulkMetadataPatch, MetadataPatch};
use crate::library::paths::LibraryPaths;
use crate::library::{lang, rename, romaji, source};

/// Canonicalize and validate a full-replacement patch against the book's
/// `current` credits, in place. Every field arrives on each submit, and none
/// carries an "unchanged" state; `credits` comes out stated.
pub fn normalize(patch: &mut MetadataPatch, current: &[Credit]) -> anyhow::Result<()> {
    patch.title = patch.title.trim().to_string();
    // A stated list is canonicalized; else a changed author line — split on
    // `&`/「、」 (a comma is intra-name), `Surname, Given` flipped — replaces the
    // authors, and an unchanged one leaves every credit as it is.
    let list = match patch.credits.take() {
        Some(list) => credits::normalize(list),
        None if patch.author.trim() == credits::author_line(current) => current.to_vec(),
        None => credits::with_authors(current, &credits::parse_input(&patch.author)),
    };
    patch.author = credits::author_line(&list);
    patch.credits = Some(list);
    // en-US → en, eng → en, zh-TW → zh-Hant, as an import stores it.
    patch.language = lang::normalize(&patch.language);
    // `rtl`/`ltr` only; anything else is `None` (Auto).
    patch.ppd = normalize_ppd(patch.ppd.take());
    // A stated `writing_mode` sets `ppd`: a `-rl` layout turns right-to-left.
    patch.writing_mode = normalize_writing_mode(patch.writing_mode.take());
    if let Some(wm) = &patch.writing_mode {
        patch.ppd = Some(ppd_of_writing_mode(wm).to_string());
    }
    // A blank romaji re-renders from the title or author, without yomi.
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
    // A `series_index` without a `series_name` is dropped.
    if patch.series_name.is_none() {
        patch.series_index = None;
    }
    patch.tags = canonicalize_tags(std::mem::take(&mut patch.tags));
    Ok(())
}

/// Canonicalize and validate a sparse bulk patch, in place: only its `Some`
/// fields change, and tags are additive. See [`BulkMetadataPatch`].
pub fn normalize_bulk(patch: &mut BulkMetadataPatch) -> anyhow::Result<()> {
    trim_to_none(&mut patch.author);
    // Canonicalize the bulk author the same way as a single edit; an empty
    // result clears it back to None.
    if let Some(a) = patch.author.take() {
        let canon = credits::join_display(&credits::parse_input(&a));
        patch.author = (!canon.is_empty()).then_some(canon);
    }
    patch.credits = credits::normalize(std::mem::take(&mut patch.credits));
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
    // A stated `writing_mode` sets `ppd`, as in a single edit.
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

/// The database half of an edit, done under the connection; [`Edited::bake`] is
/// the half that writes the book's files.
#[derive(Debug, Clone)]
pub struct Edited {
    pub row: BookRow,
    /// What the edit emptied, which the files give up with it.
    pub cleared: Cleared,
    /// Whether the edit wrote a field the files carry.
    pub reaches_files: bool,
}

impl Edited {
    /// Write the edited row into the book's files.
    pub fn bake(&self) -> Vec<FileReport> {
        if !self.reaches_files {
            return Vec::new();
        }
        bake(&self.row, &FileMetadata::of(&self.row), self.cleared, true)
    }
}

/// Normalize, persist, and rename the book's files to match, keeping the
/// library folder and a force-reconvert's derived basename in step with the
/// metadata. The files' contents are [`Edited::bake`]'s.
pub fn edit_row(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: i64,
    mut patch: MetadataPatch,
) -> anyhow::Result<Edited> {
    let before =
        db::get_book(conn, book_id)?.ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))?;
    normalize(&mut patch, &before.credits)?;
    db::update_metadata(conn, book_id, &patch)?;
    let row = rename::rename_book_files(conn, paths, book_id)?
        .ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))?;
    Ok(Edited {
        cleared: Cleared::between(&before, &row),
        reaches_files: !FileMetadata::of(&before).states_the_same(&FileMetadata::of(&row)),
        row,
    })
}

/// [`edit_row`] and then [`Edited::bake`], holding `conn` throughout. Returns
/// the refreshed row, logging any file left unwritten.
pub fn apply(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: i64,
    patch: MetadataPatch,
) -> anyhow::Result<BookRow> {
    let edited = edit_row(conn, paths, book_id, patch)?;
    log_failures(&edited.row, &edited.bake());
    Ok(edited.row)
}

/// Normalize a sparse patch, apply it to every book in `book_ids` and rename
/// their files to match. A row that vanished mid-run is skipped, not an error.
pub fn edit_rows(
    conn: &Connection,
    paths: &LibraryPaths,
    book_ids: &[i64],
    mut patch: BulkMetadataPatch,
) -> anyhow::Result<Vec<Edited>> {
    normalize_bulk(&mut patch)?;
    let reaches_files = patch.author.is_some()
        || !patch.credits.is_empty()
        || patch.language.is_some()
        || patch.publisher.is_some()
        || patch.published_at.is_some();
    let mut out = Vec::with_capacity(book_ids.len());
    for id in book_ids {
        db::apply_bulk_patch(conn, *id, &patch)?;
        if let Some(row) = rename::rename_book_files(conn, paths, *id)? {
            out.push(Edited {
                row,
                cleared: Cleared::default(),
                reaches_files,
            });
        }
    }
    Ok(out)
}

/// [`edit_rows`] and then each book's [`Edited::bake`], holding `conn`
/// throughout. Returns the refreshed rows.
pub fn apply_bulk(
    conn: &Connection,
    paths: &LibraryPaths,
    book_ids: &[i64],
    patch: BulkMetadataPatch,
) -> anyhow::Result<Vec<BookRow>> {
    let edited = edit_rows(conn, paths, book_ids, patch)?;
    for e in &edited {
        log_failures(&e.row, &e.bake());
    }
    Ok(edited.into_iter().map(|e| e.row).collect())
}

/// Print each file of `book` that could not be brought into agreement.
pub fn log_failures(book: &BookRow, reports: &[FileReport]) {
    for r in reports {
        if let Some(e) = &r.error {
            eprintln!(
                "[sidle/metadata] book {} {}: not written: {e}",
                book.id, r.path
            );
        }
    }
}

// --- the book's files ---------------------------------------------------------

/// The fields every file of a book states for its row; series, tags and romaji
/// stay in the row. Every format names the credits; an EPUB states their roles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileMetadata {
    pub title: String,
    pub creators: Vec<Credit>,
    pub language: String,
    pub publisher: Option<String>,
    pub date: Option<String>,
}

impl FileMetadata {
    /// The fields `book` holds.
    pub fn of(book: &BookRow) -> Self {
        Self {
            title: book.title.clone(),
            creators: book.credits.clone(),
            language: book.language.clone(),
            publisher: book.publisher.clone(),
            date: book.published_at.clone(),
        }
    }

    /// Whether `self` and `other` state the same fields to a file, no role and `aut` alike.
    pub fn states_the_same(&self, other: &Self) -> bool {
        self.title == other.title
            && self.language == other.language
            && self.publisher == other.publisher
            && self.date == other.date
            && self.creators.len() == other.creators.len()
            && self
                .creators
                .iter()
                .zip(&other.creators)
                .all(|(a, b)| a.name == b.name && bokai::model::roles_agree(&a.roles, &b.roles))
    }
}

/// The fields an edit emptied: the only fields a bake removes from a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleared {
    pub creators: bool,
    pub publisher: bool,
    pub date: bool,
}

impl Cleared {
    /// What the edit from `before` to `after` emptied.
    pub fn between(before: &BookRow, after: &BookRow) -> Self {
        Self {
            creators: !before.credits.is_empty() && after.credits.is_empty(),
            publisher: before.publisher.is_some() && after.publisher.is_none(),
            date: before.published_at.is_some() && after.published_at.is_none(),
        }
    }
}

/// A book file's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    Epub,
    Kfx,
    Pdf,
}

impl FileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            FileFormat::Epub => "epub",
            FileFormat::Kfx => "kfx",
            FileFormat::Pdf => "pdf",
        }
    }
}

/// One field a file disagrees with its row on; an empty `row` is a cleared field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Drift {
    pub field: &'static str,
    pub file: String,
    pub row: String,
}

/// One of a book's files, and how it stood against the row.
#[derive(Debug, Clone, Serialize)]
pub struct FileReport {
    pub path: String,
    pub format: FileFormat,
    /// The fields it disagrees on; empty for an agreeing file.
    pub drift: Vec<Drift>,
    /// True once the corrected file is on disk.
    pub written: bool,
    /// Why the file could not be read or rewritten.
    pub error: Option<String>,
}

/// Every file the library keeps for `book` that is on disk, with its format.
pub fn files_of(book: &BookRow) -> Vec<(FileFormat, String)> {
    [
        (FileFormat::Epub, &book.epub_path),
        (FileFormat::Kfx, &book.kfx_path),
        (FileFormat::Pdf, &book.pdf_path),
    ]
    .into_iter()
    .filter_map(|(format, path)| Some((format, path.clone()?)))
    .filter(|(_, path)| Path::new(path).is_file())
    .collect()
}

/// Bring every file of `book` into agreement with `want`, rewriting only the
/// files that disagree; with `write` false, only report.
pub fn bake(book: &BookRow, want: &FileMetadata, cleared: Cleared, write: bool) -> Vec<FileReport> {
    files_of(book)
        .into_iter()
        .map(|(format, path)| bake_file(&path, format, want, cleared, write))
        .collect()
}

fn bake_file(
    path: &str,
    format: FileFormat,
    want: &FileMetadata,
    cleared: Cleared,
    write: bool,
) -> FileReport {
    let mut report = FileReport {
        path: path.to_string(),
        format,
        drift: Vec::new(),
        written: false,
        error: None,
    };
    let mut run = || -> anyhow::Result<()> {
        let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
        report.drift = drift(format, &stated(format, &bytes)?, want, cleared);
        if write && !report.drift.is_empty() {
            let rewritten = patched(format, &bytes, &report.drift, want)?;
            if rewritten != bytes {
                source::replace_file(Path::new(path), &rewritten)?;
                report.written = true;
            }
        }
        Ok(())
    };
    if let Err(e) = run() {
        report.error = Some(format!("{e:#}"));
    }
    report
}

/// `bytes` restated to agree with `want`, or `None` when they agree; for a file
/// an import has yet to store.
pub fn baked_bytes(
    format: FileFormat,
    bytes: &[u8],
    want: &FileMetadata,
) -> anyhow::Result<Option<Vec<u8>>> {
    let drift = drift(format, &stated(format, bytes)?, want, Cleared::default());
    if drift.is_empty() {
        return Ok(None);
    }
    let rewritten = patched(format, bytes, &drift, want)?;
    Ok((rewritten != bytes).then_some(rewritten))
}

/// The credits `book`'s EPUB states when they name the row's people (one author line)
/// and say more: another role, or one credit the row holds as two. `None` without an
/// EPUB on disk, for other people, or for nothing new.
pub fn credits_from_epub(book: &BookRow) -> anyhow::Result<Option<Vec<Credit>>> {
    let Some(path) = book.epub_path.as_deref().filter(|p| Path::new(p).is_file()) else {
        return Ok(None);
    };
    let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
    let file = credits::from_metadata(&stated(FileFormat::Epub, &bytes)?);
    let line = |c: &[Credit]| credits::join_display(&credits::names(c));
    if line(&file) != line(&book.credits) {
        return Ok(None);
    }
    let adds_nothing = file.len() == book.credits.len()
        && file
            .iter()
            .zip(&book.credits)
            .all(|(f, r)| f.name == r.name && bokai::model::roles_agree(&f.roles, &r.roles));
    Ok((!adds_nothing).then_some(file))
}

/// Store `credits` for `book_id` and rename its files to the author line they
/// draw. The files are not written: the credits come from them.
pub fn adopt_credits(
    conn: &Connection,
    paths: &LibraryPaths,
    book_id: i64,
    credits: &[Credit],
) -> anyhow::Result<BookRow> {
    db::set_credits(conn, book_id, credits)?;
    rename::rename_book_files(conn, paths, book_id)?
        .ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))
}

/// What a file states, read the way an import reads it.
fn stated(format: FileFormat, bytes: &[u8]) -> anyhow::Result<bokai::Metadata> {
    Ok(match format {
        FileFormat::Epub => bokai::Book::from_bytes(bytes, bokai::Format::Epub)?
            .metadata()
            .clone(),
        FileFormat::Kfx => bokai::Book::from_bytes(bytes, bokai::Format::Kfx)?
            .metadata()
            .clone(),
        FileFormat::Pdf => {
            let doc = bokai::import::probe_pdf(bytes.to_vec())?;
            bokai::Metadata {
                title: doc.title.unwrap_or_default(),
                creators: doc
                    .author
                    .into_iter()
                    .map(bokai::Contributor::named)
                    .collect(),
                ..Default::default()
            }
        }
    })
}

/// `name` with its whitespace runs collapsed to single spaces, as a credit stores it.
fn collapsed(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `file` states roles other than those `want` claims, a credit stating none
/// claiming nothing: place by place for lists pairing name for name, else against a
/// creator whose name contains the credit's.
fn roles_drift(file: &[bokai::Contributor], want: &[Credit]) -> bool {
    let paired = file.len() == want.len()
        && file
            .iter()
            .zip(want)
            .all(|(f, w)| collapsed(&f.name) == w.name);
    if paired {
        return file
            .iter()
            .zip(want)
            .any(|(f, w)| !w.roles.is_empty() && !bokai::model::roles_agree(&f.roles, &w.roles));
    }
    want.iter().filter(|w| !w.roles.is_empty()).any(|w| {
        !file.iter().any(|f| {
            collapsed(&f.name).contains(w.name.as_str())
                && bokai::model::roles_agree(&f.roles, &w.roles)
        })
    })
}

/// Each name with its roles: `A (aut); B (ill)`.
fn credit_list<'a>(credits: impl Iterator<Item = (&'a str, &'a [String])>) -> String {
    credits
        .map(|(name, roles)| match roles {
            [] => name.to_string(),
            _ => format!("{name} ({})", roles.join(", ")),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The fields `file` disagrees with `want` on; a PDF's `/Info` holds the title
/// and authors alone, and only an EPUB states roles.
fn drift(
    format: FileFormat,
    file: &bokai::Metadata,
    want: &FileMetadata,
    cleared: Cleared,
) -> Vec<Drift> {
    fn push(out: &mut Vec<Drift>, field: &'static str, file: &str, row: &str) {
        out.push(Drift {
            field,
            file: file.to_string(),
            row: row.to_string(),
        });
    }
    let mut out = Vec::new();

    if file.title.trim() != want.title.trim() {
        push(&mut out, "title", &file.title, &want.title);
    }
    let file_names: Vec<String> = file
        .creators
        .iter()
        .map(|c| collapsed(&c.name))
        .filter(|a| !a.is_empty())
        .collect();
    let (now, then) = (
        credits::join_display(&file_names),
        credits::join_display(&credits::names(&want.creators)),
    );
    let names_drift = now != then && (!want.creators.is_empty() || cleared.creators);
    if names_drift {
        push(&mut out, "author", &now, &then);
    }
    if format == FileFormat::Epub && !names_drift && roles_drift(&file.creators, &want.creators) {
        push(
            &mut out,
            "role",
            &credit_list(
                file.creators
                    .iter()
                    .map(|c| (c.name.as_str(), c.roles.as_slice())),
            ),
            &credit_list(
                want.creators
                    .iter()
                    .map(|c| (c.name.as_str(), c.roles.as_slice())),
            ),
        );
    }
    if format == FileFormat::Pdf {
        return out;
    }

    if !want.language.is_empty() && lang::normalize(&file.language) != want.language {
        push(&mut out, "language", &file.language, &want.language);
    }
    let present = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match (present(&file.publisher), present(&want.publisher)) {
        (Some(f), Some(w)) if f != w => push(&mut out, "publisher", &f, &w),
        (None, Some(w)) => push(&mut out, "publisher", "", &w),
        (Some(f), None) if cleared.publisher => push(&mut out, "publisher", &f, ""),
        _ => {}
    }
    match (present(&file.date), present(&want.date)) {
        (Some(f), Some(w)) if !dates_agree(&f, &w) => push(&mut out, "date", &f, &w),
        (None, Some(w)) => push(&mut out, "date", "", &w),
        (Some(f), None) if cleared.date => push(&mut out, "date", &f, ""),
        _ => {}
    }
    out
}

/// Whether two dates name the same day on the precision they share: `2009`
/// agrees with `2009-05-19`, and `2009-05-19T00:00:00+00:00` with both.
fn dates_agree(a: &str, b: &str) -> bool {
    let day = |s: &str| s.split(['T', ' ']).next().unwrap_or("").to_string();
    let (a, b) = (day(a), day(b));
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.starts_with(&short) && matches!(long.as_bytes().get(short.len()), None | Some(b'-'))
}

/// `bytes` with every drifted field set to the row's value.
fn patched(
    format: FileFormat,
    bytes: &[u8],
    drift: &[Drift],
    want: &FileMetadata,
) -> anyhow::Result<Vec<u8>> {
    let row = |field: &str| {
        drift
            .iter()
            .find(|d| d.field == field)
            .map(|d| d.row.clone())
    };
    let authors = row("author").map(|_| credits::names(&want.creators));
    let creators = (row("author").is_some() || row("role").is_some()).then(|| {
        want.creators
            .iter()
            .map(|c| bokai::Contributor {
                name: c.name.clone(),
                file_as: None,
                roles: c.roles.clone(),
            })
            .collect()
    });
    Ok(match format {
        FileFormat::Epub => epub_meta::edit_metadata(
            bytes,
            &epub_meta::MetadataPatch {
                title: row("title"),
                creators,
                language: row("language"),
                publisher: row("publisher"),
                date: row("date"),
                asin: None,
            },
        )?,
        FileFormat::Kfx => kfx_meta::edit_metadata(
            bytes,
            &kfx_meta::MetadataPatch {
                title: row("title"),
                authors,
                language: row("language"),
                publisher: row("publisher"),
                issue_date: row("date"),
                asin: None,
                content_id: None,
            },
        )?,
        FileFormat::Pdf => pdf_meta::edit_metadata(
            bytes,
            &pdf_meta::MetadataPatch {
                title: row("title"),
                authors,
                ..Default::default()
            },
        )?,
    })
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
    // A curated `amazon_asin` moves `updated_at`; `db::set_amazon_asin` alone does not.
    db::set_book_updated_at(conn, book_id, &db::now_iso())?;
    db::get_book(conn, book_id)?.ok_or_else(|| anyhow::anyhow!("book {book_id} not found"))
}

/// What [`set_amazon_asin`] stores, or the reason it cannot.
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

/// A full-replacement patch holding `book`'s current values.
pub fn patch_from(book: &BookRow) -> MetadataPatch {
    MetadataPatch {
        title: book.title.clone(),
        author: book.author.clone(),
        credits: None,
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

/// Trim and lowercase an edited romaji field; a blank one re-renders from `source`.
pub fn normalize_romaji(value: &str, source: &str, language: &str) -> String {
    let trimmed = value.trim().to_lowercase();
    if trimmed.is_empty() {
        romaji::romanize_field(source, None, language)
    } else {
        trimmed
    }
}

/// Trim an optional string in place; an empty result is `None` ("leave unchanged"
/// in a bulk patch, "absent" in a full one).
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
            credits: None,
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
                        credits: &[],
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

        // Free text is refused.
        assert!(set_amazon_asin(&conn, a, Some("not an asin")).is_err());
        // A file's own identity names no catalogue item, and is refused.
        assert!(set_amazon_asin(&conn, a, Some("LSEAIPOJGKOLNRDWIOODBTDTEPBWBTFR")).is_err());

        let row = set_amazon_asin(&conn, a, Some("  B07PXGQC1Q ")).unwrap();
        assert_eq!(row.amazon_asin.as_deref(), Some("B07PXGQC1Q"));

        // One id names one edition, and one cover.
        assert!(set_amazon_asin(&conn, b, Some("B07PXGQC1Q")).is_err());

        // A blank clears.
        assert_eq!(
            set_amazon_asin(&conn, a, Some(" ")).unwrap().amazon_asin,
            None
        );
        assert_eq!(set_amazon_asin(&conn, a, None).unwrap().amazon_asin, None);
    }

    #[test]
    fn a_full_patch_is_canonicalized() {
        let mut p = patch();
        normalize(&mut p, &[]).unwrap();
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
        normalize(&mut p, &[]).unwrap();
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
        assert!(normalize(&mut p, &[]).is_err());
    }

    #[test]
    fn a_negative_series_index_is_refused() {
        let mut p = patch();
        p.series_name = Some("Saga".into());
        p.series_index = Some(-1.0);
        assert!(normalize(&mut p, &[]).is_err());
    }

    #[test]
    fn tags_keep_the_order_they_were_first_written_in() {
        let got = canonicalize_tags(vec!["bbb".into(), "aaa".into(), "BBB".into()]);
        assert_eq!(got, vec!["bbb", "aaa"]);
    }

    #[test]
    fn cjk_tags_pass_through_unchanged() {
        // CJK has no case: trimmed, the duplicate collapses.
        let got = canonicalize_tags(vec![" 小説 ".into(), "ライトノベル".into(), "小説".into()]);
        assert_eq!(got, vec!["小説", "ライトノベル"]);
        // A mixed tag lowercases only the half that has a case.
        let mixed = canonicalize_tags(vec!["ライトSciFi".into(), "ライトscifi".into()]);
        assert_eq!(mixed, vec!["ライトscifi"]);
    }

    fn stated_as(title: &str, authors: &[&str], language: &str) -> bokai::Metadata {
        bokai::Metadata {
            title: title.into(),
            creators: authors
                .iter()
                .map(|a| bokai::Contributor::named(*a))
                .collect(),
            language: language.into(),
            ..Default::default()
        }
    }

    fn wanted(title: &str, authors: &[&str], language: &str) -> FileMetadata {
        FileMetadata {
            title: title.into(),
            creators: authors.iter().map(|a| Credit::named(*a)).collect(),
            language: language.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_file_agrees_with_what_the_library_stores_from_it() {
        let mut file = stated_as("T", &["Jane Doe", "John Roe"], "en-US");
        file.publisher = Some(" Acme ".into());
        file.date = Some("2009-05-19T00:00:00+00:00".into());
        let mut want = wanted("T", &["Jane Doe", "John Roe"], "en");
        want.publisher = Some("Acme".into());
        want.date = Some("2009-05-19".into());
        assert!(drift(FileFormat::Epub, &file, &want, Cleared::default()).is_empty());

        want.date = Some("2009".into());
        assert!(
            drift(FileFormat::Epub, &file, &want, Cleared::default()).is_empty(),
            "a year agrees with the day it contains"
        );
        let one_string = stated_as("T", &["Jane Doe & John Roe"], "en");
        assert_eq!(
            drift(FileFormat::Kfx, &one_string, &want, Cleared::default())
                .iter()
                .map(|d| d.field)
                .collect::<Vec<_>>(),
            ["publisher", "date"],
            "one string naming both authors reads as the library does"
        );
    }

    #[test]
    fn a_surname_first_author_drifts_from_the_flipped_row() {
        let file = stated_as("T", &["Doe, Jane"], "en");
        let want = wanted("T", &["Jane Doe"], "en");
        assert_eq!(
            drift(FileFormat::Kfx, &file, &want, Cleared::default()),
            [Drift {
                field: "author",
                file: "Doe, Jane".into(),
                row: "Jane Doe".into()
            }]
        );
    }

    #[test]
    fn a_file_keeps_a_field_the_row_lacks_unless_an_edit_cleared_it() {
        let mut file = stated_as("T", &["A"], "en");
        file.publisher = Some("Acme".into());
        file.date = Some("2001".into());
        let want = wanted("T", &[], "");
        assert!(drift(FileFormat::Epub, &file, &want, Cleared::default()).is_empty());
        let cleared = Cleared {
            creators: true,
            publisher: true,
            date: true,
        };
        let fields: Vec<_> = drift(FileFormat::Epub, &file, &want, cleared)
            .into_iter()
            .map(|d| (d.field, d.row))
            .collect();
        assert_eq!(
            fields,
            [
                ("author", String::new()),
                ("publisher", String::new()),
                ("date", String::new())
            ]
        );
    }

    #[test]
    fn a_pdf_states_its_title_and_authors_alone() {
        let file = stated_as("T", &["A"], "");
        let mut want = wanted("T", &["A"], "en");
        want.publisher = Some("Acme".into());
        assert!(drift(FileFormat::Pdf, &file, &want, Cleared::default()).is_empty());
    }

    #[test]
    fn only_an_epub_drifts_on_a_role_and_only_on_one_the_row_claims() {
        let file = stated_as("T", &["Ann Writer", "Ivy Drawer"], "en");
        let mut want = wanted("T", &["Ann Writer", "Ivy Drawer"], "en");
        assert!(
            drift(FileFormat::Epub, &file, &want, Cleared::default()).is_empty(),
            "a credit stating no role claims none"
        );
        want.creators[0].roles = vec!["aut".into()];
        assert!(
            drift(FileFormat::Epub, &file, &want, Cleared::default()).is_empty(),
            "a creator stating no role reads as an author"
        );
        want.creators[1].roles = vec!["ill".into()];
        assert_eq!(
            drift(FileFormat::Epub, &file, &want, Cleared::default()),
            [Drift {
                field: "role",
                file: "Ann Writer; Ivy Drawer".into(),
                row: "Ann Writer (aut); Ivy Drawer (ill)".into(),
            }]
        );
        for format in [FileFormat::Kfx, FileFormat::Pdf] {
            assert!(
                drift(format, &file, &want, Cleared::default()).is_empty(),
                "{format:?} states no roles"
            );
        }
        let packed = stated_as("T", &["Ann Writer & Ivy Drawer"], "en");
        assert_eq!(
            drift(FileFormat::Epub, &packed, &want, Cleared::default())
                .iter()
                .map(|d| d.field)
                .collect::<Vec<_>>(),
            ["role"],
            "a role the packing creator does not state takes the row's credits into the file"
        );

        let mut packed_editors = stated_as("T", &["春野一郎、秋山次郎"], "ja");
        packed_editors.creators[0].roles = vec!["edt".into()];
        let mut as_read = wanted("T", &["春野一郎", "秋山次郎"], "ja");
        for c in &mut as_read.creators {
            c.roles = vec!["edt".into()];
        }
        assert!(
            drift(
                FileFormat::Epub,
                &packed_editors,
                &as_read,
                Cleared::default()
            )
            .is_empty(),
            "credits read out of one packing creator agree with it"
        );
    }

    fn epub(creator: &str) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let mut put = |name: &str, body: String, opts: SimpleFileOptions| {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        put("mimetype", "application/epub+zip".into(), stored);
        put(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.into(),
            SimpleFileOptions::default(),
        );
        put(
            "content.opf",
            format!(
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:opf="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:sprocket</dc:identifier>
    <dc:title>Sprocket Tales</dc:title>
    {creator}
    <dc:language>en-US</dc:language>
    <dc:publisher>Acme</dc:publisher>
  </metadata>
  <manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="c"/></spine>
</package>"#
            ),
            SimpleFileOptions::default(),
        );
        put(
            "c.xhtml",
            r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c</title></head><body><p>Text.</p></body></html>"#.into(),
            SimpleFileOptions::default(),
        );
        zip.finish().unwrap().into_inner()
    }

    fn metadata_of(path: &str, format: bokai::Format) -> bokai::Metadata {
        let bytes = std::fs::read(path).unwrap();
        bokai::Book::from_bytes(&bytes, format)
            .unwrap()
            .metadata()
            .clone()
    }

    /// An imported EPUB and the KFX converted from it, in a fresh library.
    fn library_with_a_converted_epub(
        creator: &str,
    ) -> (tempfile::TempDir, Connection, LibraryPaths, i64) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();
        let crate::library::import::ImportOutcome::Imported { book, .. } =
            crate::library::import::import_bytes(&conn, &paths, epub(creator), "sprocket.epub")
                .unwrap()
        else {
            panic!("a fresh library has no duplicate");
        };
        crate::library::convert::convert_book(
            &conn,
            &paths,
            book.id,
            crate::library::convert::Mode::Reconvert,
            &|_, _, _, _| {},
        )
        .unwrap();
        (tmp, conn, paths, book.id)
    }

    #[test]
    fn an_import_stores_the_flipped_author_in_the_file() {
        let (_tmp, conn, _paths, id) = library_with_a_converted_epub(
            r#"<dc:creator opf:role="aut" opf:file-as="Doe, Jane">Doe, Jane</dc:creator>"#,
        );
        let book = db::get_book(&conn, id).unwrap().unwrap();
        assert_eq!(book.author, "Jane Doe");
        let source = metadata_of(book.epub_path.as_deref().unwrap(), bokai::Format::Epub);
        assert_eq!(source.creator_names(), ["Jane Doe"]);
        assert_eq!(
            source.creator_sort_keys(),
            ["Doe, Jane"],
            "the sort key stays"
        );
        assert_eq!(source.language, "en-US", "an agreeing tag is left alone");
        let kfx = metadata_of(book.kfx_path.as_deref().unwrap(), bokai::Format::Kfx);
        assert_eq!(kfx.creator_names(), ["Jane Doe"]);
    }

    #[test]
    fn an_edit_reaches_every_file_of_the_book_and_a_second_pass_writes_nothing() {
        let (_tmp, conn, paths, id) =
            library_with_a_converted_epub("<dc:creator>Jane Doe</dc:creator>");
        let mut patch = patch_from(&db::get_book(&conn, id).unwrap().unwrap());
        patch.title = "Sprocket Tales, Revised".into();
        patch.author = "Ann Writer & Bob Maker".into();
        patch.publisher = None;
        let edited = edit_row(&conn, &paths, id, patch).unwrap();
        assert!(edited.cleared.publisher);

        let reports = edited.bake();
        assert_eq!(reports.len(), 2, "the EPUB and the KFX");
        assert!(
            reports.iter().all(|r| r.written && r.error.is_none()),
            "{reports:?}"
        );
        for (path, format) in [
            (
                edited.row.epub_path.as_deref().unwrap(),
                bokai::Format::Epub,
            ),
            (edited.row.kfx_path.as_deref().unwrap(), bokai::Format::Kfx),
        ] {
            let m = metadata_of(path, format);
            assert_eq!(m.title, "Sprocket Tales, Revised");
            assert_eq!(m.creator_names(), ["Ann Writer", "Bob Maker"]);
            assert_eq!(m.publisher, None, "the cleared publisher left the file");
        }

        let before: Vec<Vec<u8>> = files_of(&edited.row)
            .iter()
            .map(|(_, p)| std::fs::read(p).unwrap())
            .collect();
        let again = bake(
            &edited.row,
            &FileMetadata::of(&edited.row),
            Cleared::default(),
            true,
        );
        assert!(
            again.iter().all(|r| r.drift.is_empty() && !r.written),
            "{again:?}"
        );
        let after: Vec<Vec<u8>> = files_of(&edited.row)
            .iter()
            .map(|(_, p)| std::fs::read(p).unwrap())
            .collect();
        assert_eq!(before, after, "an agreeing file is not rewritten");
    }

    #[test]
    fn an_edit_of_library_fields_alone_leaves_the_files_unread() {
        let (_tmp, conn, paths, id) =
            library_with_a_converted_epub("<dc:creator>Jane Doe</dc:creator>");
        let mut patch = patch_from(&db::get_book(&conn, id).unwrap().unwrap());
        patch.tags = vec!["shelf".into()];
        let single = edit_row(&conn, &paths, id, patch).unwrap();
        assert!(!single.reaches_files);
        assert!(single.bake().is_empty());

        let mut patch = patch_from(&single.row);
        patch.tags = vec!["shelf".into(), "again".into()];
        patch.credits = Some(vec![Credit::with_roles("Jane Doe", &["aut"])]);
        let stated = edit_row(&conn, &paths, id, patch).unwrap();
        assert!(
            !stated.reaches_files,
            "stating `aut` for an author that stated no role changes no file"
        );

        let edited = edit_rows(
            &conn,
            &paths,
            &[id],
            BulkMetadataPatch {
                series_name: Some("Saga".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!edited[0].reaches_files);
        assert!(edited[0].bake().is_empty());
    }

    const TWO_CREATORS: &str = r#"<dc:creator opf:role="aut">Ann Writer</dc:creator>
    <dc:creator>Ivy Drawer</dc:creator>"#;

    #[test]
    fn an_import_reads_each_creator_s_roles() {
        let (_tmp, conn, _paths, id) = library_with_a_converted_epub(
            r#"<dc:creator opf:role="aut">Ann Writer</dc:creator>
    <dc:creator opf:role="ill">Ivy Drawer</dc:creator>"#,
        );
        let book = db::get_book(&conn, id).unwrap().unwrap();
        assert_eq!(
            book.credits,
            [
                Credit::with_roles("Ann Writer", &["aut"]),
                Credit::with_roles("Ivy Drawer", &["ill"]),
            ]
        );
        assert_eq!(
            book.author, "Ann Writer",
            "the author line names the author alone"
        );
        let kfx = metadata_of(book.kfx_path.as_deref().unwrap(), bokai::Format::Kfx);
        assert_eq!(
            kfx.creator_names(),
            ["Ann Writer", "Ivy Drawer"],
            "a KFX states no roles, so it names every creator"
        );
    }

    #[test]
    fn a_role_edit_reaches_the_epub_and_leaves_the_kfx_alone() {
        let (_tmp, conn, paths, id) = library_with_a_converted_epub(TWO_CREATORS);
        let before = db::get_book(&conn, id).unwrap().unwrap();
        assert_eq!(before.author, "Ann Writer & Ivy Drawer");
        let kfx_before = std::fs::read(before.kfx_path.as_deref().unwrap()).unwrap();

        let mut patch = patch_from(&before);
        patch.credits = Some(vec![
            Credit::with_roles("Ann Writer", &["aut"]),
            Credit::with_roles("Ivy Drawer", &["ill"]),
        ]);
        let edited = edit_row(&conn, &paths, id, patch).unwrap();
        assert!(edited.reaches_files);
        assert_eq!(edited.row.author, "Ann Writer");
        let reports = edited.bake();
        let written: Vec<FileFormat> = reports
            .iter()
            .filter(|r| r.written)
            .map(|r| r.format)
            .collect();
        assert_eq!(written, [FileFormat::Epub], "{reports:?}");

        let epub = metadata_of(
            edited.row.epub_path.as_deref().unwrap(),
            bokai::Format::Epub,
        );
        let roles: Vec<(String, Vec<String>)> = epub
            .creators
            .into_iter()
            .map(|c| (c.name, c.roles))
            .collect();
        assert_eq!(
            roles,
            [
                ("Ann Writer".to_string(), vec!["aut".to_string()]),
                ("Ivy Drawer".to_string(), vec!["ill".to_string()]),
            ]
        );
        assert_eq!(
            std::fs::read(edited.row.kfx_path.as_deref().unwrap()).unwrap(),
            kfx_before,
            "the KFX names the same creators and is left byte for byte"
        );
        assert!(
            bake(
                &edited.row,
                &FileMetadata::of(&edited.row),
                Cleared::default(),
                true
            )
            .iter()
            .all(|r| r.drift.is_empty() && !r.written),
            "a second pass finds nothing to write"
        );
    }

    #[test]
    fn an_author_line_edit_keeps_every_other_credit() {
        let (_tmp, conn, paths, id) = library_with_a_converted_epub(TWO_CREATORS);
        let mut patch = patch_from(&db::get_book(&conn, id).unwrap().unwrap());
        patch.credits = Some(vec![
            Credit::named("Ann Writer"),
            Credit::with_roles("Ivy Drawer", &["ill"]),
        ]);
        edit_row(&conn, &paths, id, patch).unwrap().bake();

        let mut patch = patch_from(&db::get_book(&conn, id).unwrap().unwrap());
        patch.author = "Ann Writer & Bo Cowriter".into();
        let row = edit_row(&conn, &paths, id, patch).unwrap().row;
        assert_eq!(
            row.credits,
            [
                Credit::named("Ann Writer"),
                Credit::named("Bo Cowriter"),
                Credit::with_roles("Ivy Drawer", &["ill"]),
            ]
        );
        assert_eq!(row.author, "Ann Writer & Bo Cowriter");
    }

    #[test]
    fn an_unchanged_author_line_never_splits_a_credit() {
        let (_tmp, conn, paths, id) =
            library_with_a_converted_epub("<dc:creator>Smith &amp; Jones</dc:creator>");
        let before = db::get_book(&conn, id).unwrap().unwrap();
        assert_eq!(before.credits, [Credit::named("Smith & Jones")]);
        let mut patch = patch_from(&before);
        patch.title = "Retitled".into();
        let edited = edit_row(&conn, &paths, id, patch).unwrap();
        assert_eq!(edited.row.credits, before.credits);
        let reports = edited.bake();
        assert!(reports.iter().all(|r| r.error.is_none()), "{reports:?}");
        let epub = metadata_of(
            edited.row.epub_path.as_deref().unwrap(),
            bokai::Format::Epub,
        );
        assert_eq!(epub.creator_names(), ["Smith & Jones"]);
    }

    #[test]
    fn a_bulk_assignment_gives_a_role_and_renders_the_new_author_line() {
        let (_tmp, conn, paths, id) = library_with_a_converted_epub(TWO_CREATORS);
        let edited = edit_rows(
            &conn,
            &paths,
            &[id],
            BulkMetadataPatch {
                credits: vec![Credit::with_roles("Ivy Drawer", &["ILL"])],
                ..Default::default()
            },
        )
        .unwrap();
        let row = &edited[0].row;
        assert_eq!(row.author, "Ann Writer");
        assert_eq!(row.credits[1], Credit::with_roles("Ivy Drawer", &["ill"]));
        assert_eq!(
            row.author_romaji,
            romaji::romanize_field("Ann Writer", None, "en")
        );
        assert!(edited[0].reaches_files);
        let reports = edited[0].bake();
        assert!(
            reports
                .iter()
                .any(|r| r.format == FileFormat::Epub && r.written),
            "{reports:?}"
        );
    }

    #[test]
    fn a_row_from_before_roles_takes_them_from_its_epub() {
        let (_tmp, conn, paths, id) = library_with_a_converted_epub(
            r#"<dc:creator opf:role="ill">Ivy Drawer</dc:creator>
    <dc:creator opf:role="aut">Ann Writer</dc:creator>"#,
        );
        db::set_credits(
            &conn,
            id,
            &[Credit::named("Ivy Drawer"), Credit::named("Ann Writer")],
        )
        .unwrap();
        let before = rename::rename_book_files(&conn, &paths, id)
            .unwrap()
            .unwrap();
        assert_eq!(before.author, "Ivy Drawer & Ann Writer");
        assert!(
            before
                .epub_path
                .as_deref()
                .unwrap()
                .contains("[Ivy Drawer]")
        );

        let from_file = credits_from_epub(&before)
            .unwrap()
            .expect("the file says more");
        assert_eq!(from_file[0], Credit::with_roles("Ivy Drawer", &["ill"]));
        let after = adopt_credits(&conn, &paths, id, &from_file).unwrap();
        assert_eq!(after.author, "Ann Writer");
        assert!(
            after.epub_path.as_deref().unwrap().contains("[Ann Writer]"),
            "the files are named for the first author: {:?}",
            after.epub_path
        );
        assert_eq!(
            credits_from_epub(&after).unwrap(),
            None,
            "nothing left to adopt"
        );
        assert!(
            bake(&after, &FileMetadata::of(&after), Cleared::default(), false)
                .iter()
                .all(|r| r.drift.is_empty()),
            "the row now agrees with its files"
        );

        db::set_credits(&conn, id, &[Credit::named("Someone Else")]).unwrap();
        let other = db::get_book(&conn, id).unwrap().unwrap();
        assert_eq!(
            credits_from_epub(&other).unwrap(),
            None,
            "a file naming other people is not the row's to read from"
        );
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
