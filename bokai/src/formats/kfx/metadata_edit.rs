//! Surgical in-place metadata edit for a KFX container.
//!
//! Patches the book's metadata fragments — Amazon's categorised `book_metadata`
//! ($490) wrapper and/or the flat `metadata` ($258) fragment — setting
//! title / authors / language / publisher / issue_date / ASIN without
//! re-encoding the book. This is the KFX side of "write metadata into the
//! source", replacing the reconvert-to-bake path: the same fields
//! `library_update_metadata` records in the DB row now reach the KFX artifact
//! directly, through the container edit harness ([`edit_container`]).
//!
//! Both shapes are patched when present, so the value a consumer reads is the
//! same whichever it consults — the loader prefers $490 and falls back to $258
//! (see the port loader's `extract_book_metadata`). Every value written is an inline Ion
//! string, so no doc-symbol-table growth is required; and only the metadata
//! entities are rebuilt — the harness passes everything else through verbatim.
//!
//! Scope (v1): edits the existing `kindle_title_metadata` category in $490 and
//! the flat $258 fields. It does not *create* a metadata fragment or category
//! from scratch — a container with neither shape is rejected. Real Amazon/bokai
//! KFX always carry `book_metadata` with a `kindle_title_metadata` category.

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::container_edit::{EntityEdit, edit_container};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::ConvertError;
use crate::kfx_to_epub::loader::{self, SymbolTable};

/// The `kindle_title_metadata` key names used in Amazon's $490 shape.
const KEY_TITLE: &str = "title";
const KEY_AUTHOR: &str = "author";
const KEY_AUTHOR_PRON: &str = "author_pronunciation";
const KEY_LANGUAGE: &str = "language";
const KEY_PUBLISHER: &str = "publisher";
const KEY_ISSUE_DATE: &str = "issue_date";
const KEY_ASIN: &str = "ASIN";

/// Which metadata fields to set. `None` leaves a field untouched; `Some(_)`
/// overwrites it in every metadata shape the container carries.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// Replaces the full ordered author list. `Some(vec![])` clears authors.
    /// Setting authors also drops any stale `author_pronunciation` entries,
    /// since a pronunciation can't be trusted once the author changes.
    pub authors: Option<Vec<String>>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    /// Publication date (KFX stores `YYYY-MM-DD`). Carried in the $490 shape.
    pub issue_date: Option<String>,
    pub asin: Option<String>,
}

impl MetadataPatch {
    /// True if the patch sets no field — [`edit_metadata`] then returns the
    /// input unchanged.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.authors.is_none()
            && self.language.is_none()
            && self.publisher.is_none()
            && self.issue_date.is_none()
            && self.asin.is_none()
    }

    /// The single-valued ($490) key/value pairs this patch sets, in a stable
    /// order. Authors are multi-valued and handled separately.
    fn single_kv(&self) -> Vec<(&'static str, &str)> {
        let mut v = Vec::new();
        if let Some(t) = &self.title {
            v.push((KEY_TITLE, t.as_str()));
        }
        if let Some(l) = &self.language {
            v.push((KEY_LANGUAGE, l.as_str()));
        }
        if let Some(p) = &self.publisher {
            v.push((KEY_PUBLISHER, p.as_str()));
        }
        if let Some(d) = &self.issue_date {
            v.push((KEY_ISSUE_DATE, d.as_str()));
        }
        if let Some(a) = &self.asin {
            v.push((KEY_ASIN, a.as_str()));
        }
        v
    }
}

