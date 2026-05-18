//! Metadata round-trip validation — verify the OPF metadata fields the
//! Kindle reader exposes survive into KFX.
//!
//! Source side: parse the OPF (`Metadata` struct from boko's epub parser).
//! KFX side: walk two entities:
//!
//! - **`metadata` ($258)** — has `reading_orders[*].page_progression_direction`.
//! - **`book_metadata` ($490)** — has `categorised_metadata`, a list of
//!   `{category, metadata: [{key, value}, ...]}` blobs. The Kindle library
//!   service reads keys like `title`, `author`, `language`, `cover_image`.
//!
//! Round-trip rules:
//!
//! - `title` → KindleTitle.`title` (exact string match)
//! - `language` → KindleTitle.`language`
//! - `authors[0]` → KindleTitle.`author` (KFX only stores one; if source has
//!   multiple, this validator checks the first survives — additional authors
//!   are silently lost by design)
//! - `cover_image` (path) → must produce a non-empty `cover_image` value
//!   pointing at a resource_name (existence check only; the path
//!   transformation is intentional)
//! - `page_progression_direction` → metadata.reading_orders[0].
//!   page_progression_direction (`$rtl` / `$ltr` / omitted)

use std::collections::HashMap;
use std::io::Cursor;

use zip::ZipArchive;

use crate::epub::{parse_container_xml, parse_opf};
use crate::kfx::container::{
    extract_doc_symbols, parse_container_header, parse_container_info, parse_index_table,
    skip_enty_header,
};
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::symbols::{KFX_SYMBOL_TABLE, KfxSymbol};

/// A field-level mismatch between EPUB and KFX. Direction-neutral: `epub` is
/// the value seen on the EPUB side, `kfx` is the value seen on the KFX side,
/// regardless of which one is the conversion source.
#[derive(Debug, Clone)]
pub struct FieldDiff {
    pub field: &'static str,
    pub epub: String,
    pub kfx: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub epub_title: String,
    pub epub_language: String,
    pub epub_first_author: String,
    pub epub_identifier: String,
    pub epub_has_cover: bool,
    pub epub_ppd: Option<String>,
    pub epub_extra_authors: usize,

    pub kfx_title: String,
    pub kfx_language: String,
    pub kfx_first_author: String,
    pub kfx_cover_image: Option<String>,
    pub kfx_ppd: Option<String>,
    /// `book_id` field if present — derived from EPUB identifier in EPUB→KFX
    /// flow, or already present from a prior conversion in KFX→EPUB flow.
    pub kfx_book_id: Option<String>,

