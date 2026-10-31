//! Surgical in-place metadata edit for a KFX container.

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::container_edit::{EntityEdit, edit_container};
use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::{self, SymbolTable};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::model::sort_key_survives;
use crate::util::truncate_to_date;

/// The `kindle_title_metadata` key names of the $490 shape.
const KEY_TITLE: &str = "title";
const KEY_AUTHOR: &str = "author";
const KEY_AUTHOR_PRON: &str = "author_pronunciation";
const KEY_LANGUAGE: &str = "language";
const KEY_PUBLISHER: &str = "publisher";
const KEY_ISSUE_DATE: &str = "issue_date";
const KEY_ASIN: &str = "ASIN";
const KEY_CONTENT_ID: &str = "content_id";

/// Which metadata fields to set. `None` leaves a field untouched; `Some(_)`
/// overwrites it in every metadata shape the container carries, and an empty
/// value removes it.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// Replaces the full ordered author list; `Some(vec![])` clears it.
    pub authors: Option<Vec<String>>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    /// Publication date, stored as its `YYYY-MM-DD` date part.
    pub issue_date: Option<String>,
    pub asin: Option<String>,
    /// The device-internal key (`.sdr` directory, catalog entry).
    pub content_id: Option<String>,
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
            && self.content_id.is_none()
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
        if let Some(c) = &self.content_id {
            v.push((KEY_CONTENT_ID, c.as_str()));
        }
        v
    }
}