/// Apply `patch` to `kfx_bytes`, returning the rewritten container. Edits the
/// `book_metadata` ($490) and/or `metadata` ($258) fragments in place and passes
/// every other entity through byte-for-byte.
///
/// Returns the input unchanged when `patch` is empty. Errors (via
/// [`ConvertError::InvalidKfx`]) if the bytes aren't a KFX container, or if the
/// container carries neither metadata shape to edit.
pub fn edit_metadata(kfx_bytes: &[u8], patch: &MetadataPatch) -> Result<Vec<u8>, ConvertError> {
    if patch.is_empty() {
        return Ok(kfx_bytes.to_vec());
    }

    // Load once for the symbol table (category names may be symbols) and to
    // confirm there is an editable metadata fragment.
    let book = loader::load(kfx_bytes)?;
    let has_490 = book.by_type.contains_key(&(KfxSymbol::BookMetadata as u64));
    let has_258 = book.by_type.contains_key(&(KfxSymbol::Metadata as u64));
    if !has_490 && !has_258 {
        return Err(ConvertError::InvalidKfx(
            "KFX has no metadata fragment to edit".into(),
        ));
    }

    edit_container(kfx_bytes, |e| {
        if e.is_type(KfxSymbol::BookMetadata) {
            Ok(EntityEdit::Ion(patch_book_metadata(
                &e.parse_ion()?,
                patch,
                &book.symbols,
            )))
        } else if e.is_type(KfxSymbol::Metadata) {
            Ok(EntityEdit::Ion(patch_flat_metadata(&e.parse_ion()?, patch)))
        } else {
            Ok(EntityEdit::Keep)
        }
    })
}

// --- $490 categorised `book_metadata` ---------------------------------------

/// Rewrite `book_metadata` ($490), patching the `kindle_title_metadata`
/// category's key/value list. Preserves the annotation wrapper and field order;
/// every category other than `kindle_title_metadata` is left untouched.
fn patch_book_metadata(
    parsed: &IonValue,
    patch: &MetadataPatch,
    symbols: &SymbolTable,
) -> IonValue {
    if let IonValue::Annotated(anns, inner) = parsed {
        return IonValue::Annotated(
            anns.clone(),
            Box::new(patch_book_metadata(inner, patch, symbols)),
        );
    }
    let Some(fields) = parsed.as_struct() else {
        return parsed.clone();
    };
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len());
    for (k, v) in fields {
        if *k == KfxSymbol::CategorisedMetadata as u64
            && let IonValue::List(cats) = v
        {
            let new_cats = cats
                .iter()
                .map(|cat| patch_category(cat, patch, symbols))
                .collect();
            out.push((*k, IonValue::List(new_cats)));
        } else {
            out.push((*k, v.clone()));
        }
    }
    IonValue::Struct(out)
}

/// If `cat` is the `kindle_title_metadata` category, patch its metadata ($258)
/// key/value list; otherwise return it untouched.
fn patch_category(cat: &IonValue, patch: &MetadataPatch, symbols: &SymbolTable) -> IonValue {
    let Some(fields) = cat.unwrap_annotated().as_struct() else {
        return cat.clone();
    };
    let is_title = get_field(fields, KfxSymbol::Category as u64).and_then(|c| symbols.text_of(c))
        == Some("kindle_title_metadata");
    if !is_title {
        return cat.clone();
    }
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len());
    for (k, v) in fields {
        if *k == KfxSymbol::Metadata as u64
            && let IonValue::List(items) = v
        {
            out.push((*k, IonValue::List(patch_title_items(items, patch))));
        } else {
            out.push((*k, v.clone()));
        }
    }
    match cat {
        IonValue::Annotated(anns, _) => {
            IonValue::Annotated(anns.clone(), Box::new(IonValue::Struct(out)))
        }
        _ => IonValue::Struct(out),
    }
}

/// Patch the `kindle_title_metadata` key/value item list: replace single-valued
/// keys in place (append if absent), and replace the whole run of `author` (and
/// stale `author_pronunciation`) items with the new author list, keeping their
/// original position.
fn patch_title_items(items: &[IonValue], patch: &MetadataPatch) -> Vec<IonValue> {
    let singles = patch.single_kv();
    let mut single_done = vec![false; singles.len()];
    let mut author_anchor: Option<usize> = None;

    let mut out: Vec<IonValue> = Vec::with_capacity(items.len() + singles.len());
    for item in items {
        let key = item_key(item);
        // Drop existing author / author_pronunciation items when replacing authors.
        if patch.authors.is_some() && matches!(key, Some(KEY_AUTHOR) | Some(KEY_AUTHOR_PRON)) {
            author_anchor.get_or_insert(out.len());
            continue;
        }
        // Replace a single-valued key in place.
        if let Some(pos) = singles.iter().position(|(k, _)| key == Some(*k)) {
            out.push(kv_item(singles[pos].0, singles[pos].1));
            single_done[pos] = true;
            continue;
        }
        out.push(item.clone());
    }

    // Append single-valued keys that weren't already present.
    for (i, (k, val)) in singles.iter().enumerate() {
        if !single_done[i] {
            out.push(kv_item(k, val));
        }
    }

    // Splice the new author run in where the old authors were (or append).
    if let Some(authors) = &patch.authors {
        let at = author_anchor.unwrap_or(out.len());
        let new_authors: Vec<IonValue> = authors.iter().map(|a| kv_item(KEY_AUTHOR, a)).collect();
        out.splice(at..at, new_authors);
    }

    out
}