    pub diffs: Vec<FieldDiff>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.diffs.is_empty()
    }

    pub fn print_summary(&self, _dir: super::Direction) {
        println!("Title:");
        println!("  EPUB: {:?}", self.epub_title);
        println!("  KFX:  {:?}", self.kfx_title);
        println!("Language:");
        println!("  EPUB: {:?}", self.epub_language);
        println!("  KFX:  {:?}", self.kfx_language);
        println!("Author (first):");
        println!(
            "  EPUB: {:?}{}",
            self.epub_first_author,
            if self.epub_extra_authors > 0 {
                format!(
                    " (+{} more; KFX stores only first by design)",
                    self.epub_extra_authors
                )
            } else {
                String::new()
            }
        );
        println!("  KFX:  {:?}", self.kfx_first_author);
        println!("Cover image:");
        println!("  EPUB has cover:  {}", self.epub_has_cover);
        println!("  KFX cover_image: {:?}", self.kfx_cover_image);
        println!("Page progression direction:");
        println!("  EPUB: {:?}", self.epub_ppd);
        println!("  KFX:  {:?}", self.kfx_ppd);
        println!("Identifier round-trip:");
        println!("  EPUB identifier: {:?}", self.epub_identifier);
        println!("  KFX book_id:     {:?}", self.kfx_book_id);
        println!("Defects: {}", self.diffs.len());
    }

    pub fn print_details(&self, _limit: usize, _dir: super::Direction) {
        if self.diffs.is_empty() {
            return;
        }
        println!("\n--- Field mismatches ---");
        for d in &self.diffs {
            println!("  {}: epub={:?}  kfx={:?}", d.field, d.epub, d.kfx);
        }
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let epub = extract_epub_metadata(epub_bytes)?;
    let kfx = extract_kfx_metadata(kfx_bytes)?;

    let mut diffs: Vec<FieldDiff> = Vec::new();

    if !epub.title.is_empty() && epub.title != kfx.title {
        diffs.push(FieldDiff {
            field: "title",
            epub: epub.title.clone(),
            kfx: kfx.title.clone(),
        });
    }
    if !epub.language.is_empty() && epub.language != kfx.language {
        diffs.push(FieldDiff {
            field: "language",
            epub: epub.language.clone(),
            kfx: kfx.language.clone(),
        });
    }
    if !epub.first_author.is_empty() && epub.first_author != kfx.first_author {
        diffs.push(FieldDiff {
            field: "author",
            epub: epub.first_author.clone(),
            kfx: kfx.first_author.clone(),
        });
    }
    // Cover: EPUB declares a cover path → KFX should have a non-empty
    // cover_image pointing at a resource. We don't compare paths; the
    // transformation OPF-path → KFX-resource-name is intentional.
    if epub.has_cover && kfx.cover_image.as_deref().unwrap_or("").is_empty() {
        diffs.push(FieldDiff {
            field: "cover_image",
            epub: epub.cover_path.clone().unwrap_or_default(),
            kfx: "(missing)".into(),
        });
    }
    // PPD: only check when EPUB declared one. EPUB "default" or absent
    // matches KFX omission (no $rtl / $ltr emitted).
    match (&epub.ppd, &kfx.ppd) {
        (Some(s), kfx_ppd) if s == "rtl" || s == "ltr" => {
            let kfx_str = kfx_ppd.clone().unwrap_or_default();
            // KFX stores it as "$rtl" or "$ltr"; normalise.
            let kfx_norm = kfx_str.trim_start_matches('$').to_string();
            if kfx_norm != *s {
                diffs.push(FieldDiff {
                    field: "page_progression_direction",
                    epub: s.clone(),
                    kfx: kfx_str,
                });
            }
        }
        _ => {}
    }

    Ok(Report {
        epub_title: epub.title,
        epub_language: epub.language,
        epub_first_author: epub.first_author,
        epub_identifier: epub.identifier,
        epub_has_cover: epub.has_cover,
        epub_ppd: epub.ppd,
        epub_extra_authors: epub.extra_authors,
        kfx_title: kfx.title,
        kfx_language: kfx.language,
        kfx_first_author: kfx.first_author,
        kfx_cover_image: kfx.cover_image,
        kfx_ppd: kfx.ppd,
        kfx_book_id: kfx.book_id,
        diffs,
    })
}

// ============================================================================
// Source-side
// ============================================================================

#[derive(Debug, Default)]
struct EpubMetadata {
    title: String,
    language: String,
    first_author: String,
    identifier: String,
    has_cover: bool,
    cover_path: Option<String>,
    ppd: Option<String>,
    extra_authors: usize,
}

fn extract_epub_metadata(epub_bytes: &[u8]) -> Result<EpubMetadata, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_bytes = read_zip_entry(&mut archive, &opf_path)
        .map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let enc = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, enc);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    Ok(EpubMetadata {
        title: opf.metadata.title.clone(),
        language: opf.metadata.language.clone(),
        first_author: opf.metadata.authors.first().cloned().unwrap_or_default(),
        extra_authors: opf.metadata.authors.len().saturating_sub(1),
        identifier: opf.metadata.identifier.clone(),
        has_cover: opf.metadata.cover_image.is_some(),
        cover_path: opf.metadata.cover_image.clone(),
        ppd: opf.metadata.page_progression_direction.clone(),
    })
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

// ============================================================================
// KFX-side
// ============================================================================

#[derive(Debug, Default)]
struct KfxMetadata {
    title: String,
    language: String,
    first_author: String,
    cover_image: Option<String>,
    book_id: Option<String>,
    ppd: Option<String>,
}