/// Apply `patch` to the `book_metadata` ($490) and `metadata` ($258) fragments,
/// passing every other entity through byte for byte; beside a $490, $258 has only
/// the fields it carries replaced. Returns `kfx_bytes` when neither fragment changes.
pub fn edit_metadata(kfx_bytes: &[u8], patch: &MetadataPatch) -> Result<Vec<u8>, KfxError> {
    if patch.is_empty() {
        return Ok(kfx_bytes.to_vec());
    }
    let patch = MetadataPatch {
        issue_date: patch.issue_date.as_deref().map(truncate_to_date),
        ..patch.clone()
    };

    // `book.symbols` resolves category names; a container with neither fragment errors.
    let book = loader::load(kfx_bytes)?;
    let has_490 = book.by_type.contains_key(&(KfxSymbol::BookMetadata as u64));
    let has_258 = book.by_type.contains_key(&(KfxSymbol::Metadata as u64));
    if !has_490 && !has_258 {
        return Err(KfxError::InvalidKfx(
            "KFX has no metadata fragment to edit".into(),
        ));
    }

    let mut changed = false;
    let out = edit_container(kfx_bytes, |e| {
        let parsed = if e.is_type(KfxSymbol::BookMetadata) || e.is_type(KfxSymbol::Metadata) {
            e.parse_ion()?
        } else {
            return Ok(EntityEdit::Keep);
        };
        let patched = if e.is_type(KfxSymbol::BookMetadata) {
            patch_book_metadata(&parsed, &patch, &book.symbols)
        } else {
            patch_flat_metadata(&parsed, &patch, !has_490)
        };
        if patched == parsed {
            return Ok(EntityEdit::Keep);
        }
        changed = true;
        Ok(EntityEdit::Ion(patched))
    })?;
    Ok(if changed { out } else { kfx_bytes.to_vec() })
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

/// `cat` with its `metadata` ($258) key/value list patched when it is the
/// `kindle_title_metadata` category, and as it was for any other.
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

/// Patch the `kindle_title_metadata` item list: each single-valued key is set in
/// place, appended when absent and dropped when empty. A changed author run takes
/// the old one's place; `author_pronunciation` stays if every name it keys survives.
fn patch_title_items(items: &[IonValue], patch: &MetadataPatch) -> Vec<IonValue> {
    let singles = patch.single_kv();
    let mut single_done = vec![false; singles.len()];
    let mut author_anchor: Option<usize> = None;

    let old_authors: Vec<&str> = items
        .iter()
        .filter(|i| item_key(i) == Some(KEY_AUTHOR))
        .filter_map(item_value)
        .collect();
    let pronunciations = items
        .iter()
        .filter(|i| item_key(i) == Some(KEY_AUTHOR_PRON))
        .count();
    let new_authors = patch.authors.as_deref().filter(|new| {
        !old_authors
            .iter()
            .copied()
            .eq(new.iter().map(String::as_str))
    });
    let keep_pronunciations = new_authors.is_none_or(|new| {
        (0..pronunciations).all(|i| {
            matches!((old_authors.get(i), new.get(i)), (Some(old), Some(new)) if sort_key_survives(old, new))
        })
    });

    let mut out: Vec<IonValue> = Vec::with_capacity(items.len() + singles.len());
    for item in items {
        let key = item_key(item);
        if new_authors.is_some() && key == Some(KEY_AUTHOR) {
            author_anchor.get_or_insert(out.len());
            continue;
        }
        if key == Some(KEY_AUTHOR_PRON) && !keep_pronunciations {
            continue;
        }
        if let Some(pos) = singles.iter().position(|(k, _)| key == Some(*k)) {
            single_done[pos] = true;
            let (k, v) = singles[pos];
            if v.is_empty() {
                continue;
            }
            out.push(if item_value(item) == Some(v) {
                item.clone()
            } else {
                kv_item(k, v)
            });
            continue;
        }
        out.push(item.clone());
    }

    for (i, (k, val)) in singles.iter().enumerate() {
        if !single_done[i] && !val.is_empty() {
            out.push(kv_item(k, val));
        }
    }

    if let Some(authors) = new_authors {
        let at = author_anchor.unwrap_or(out.len());
        out.splice(at..at, authors.iter().map(|a| kv_item(KEY_AUTHOR, a)));
    }

    out
}

/// The `key` string of a `{key, value}` item, if present.
fn item_key(item: &IonValue) -> Option<&str> {
    item.as_struct()
        .and_then(|f| get_field(f, KfxSymbol::Key as u64))
        .and_then(IonValue::as_string)
}

/// The `value` string of a `{key, value}` item, if it is one.
fn item_value(item: &IonValue) -> Option<&str> {
    item.as_struct()
        .and_then(|f| get_field(f, KfxSymbol::Value as u64))
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

/// Rewrite the flat `metadata` ($258) fragment by symbol id, keeping its other
/// fields and annotation wrapper: an empty value removes a field, and a field
/// `parsed` lacks is added only with `append`.
fn patch_flat_metadata(parsed: &IonValue, patch: &MetadataPatch, append: bool) -> IonValue {
    if let IonValue::Annotated(anns, inner) = parsed {
        return IonValue::Annotated(
            anns.clone(),
            Box::new(patch_flat_metadata(inner, patch, append)),
        );
    }
    let Some(fields) = parsed.as_struct() else {
        return parsed.clone();
    };

    // (symbol id, new value) pairs to set; `None` removes the field.
    let mut sets: Vec<(u64, Option<IonValue>)> = Vec::new();
    let mut str_field = |sym: KfxSymbol, v: &Option<String>| {
        if let Some(s) = v {
            sets.push((
                sym as u64,
                (!s.is_empty()).then(|| IonValue::String(s.clone())),
            ));
        }
    };
    str_field(KfxSymbol::Title, &patch.title);
    str_field(KfxSymbol::Language, &patch.language);
    str_field(KfxSymbol::Publisher, &patch.publisher);
    str_field(KfxSymbol::IssueDate, &patch.issue_date);
    str_field(KfxSymbol::Asin, &patch.asin);
    if let Some(authors) = &patch.authors {
        sets.push((
            KfxSymbol::Author as u64,
            (!authors.is_empty()).then(|| authors_ion(authors)),
        ));
    }

    let mut done = vec![false; sets.len()];
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len() + sets.len());
    for (k, v) in fields {
        match sets.iter().position(|(sk, _)| sk == k) {
            Some(pos) => {
                done[pos] = true;
                if let Some(new) = &sets[pos].1 {
                    out.push((*k, new.clone()));
                }
            }
            None => out.push((*k, v.clone())),
        }
    }
    if append {
        for (i, (k, val)) in sets.iter().enumerate() {
            if !done[i]
                && let Some(val) = val
            {
                out.push((*k, val.clone()));
            }
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

    /// `edit_metadata` with a title and authors: `after` holds them, keeps the
    /// rest, and converts to EPUB.
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
        // `before` and `after` agree on everything the patch left alone.
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
            crate::formats::kfx::converts_to_epub(&out),
            "patched KFX must still convert to EPUB"
        );
    }

    /// A re-key sets `asin` and `content_id` to one value and leaves the title and
    /// media untouched.
    #[test]
    fn rekey_sets_asin_and_content_id_together() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let before = loader::load(&kfx).expect("load original");

        let key = super::super::metadata::generate_content_id("urn:uuid:re-key");
        let out = edit_metadata(
            &kfx,
            &MetadataPatch {
                asin: Some(key.clone()),
                content_id: Some(key.clone()),
                ..Default::default()
            },
        )
        .expect("edit_metadata");
        let after = loader::load(&out).expect("rewritten container must re-load");

        assert_eq!(after.metadata.asin.as_deref(), Some(&*key));
        assert_eq!(
            before.metadata.title, after.metadata.title,
            "a re-key is not a metadata edit"
        );
        assert_eq!(before.raw_media.len(), after.raw_media.len());
        assert!(crate::formats::kfx::converts_to_epub(&out));
    }

    /// `publisher` alone changes; title and authors are untouched.
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

    fn values_of(items: &[IonValue]) -> Vec<(&str, &str)> {
        items
            .iter()
            .filter_map(|it| Some((item_key(it)?, item_value(it)?)))
            .collect()
    }

    #[test]
    fn a_reordered_author_keeps_its_pronunciation() {
        let items = vec![
            kv_item(KEY_AUTHOR, "Doe, Jane"),
            kv_item(KEY_AUTHOR_PRON, "Doe, Jane"),
            kv_item(KEY_TITLE, "T"),
        ];
        let patch = MetadataPatch {
            authors: Some(vec!["Jane Doe".into()]),
            ..Default::default()
        };
        assert_eq!(
            values_of(&patch_title_items(&items, &patch)),
            [
                ("author", "Jane Doe"),
                ("author_pronunciation", "Doe, Jane"),
                ("title", "T")
            ]
        );
    }

    #[test]
    fn pronunciations_go_once_any_author_they_name_is_replaced() {
        let items = vec![
            kv_item(KEY_AUTHOR, "Doe, Jane"),
            kv_item(KEY_AUTHOR, "Roe, John"),
            kv_item(KEY_AUTHOR_PRON, "Doe, Jane"),
            kv_item(KEY_AUTHOR_PRON, "Roe, John"),
        ];
        let patch = MetadataPatch {
            authors: Some(vec!["Jane Doe".into(), "Ann Writer".into()]),
            ..Default::default()
        };
        assert_eq!(
            values_of(&patch_title_items(&items, &patch)),
            [("author", "Jane Doe"), ("author", "Ann Writer")],
            "a positional list cannot keep the first key without misreading the second"
        );
    }

    #[test]
    fn an_unchanged_item_list_is_returned_as_it_was() {
        let items = vec![
            kv_item(KEY_TITLE, "T"),
            kv_item(KEY_AUTHOR, "A"),
            kv_item(KEY_LANGUAGE, "en"),
        ];
        let patch = MetadataPatch {
            title: Some("T".into()),
            authors: Some(vec!["A".into()]),
            language: Some("en".into()),
            ..Default::default()
        };
        assert_eq!(patch_title_items(&items, &patch), items);
    }

    #[test]
    fn an_empty_value_removes_the_key() {
        let items = vec![
            kv_item(KEY_TITLE, "T"),
            kv_item(KEY_PUBLISHER, "P"),
            kv_item(KEY_ISSUE_DATE, "2001-02-03"),
        ];
        let patch = MetadataPatch {
            publisher: Some(String::new()),
            issue_date: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            values_of(&patch_title_items(&items, &patch)),
            [("title", "T")]
        );
    }

    #[test]
    fn the_flat_fragment_beside_a_490_takes_only_fields_it_has() {
        let flat = IonValue::Struct(vec![(
            KfxSymbol::Title as u64,
            IonValue::String("Old".into()),
        )]);
        let patch = MetadataPatch {
            title: Some("New".into()),
            publisher: Some("P".into()),
            ..Default::default()
        };
        let beside = patch_flat_metadata(&flat, &patch, false);
        assert_eq!(
            beside,
            IonValue::Struct(vec![(
                KfxSymbol::Title as u64,
                IonValue::String("New".into())
            )])
        );
        let alone = patch_flat_metadata(&flat, &patch, true);
        assert_eq!(alone.as_struct().map(<[_]>::len), Some(2));
    }

    fn synthetic_kfx() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let mut put = |name: &str, body: &str, opts: SimpleFileOptions| {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        put("mimetype", "application/epub+zip", stored);
        put(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            SimpleFileOptions::default(),
        );
        put(
            "content.opf",
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:sprocket</dc:identifier>
    <dc:title>Old Title</dc:title>
    <dc:creator>Doe, Jane</dc:creator>
    <dc:language>en</dc:language>
    <dc:publisher>Old Press</dc:publisher>
    <dc:date>2001-02-03</dc:date>
  </metadata>
  <manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="c"/></spine>
</package>"#,
            SimpleFileOptions::default(),
        );
        put(
            "c.xhtml",
            r#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c</title></head><body><p>Text.</p></body></html>"#,
            SimpleFileOptions::default(),
        );
        let epub = zip.finish().unwrap().into_inner();
        let mut book = crate::Book::from_bytes(&epub, crate::Format::Epub).unwrap();
        let mut kfx = std::io::Cursor::new(Vec::new());
        book.export(crate::Format::Kfx, &mut kfx).unwrap();
        kfx.into_inner()
    }

    #[test]
    fn a_patch_the_container_already_carries_returns_the_input() {
        let kfx = synthetic_kfx();
        let patch = MetadataPatch {
            title: Some("Old Title".into()),
            authors: Some(vec!["Doe, Jane".into()]),
            publisher: Some("Old Press".into()),
            ..Default::default()
        };
        assert_eq!(edit_metadata(&kfx, &patch).unwrap(), kfx);
    }

    #[test]
    fn an_issue_date_is_stored_as_its_date_part() {
        let kfx = synthetic_kfx();
        let patch = MetadataPatch {
            issue_date: Some("2009-05-19T00:00:00+00:00".into()),
            authors: Some(vec!["Jane Doe".into()]),
            publisher: Some(String::new()),
            ..Default::default()
        };
        let after = loader::load(&edit_metadata(&kfx, &patch).unwrap()).unwrap();
        assert_eq!(after.metadata.issue_date.as_deref(), Some("2009-05-19"));
        assert_eq!(after.metadata.authors, ["Jane Doe"]);
        assert_eq!(after.metadata.publisher, None);
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