/// The `key` string of a `{key, value}` item, if present.
fn item_key(item: &IonValue) -> Option<&str> {
    item.as_struct()
        .and_then(|f| get_field(f, KfxSymbol::Key as u64))
        .and_then(IonValue::as_string)
}

/// A fresh `{key: <key>, value: <value>}` metadata item (both inline strings).
fn kv_item(key: &str, value: &str) -> IonValue {
    IonValue::Struct(vec![
        (KfxSymbol::Key as u64, IonValue::String(key.into())),
        (KfxSymbol::Value as u64, IonValue::String(value.into())),
    ])
}

// --- $258 flat `metadata` ---------------------------------------------------

/// Rewrite the flat `metadata` ($258) fragment, setting the patched fields by
/// symbol id. Preserves the annotation wrapper and every other field (this
/// fragment can also carry `reading_orders`, `cover_image`, etc.).
fn patch_flat_metadata(parsed: &IonValue, patch: &MetadataPatch) -> IonValue {
    if let IonValue::Annotated(anns, inner) = parsed {
        return IonValue::Annotated(anns.clone(), Box::new(patch_flat_metadata(inner, patch)));
    }
    let Some(fields) = parsed.as_struct() else {
        return parsed.clone();
    };

    // (symbol id, new value) pairs to set.
    let mut sets: Vec<(u64, IonValue)> = Vec::new();
    let mut str_field = |sym: KfxSymbol, v: &Option<String>| {
        if let Some(s) = v {
            sets.push((sym as u64, IonValue::String(s.clone())));
        }
    };
    str_field(KfxSymbol::Title, &patch.title);
    str_field(KfxSymbol::Language, &patch.language);
    str_field(KfxSymbol::Publisher, &patch.publisher);
    str_field(KfxSymbol::IssueDate, &patch.issue_date);
    str_field(KfxSymbol::Asin, &patch.asin);
    if let Some(authors) = &patch.authors {
        sets.push((KfxSymbol::Author as u64, authors_ion(authors)));
    }

    let mut done = vec![false; sets.len()];
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len() + sets.len());
    for (k, v) in fields {
        if let Some(pos) = sets.iter().position(|(sk, _)| sk == k) {
            out.push((*k, sets[pos].1.clone()));
            done[pos] = true;
        } else {
            out.push((*k, v.clone()));
        }
    }
    for (i, (k, val)) in sets.iter().enumerate() {
        if !done[i] {
            out.push((*k, val.clone()));
        }
    }
    IonValue::Struct(out)
}