fn extract_kfx_metadata(kfx_bytes: &[u8]) -> Result<KfxMetadata, String> {
    let header =
        parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    if header.container_info_offset + header.container_info_length > kfx_bytes.len() {
        return Err("container info out of bounds".into());
    }
    let info_data = &kfx_bytes[header.container_info_offset
        ..header.container_info_offset + header.container_info_length];
    let info = parse_container_info(info_data)
        .map_err(|e| format!("kfx container info: {:?}", e))?;

    let extended_symbols = match info.doc_symbols {
        Some((off, len)) if off + len <= kfx_bytes.len() => {
            extract_doc_symbols(&kfx_bytes[off..off + len])
        }
        _ => Vec::new(),
    };
    let base_symbol_count = KFX_SYMBOL_TABLE.len() as u64;
    let resolve_sym = |id: u64| -> String {
        if id < base_symbol_count {
            KFX_SYMBOL_TABLE
                .get(id as usize)
                .copied()
                .unwrap_or("?")
                .to_string()
        } else {
            let idx = (id - base_symbol_count) as usize;
            extended_symbols
                .get(idx)
                .cloned()
                .unwrap_or_else(|| "?".to_string())
        }
    };

    let Some((idx_off, idx_len)) = info.index else {
        return Err("kfx: no index table".into());
    };
    let entities =
        parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    let metadata_type = KfxSymbol::Metadata as u32;
    let book_metadata_type = KfxSymbol::BookMetadata as u32;

    let mut out = KfxMetadata::default();
    let mut kv: HashMap<String, String> = HashMap::new();

    for ent in &entities {
        if ent.type_id == metadata_type {
            if let Some(value) = parse_entity(kfx_bytes, ent) {
                extract_ppd(&value, &resolve_sym, &mut out.ppd);
            }
        } else if ent.type_id == book_metadata_type {
            if let Some(value) = parse_entity(kfx_bytes, ent) {
                extract_categorised(&value, &resolve_sym, &mut kv);
            }
        }
    }

    out.title = kv.get("title").cloned().unwrap_or_default();
    out.language = kv.get("language").cloned().unwrap_or_default();
    out.first_author = kv.get("author").cloned().unwrap_or_default();
    out.cover_image = kv.get("cover_image").cloned();
    out.book_id = kv.get("book_id").cloned();
    Ok(out)
}

fn parse_entity(data: &[u8], ent: &crate::kfx::container::EntityLoc) -> Option<IonValue> {
    if ent.offset + ent.length > data.len() {
        return None;
    }
    let entity = &data[ent.offset..ent.offset + ent.length];
    let ion = skip_enty_header(entity);
    IonParser::new(ion).parse().ok()
}

/// Walk `metadata` ($258): `{reading_orders: [{page_progression_direction: $rtl, ...}, ...]}`.
fn extract_ppd<F>(value: &IonValue, resolve_sym: &F, ppd: &mut Option<String>)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    for (k, v) in fields {
        if resolve_sym(*k) == "reading_orders"
            && let IonValue::List(items) = v
        {
            for r in items {
                if let IonValue::Struct(rfields) = r {
                    for (rk, rv) in rfields {
                        if resolve_sym(*rk) == "page_progression_direction"
                            && let IonValue::Symbol(s) = rv
                        {
                            *ppd = Some(resolve_sym(*s));
                        }
                    }
                }
            }
        }
    }
}

/// Walk `book_metadata` ($490): `{categorised_metadata: [{category, metadata: [{key, value}, ...]}, ...]}`.
/// Collect all (key, value) pairs into a flat map.
fn extract_categorised<F>(value: &IonValue, resolve_sym: &F, out: &mut HashMap<String, String>)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    for (k, v) in fields {
        if resolve_sym(*k) == "categorised_metadata"
            && let IonValue::List(cats) = v
        {
            for cat in cats {
                let IonValue::Struct(cfields) = cat else { continue };
                for (ck, cv) in cfields {
                    if resolve_sym(*ck) == "metadata"
                        && let IonValue::List(entries) = cv
                    {
                        for entry in entries {
                            let IonValue::Struct(efields) = entry else { continue };
                            let mut key: String = String::new();
                            let mut val: String = String::new();
                            for (ek, ev) in efields {
                                match resolve_sym(*ek).as_str() {
                                    "key" => {
                                        if let IonValue::String(s) = ev {
                                            key = s.clone();
                                        }
                                    }
                                    "value" => {
                                        match ev {
                                            IonValue::String(s) => val = s.clone(),
                                            IonValue::Symbol(s) => val = resolve_sym(*s),
                                            IonValue::Bool(b) => val = b.to_string(),
                                            _ => {}
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if !key.is_empty() {
                                out.insert(key, val);
                            }
                        }
                    }
                }
            }
        }
    }
}