/// Encode an author list for the flat `$222` field: a bare string for one
/// author, a list for several — both shapes the loader reads.
fn authors_ion(authors: &[String]) -> IonValue {
    match authors {
        [one] => IonValue::String(one.clone()),
        many => IonValue::List(many.iter().map(|a| IonValue::String(a.clone())).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    /// End-to-end: patch title + authors on the real fixture, then prove the
    /// rewritten container re-loads with the new values, leaves other metadata
    /// and the cover intact, and still converts to EPUB.
    #[test]
    fn edit_title_and_authors_on_fixture() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let before = loader::load(&kfx).expect("load original");
        assert!(!before.metadata.title.is_empty(), "fixture has a title");

        let patch = MetadataPatch {
            title: Some("新しいタイトル".into()),
            authors: Some(vec!["著者 一".into(), "著者 二".into()]),
            ..Default::default()
        };
        let out = edit_metadata(&kfx, &patch).expect("edit_metadata");
        let after = loader::load(&out).expect("rewritten container must re-load");

        assert_eq!(after.metadata.title, "新しいタイトル");
        assert_eq!(after.metadata.authors, vec!["著者 一", "著者 二"]);
        // Untouched fields survive.
        assert_eq!(
            before.metadata.language, after.metadata.language,
            "language untouched"
        );
        assert_eq!(
            before.metadata.cover_resource_name, after.metadata.cover_resource_name,
            "cover untouched by a metadata edit"
        );
        assert_eq!(
            before.raw_media.len(),
            after.raw_media.len(),
            "no resources touched"
        );
        assert!(
            crate::kfx_to_epub::convert_to_epub(&out).is_ok(),
            "patched KFX must still convert to EPUB"
        );
    }

    /// A single-field edit changes only that field.
    #[test]
    fn edit_single_field_leaves_rest() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let before = loader::load(&kfx).expect("load original");

        let patch = MetadataPatch {
            publisher: Some("新潮社".into()),
            ..Default::default()
        };
        let after = loader::load(&edit_metadata(&kfx, &patch).unwrap()).unwrap();

        assert_eq!(after.metadata.publisher.as_deref(), Some("新潮社"));
        assert_eq!(
            before.metadata.title, after.metadata.title,
            "title untouched"
        );
        assert_eq!(
            before.metadata.authors, after.metadata.authors,
            "authors untouched"
        );
    }

    #[test]
    fn empty_patch_returns_input_unchanged() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let out = edit_metadata(&kfx, &MetadataPatch::default()).unwrap();
        assert_eq!(out, kfx, "an empty patch is a no-op");
    }

    #[test]
    fn non_kfx_bytes_error() {
        let patch = MetadataPatch {
            title: Some("x".into()),
            ..Default::default()
        };
        assert!(edit_metadata(b"not a kfx container", &patch).is_err());
    }

    #[test]
    fn patch_title_items_replaces_and_appends() {
        // Existing: [{title: "Old"}, {author: "A"}, {language: "en"}]
        let items = vec![
            kv_item(KEY_TITLE, "Old"),
            kv_item(KEY_AUTHOR, "A"),
            kv_item(KEY_LANGUAGE, "en"),
        ];
        let patch = MetadataPatch {
            title: Some("New".into()),
            authors: Some(vec!["X".into(), "Y".into()]),
            publisher: Some("P".into()), // absent → appended
            ..Default::default()
        };
        let out = patch_title_items(&items, &patch);

        let kv: Vec<(&str, &str)> = out
            .iter()
            .filter_map(|it| {
                let f = it.as_struct()?;
                Some((
                    get_field(f, KfxSymbol::Key as u64)?.as_string()?,
                    get_field(f, KfxSymbol::Value as u64)?.as_string()?,
                ))
            })
            .collect();

        // Title replaced in place, authors X/Y spliced where "A" was, language
        // preserved, publisher appended at the end.
        assert_eq!(
            kv,
            vec![
                ("title", "New"),
                ("author", "X"),
                ("author", "Y"),
                ("language", "en"),
                ("publisher", "P"),
            ]
        );
    }

    #[test]
    fn patch_title_items_drops_stale_pronunciation() {
        let items = vec![
            kv_item(KEY_AUTHOR, "A"),
            kv_item(KEY_AUTHOR_PRON, "えー"),
            kv_item(KEY_TITLE, "T"),
        ];
        let patch = MetadataPatch {
            authors: Some(vec!["B".into()]),
            ..Default::default()
        };
        let out = patch_title_items(&items, &patch);
        let keys: Vec<&str> = out.iter().filter_map(item_key).collect();
        // author replaced by "B", pronunciation dropped, title kept.
        assert_eq!(keys, vec!["author", "title"]);
        assert_eq!(item_key(&out[0]), Some("author"));
        assert_eq!(
            out[0].as_struct().unwrap()[1].1.as_string(),
            Some("B"),
            "new author value"
        );
    }

    #[test]
    fn authors_ion_shapes() {
        assert!(matches!(authors_ion(&["Solo".into()]), IonValue::String(s) if s == "Solo"));
        match authors_ion(&["A".into(), "B".into()]) {
            IonValue::List(items) => assert_eq!(items.len(), 2),
            other => panic!("expected list, got {other:?}"),
        }
    }
}
