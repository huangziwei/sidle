//! KFX metadata schema: [`metadata_schema`] holds the rules converting book
//! metadata into KFX `categorised_metadata`. A new field is a new rule.

use crate::model::Metadata;

/// A CJK language family the Kindle treats specially (CJK fonts, upright
/// vertical orientation, per-script reflow features). Latin/other languages
/// don't classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CjkLang {
    /// Traditional Chinese (`tcn` in Amazon's reflow-feature vocabulary).
    ZhHant,
    /// Simplified Chinese (`cn`).
    ZhHans,
    /// Generic Chinese with no script/region — passed through as `zh`.
    ZhGeneric,
    /// Japanese.
    Ja,
    /// Korean.
    Ko,
}

/// Classify a book's (BCP-47-ish) language tag into a [`CjkLang`], or `None`
/// for Latin/other. Case- and separator-insensitive (`zh_Hant`, `ZH-HANT`,
/// `zh-hant` all match). Both language mappers below share it.
pub fn classify_cjk_language(book_lang: &str) -> Option<CjkLang> {
    let l = book_lang.trim().to_ascii_lowercase().replace('_', "-");
    match l.as_str() {
        "zh-hant" | "zh-tw" | "zh-hk" | "zh-mo" => Some(CjkLang::ZhHant),
        "zh-hans" | "zh-cn" | "zh-sg" | "zh-my" => Some(CjkLang::ZhHans),
        "zh" => Some(CjkLang::ZhGeneric),
        _ if l == "ja" || l.starts_with("ja-") => Some(CjkLang::Ja),
        _ if l == "ko" || l.starts_with("ko-") => Some(CjkLang::Ko),
        _ => None,
    }
}

/// The book-level `language` metadata value in Amazon's device form: the
/// lowercase script-subtag form (`zh-hant`, not `zh-Hant`). Non-CJK tags pass
/// through. [`kfx_content_language`] stamps `zh-tw` on each `$style`.
pub fn kfx_book_language(book_lang: &str) -> String {
    match classify_cjk_language(book_lang) {
        Some(CjkLang::ZhHant) => "zh-hant".to_string(),
        Some(CjkLang::ZhHans) => "zh-hans".to_string(),
        Some(CjkLang::ZhGeneric) => "zh".to_string(),
        Some(CjkLang::Ja) => "ja".to_string(),
        Some(CjkLang::Ko) => "ko".to_string(),
        None => book_lang.to_string(),
    }
}

/// The content-level `language` for each reflowable `$style`, in Amazon's
/// device locale form. Empty for languages needing no hint; Traditional Chinese
/// is `zh-tw` here and `zh-hant` in [`kfx_book_language`].
pub fn kfx_content_language(book_lang: &str) -> String {
    match classify_cjk_language(book_lang) {
        Some(CjkLang::ZhHant) => "zh-tw".to_string(),
        Some(CjkLang::ZhHans) => "zh-cn".to_string(),
        Some(CjkLang::ZhGeneric) => "zh".to_string(),
        Some(CjkLang::Ja) => "ja".to_string(),
        Some(CjkLang::Ko) => "ko".to_string(),
        None => String::new(),
    }
}

/// The `com.amazon.yjconversion` `content_features` reflow-language marker key
/// for a CJK language, `None` where the language has none.
pub fn cjk_reflow_feature(book_lang: &str) -> Option<(&'static str, i64)> {
    match classify_cjk_language(book_lang) {
        Some(CjkLang::ZhHant) => Some(("tcn-reflow-language", 1)),
        Some(CjkLang::ZhHans | CjkLang::ZhGeneric) => Some(("cn-reflow-language", 1)),
        Some(CjkLang::Ja) => Some(("jp-reflow-language", 1)),
        Some(CjkLang::Ko) | None => None,
    }
}

/// Category for KFX metadata entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataCategory {
    /// Book title, author, language, etc.
    KindleTitle,
    /// eBook capabilities (selection, nested_span, etc.)
    KindleEbook,
    /// Creator/audit information
    KindleAudit,
    /// Device-level capability flags. A fixed-layout book states its
    /// comic-reader keys here.
    KindleCapability,
}

impl MetadataCategory {
    /// Get the KFX category string.
    pub fn as_str(self) -> &'static str {
        match self {
            MetadataCategory::KindleTitle => "kindle_title_metadata",
            MetadataCategory::KindleEbook => "kindle_ebook_metadata",
            MetadataCategory::KindleAudit => "kindle_audit_metadata",
            MetadataCategory::KindleCapability => "kindle_capability_metadata",
        }
    }
}

/// A rule for mapping a metadata field to KFX format.
#[derive(Debug, Clone)]
pub struct MetadataRule {
    /// The KFX key name (e.g., "title", "author").
    pub key: &'static str,
    /// Which category this belongs to.
    pub category: MetadataCategory,
    /// How to extract the value from Metadata.
    pub source: MetadataSource,
}

/// Source of metadata value.
#[derive(Debug, Clone)]
pub enum MetadataSource {
    /// Static string value.
    Static(&'static str),
    /// Static boolean value (emitted as Ion bool, not string).
    StaticBool(bool),
    /// Dynamic value from Metadata struct.
    Dynamic(MetadataField),
}

/// Emitted metadata value: one variant per Ion type a metadatum takes.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
    Text(String),
    Bool(bool),
    Int(i64),
}

impl PartialEq<&str> for MetadataValue {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, MetadataValue::Text(s) if s == *other)
    }
}

impl PartialEq<str> for MetadataValue {
    fn eq(&self, other: &str) -> bool {
        matches!(self, MetadataValue::Text(s) if s == other)
    }
}

/// Fields that can be extracted from Metadata.
#[derive(Debug, Clone, Copy)]
pub enum MetadataField {
    Title,
    Language,
    Author,
    Description,
    Publisher,
    Identifier,
    Date,
    CoverImage,
    /// Asset ID - from context (container ID), not Metadata.
    AssetId,
    /// Book ID - from context (derived from identifier), not Metadata.
    BookId,
    /// ASIN - from context. A sideloaded file gets a minted 32-char
    /// uppercase-alphanumeric value, the Kindle library's per-book cache key.
    Asin,
    /// content_id - the same value as `Asin`.
    ContentId,
    /// dcterms:modified timestamp
    ModifiedDate,
    /// First contributor with role="trl" (translator)
    Translator,
    /// file-as for title (sort key)
    TitleSort,
    /// Per-author sort keys (`Metadata.author_sorts`); emitted as one
    /// `author_pronunciation` entry per author, like `Author`.
    AuthorSort,
    /// Series/collection name
    SeriesName,
    /// Series position (group-position)
    SeriesPosition,
    /// Whether the book ships typefaces of its own — from context, not
    /// Metadata. Emitted as `override_kindle_font`.
    PublisherFonts,
}

impl MetadataField {
    /// Extract the value from a Metadata struct.
    /// Returns None if the field is empty or not set.
    pub fn extract(self, meta: &Metadata) -> Option<&str> {
        match self {
            MetadataField::Title => {
                if meta.title.is_empty() {
                    None
                } else {
                    Some(&meta.title)
                }
            }
            MetadataField::Language => {
                if meta.language.is_empty() {
                    None
                } else {
                    Some(&meta.language)
                }
            }
            // Single-author convenience. `build_category_entries` emits one
            // repeated `author` entry per author, Amazon's shape.
            MetadataField::Author => meta.authors.first().map(|s| s.as_str()),
            MetadataField::Description => meta.description.as_deref(),
            MetadataField::Publisher => meta.publisher.as_deref(),
            MetadataField::Identifier => {
                if meta.identifier.is_empty() {
                    None
                } else {
                    Some(&meta.identifier)
                }
            }
            MetadataField::Date => meta.date.as_deref(),
            MetadataField::CoverImage => meta.cover_image.as_deref(),
            MetadataField::ModifiedDate => meta.modified_date.as_deref(),
            MetadataField::Translator => {
                // Find first contributor with role "trl"
                meta.contributors
                    .iter()
                    .find(|c| c.role.as_deref() == Some("trl"))
                    .map(|c| c.name.as_str())
            }
            MetadataField::TitleSort => meta.title_sort.as_deref(),
            // Single-value convenience, like `Author`; `build_category_entries`
            // emits one repeated `author_pronunciation` entry per element.
            MetadataField::AuthorSort => meta.author_sorts.first().map(|s| s.as_str()),
            MetadataField::SeriesName => meta.collection.as_ref().map(|c| c.name.as_str()),
            // These are context-driven or need special handling
            MetadataField::AssetId
            | MetadataField::BookId
            | MetadataField::Asin
            | MetadataField::ContentId
            | MetadataField::PublisherFonts
            | MetadataField::SeriesPosition => None,
        }
    }
}

/// Get the standard KFX metadata schema: every rule converting book metadata to
/// KFX. A new field is a new rule here, with no export-code change.
pub fn metadata_schema() -> Vec<MetadataRule> {
    vec![
        // kindle_title_metadata category
        MetadataRule {
            key: "title",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Title),
        },
        MetadataRule {
            key: "language",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Language),
        },
        MetadataRule {
            key: "author",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Author),
        },
        MetadataRule {
            key: "description",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Description),
        },
        MetadataRule {
            key: "publisher",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Publisher),
        },
        MetadataRule {
            key: "issue_date",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Date),
        },
        MetadataRule {
            key: "cover_image",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::CoverImage),
        },
        MetadataRule {
            key: "asset_id",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::AssetId),
        },
        MetadataRule {
            key: "book_id",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::BookId),
        },
        MetadataRule {
            key: "ASIN",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Asin),
        },
        MetadataRule {
            key: "content_id",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::ContentId),
        },
        // Always PDOC — `cde_content_type` states provenance, not genre, and
        // this writer produces personal documents. A store type (EBOK, MAGZ, …)
        // triggers an ASIN-catalogue lookup that fails for a sideload.
        MetadataRule {
            key: "cde_content_type",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Static("PDOC"),
        },
        MetadataRule {
            key: "is_sample",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::StaticBool(false),
        },
        MetadataRule {
            key: "override_kindle_font",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::PublisherFonts),
        },
        // Extended metadata for better round-trip fidelity
        MetadataRule {
            key: "modified_date",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::ModifiedDate),
        },
        MetadataRule {
            key: "translator",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::Translator),
        },
        MetadataRule {
            key: "title_pronunciation",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::TitleSort),
        },
        MetadataRule {
            key: "author_pronunciation",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::AuthorSort),
        },
        MetadataRule {
            key: "series_name",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::SeriesName),
        },
        MetadataRule {
            key: "series_position",
            category: MetadataCategory::KindleTitle,
            source: MetadataSource::Dynamic(MetadataField::SeriesPosition),
        },
        // kindle_ebook_metadata category
        MetadataRule {
            key: "selection",
            category: MetadataCategory::KindleEbook,
            source: MetadataSource::Static("enabled"),
        },
        MetadataRule {
            key: "nested_span",
            category: MetadataCategory::KindleEbook,
            source: MetadataSource::Static("enabled"),
        },
        // kindle_audit_metadata category
        MetadataRule {
            key: "file_creator",
            category: MetadataCategory::KindleAudit,
            source: MetadataSource::Static("bokai"),
        },
    ]
}

use crate::util::truncate_to_date;

/// Context for metadata entry building: the values transformed during export,
/// such as resource names generated by the export pass.
#[derive(Debug, Default)]
pub struct MetadataContext<'a> {
    /// Version string for audit metadata.
    pub version: Option<&'a str>,
    /// Cover image resource name (e.g., "e6"), not the path.
    pub cover_resource_name: Option<&'a str>,
    /// Asset ID (same as container ID, changes per export).
    /// Format: "CR!" + 28 uppercase alphanumeric characters.
    pub asset_id: Option<&'a str>,
    /// Book ID (stable per publication, derived from identifier).
    /// Format: 23-character URL-safe Base64.
    pub book_id: Option<String>,
    /// ASIN — real Amazon catalogue identifier, set only when the source
    /// carries a genuine one (KFX → EPUB → KFX). A synthesized value returns
    /// nothing from any catalogue query.
    pub asin: Option<String>,
    /// `override_kindle_font`: `true` offers the publisher's font in the picker.
    pub has_publisher_fonts: bool,

    /// `content_id` — device-internal key for the per-book `.sdr` state
    /// directory. An empty value breaks the in-book exit menu on a PDOC title.
    pub content_id: Option<String>,
}

/// Generate a book ID from a publication identifier: 23-character URL-safe
/// Base64 (version byte + 16 derived bytes), derived deterministically and
/// stable across exports of the same book.
pub fn generate_book_id(identifier: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Version prefix (0x05 based on reference KFX files)
    let mut bytes = vec![0x05u8];

    // Hash the identifier to get deterministic bytes
    let mut hasher = DefaultHasher::new();
    identifier.hash(&mut hasher);
    let hash1 = hasher.finish();
    // Hash again with salt for more bytes
    "bokai-book-id".hash(&mut hasher);
    let hash2 = hasher.finish();

    bytes.extend_from_slice(&hash1.to_le_bytes());
    bytes.extend_from_slice(&hash2.to_le_bytes());

    // URL-safe Base64 encode (no padding), 17 bytes → 23 chars
    base64_url_encode(&bytes[..17])
}

/// URL-safe Base64 encoding without padding.
fn base64_url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut result = String::new();
    let mut bits: u32 = 0;
    let mut bit_count = 0;

    for &byte in bytes {
        bits = (bits << 8) | byte as u32;
        bit_count += 8;

        while bit_count >= 6 {
            bit_count -= 6;
            let idx = ((bits >> bit_count) & 0x3F) as usize;
            result.push(ALPHABET[idx] as char);
        }
    }

    // Handle remaining bits (no padding)
    if bit_count > 0 {
        let idx = ((bits << (6 - bit_count)) & 0x3F) as usize;
        result.push(ALPHABET[idx] as char);
    }

    result
}

/// Generate a 32-char uppercase-alphanumeric `content_id` deterministically from
/// the publication identifier — the device-internal key naming the per-book
/// `.sdr` directory. Local-only, never sent to Amazon.
pub fn generate_content_id(identifier: &str) -> String {
    let digest = sha1_smol::Sha1::from(identifier.as_bytes())
        .digest()
        .bytes();
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push(ALPHABET[((byte >> 4) & 0x1F) as usize] as char);
        out.push(ALPHABET[(byte & 0x1F) as usize] as char);
    }
    debug_assert_eq!(out.len(), 32);
    out
}

/// Real Amazon catalogue ASINs are 10 characters, uppercase alphanumeric
/// (no lowercase, no symbols). A bokai-synthesized fallback is 32 chars of
/// Crockford-style Base32 — distinguishable by length alone.
pub fn looks_like_real_amazon_asin(s: &str) -> bool {
    s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The identifier a produced file carries, synthesized from `meta.identifier`
/// via [`generate_content_id`]; `None` without one. A source ASIN never passes
/// through: it collapses the conversion and its source into one library entry.
pub fn resolve_export_asin(meta: &Metadata) -> Option<String> {
    (!meta.identifier.is_empty()).then(|| generate_content_id(&meta.identifier))
}

/// Build metadata entries for a category from the schema: applies the rules to
/// `meta` and `ctx`, returning (key, value) pairs.
pub fn build_category_entries(
    category: MetadataCategory,
    meta: &Metadata,
    ctx: &MetadataContext,
) -> Vec<(&'static str, MetadataValue)> {
    let schema = metadata_schema();
    let mut entries = Vec::new();

    for rule in schema.iter().filter(|r| r.category == category) {
        let value: Option<MetadataValue> = match &rule.source {
            MetadataSource::Static(s) => Some(MetadataValue::Text(s.to_string())),
            MetadataSource::StaticBool(b) => Some(MetadataValue::Bool(*b)),
            MetadataSource::Dynamic(field) => {
                // Special handling for fields that need transformation
                match field {
                    MetadataField::CoverImage => {
                        // Use the resource name from context, not the path from metadata
                        ctx.cover_resource_name
                            .map(|s| MetadataValue::Text(s.to_string()))
                    }
                    MetadataField::Date => {
                        // KFX expects YYYY-MM-DD format, not full ISO timestamp
                        field
                            .extract(meta)
                            .map(|s| MetadataValue::Text(truncate_to_date(s)))
                    }
                    MetadataField::AssetId => {
                        // Asset ID from context (same as container ID)
                        ctx.asset_id.map(|s| MetadataValue::Text(s.to_string()))
                    }
                    MetadataField::BookId => {
                        // Book ID from context (derived from identifier)
                        ctx.book_id.clone().map(MetadataValue::Text)
                    }
                    MetadataField::Asin => {
                        // Passthrough only — never fabricated. Empty when the
                        // source EPUB carries no ASIN.
                        ctx.asin.clone().map(MetadataValue::Text)
                    }
                    MetadataField::PublisherFonts => {
                        Some(MetadataValue::Bool(ctx.has_publisher_fonts))
                    }
                    MetadataField::ContentId => {
                        // Device-internal `.sdr` key, synthesized from the
                        // identifier. Independent of the ASIN.
                        ctx.content_id.clone().map(MetadataValue::Text)
                    }
                    MetadataField::Author => {
                        // One `author` entry PER author — Amazon's
                        // kindle_title_metadata repeats the key. The importer
                        // reads each repeated key verbatim.
                        for a in &meta.authors {
                            entries.push((rule.key, MetadataValue::Text(a.clone())));
                        }
                        None
                    }
                    MetadataField::AuthorSort => {
                        // Repeated per author like `author` — Amazon emits
                        // the pronunciations positionally, one key per
                        // author, in the same order.
                        for s in &meta.author_sorts {
                            entries.push((rule.key, MetadataValue::Text(s.clone())));
                        }
                        None
                    }
                    MetadataField::ModifiedDate => {
                        // Always stamp the conversion time, never copy the source value —
                        // modified_date describes *this file*, not the work.
                        Some(MetadataValue::Text(crate::util::time_now_iso8601_utc()))
                    }
                    MetadataField::Language => {
                        // Normalize to Amazon's device form (`zh-Hant` →
                        // `zh-hant`). Non-CJK tags pass through.
                        field
                            .extract(meta)
                            .map(|s| MetadataValue::Text(kfx_book_language(s)))
                    }
                    MetadataField::SeriesPosition => {
                        // Series position from collection
                        meta.collection.as_ref().and_then(|c| c.position).map(|p| {
                            // A whole number formats as an integer
                            if p.fract() == 0.0 {
                                MetadataValue::Text(format!("{}", p as i64))
                            } else {
                                MetadataValue::Text(format!("{}", p))
                            }
                        })
                    }
                    _ => field
                        .extract(meta)
                        .map(|s| MetadataValue::Text(s.to_string())),
                }
            }
        };

        if let Some(v) = value {
            entries.push((rule.key, v));
        }
    }

    // Special case: add version to audit metadata
    if category == MetadataCategory::KindleAudit
        && let Some(v) = ctx.version
    {
        entries.push(("creator_version", MetadataValue::Text(v.to_string())));
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cjk_language_mappers() {
        // Traditional Chinese: book form differs from per-style form (verified
        // against Amazon's own KFX: book `zh-hant`, styles `zh-tw`).
        for tag in ["zh-Hant", "zh-TW", "zh_Hant", "ZH-HANT", "zh-HK"] {
            assert_eq!(kfx_book_language(tag), "zh-hant", "book {tag}");
            assert_eq!(kfx_content_language(tag), "zh-tw", "style {tag}");
            assert_eq!(cjk_reflow_feature(tag), Some(("tcn-reflow-language", 1)));
        }
        // Simplified Chinese.
        assert_eq!(kfx_book_language("zh-Hans"), "zh-hans");
        assert_eq!(kfx_content_language("zh-CN"), "zh-cn");
        assert_eq!(
            cjk_reflow_feature("zh-hans"),
            Some(("cn-reflow-language", 1))
        );
        // Japanese: book and style forms coincide; vertical marker handled at
        // the call site, base marker here.
        assert_eq!(kfx_book_language("ja"), "ja");
        assert_eq!(kfx_content_language("ja-JP"), "ja");
        assert_eq!(cjk_reflow_feature("ja"), Some(("jp-reflow-language", 1)));
        // Korean has fonts (book/style tag) but no reflow marker in Amazon's table.
        assert_eq!(kfx_book_language("ko"), "ko");
        assert_eq!(cjk_reflow_feature("ko"), None);
        // Latin passes through untouched, no content tag, no marker.
        assert_eq!(kfx_book_language("en-US"), "en-US");
        assert_eq!(kfx_content_language("en"), "");
        assert_eq!(cjk_reflow_feature("en"), None);
    }

    #[test]
    fn test_metadata_field_extraction() {
        let meta = Metadata {
            title: "Test Book".to_string(),
            authors: vec!["Author One".to_string()],
            language: "en".to_string(),
            description: Some("A description".to_string()),
            publisher: None,
            ..Default::default()
        };

        assert_eq!(MetadataField::Title.extract(&meta), Some("Test Book"));
        assert_eq!(MetadataField::Author.extract(&meta), Some("Author One"));
        assert_eq!(MetadataField::Language.extract(&meta), Some("en"));
        assert_eq!(
            MetadataField::Description.extract(&meta),
            Some("A description")
        );
        assert_eq!(MetadataField::Publisher.extract(&meta), None);
    }

    #[test]
    fn test_build_category_entries() {
        let meta = Metadata {
            title: "Test Book".to_string(),
            authors: vec!["Author".to_string()],
            language: "en".to_string(),
            ..Default::default()
        };

        let ctx = MetadataContext::default();
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);

        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "title" && v == "Test Book")
        );
        assert!(entries.iter().any(|(k, v)| *k == "language" && v == "en"));
        assert!(entries.iter().any(|(k, v)| *k == "author" && v == "Author"));
        assert!(!entries.iter().any(|(k, _)| *k == "description"));
    }

    #[test]
    fn test_build_ebook_entries() {
        let meta = Metadata::default();
        let ctx = MetadataContext::default();
        let entries = build_category_entries(MetadataCategory::KindleEbook, &meta, &ctx);

        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "selection" && v == "enabled")
        );
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "nested_span" && v == "enabled")
        );
    }

    #[test]
    fn test_build_audit_entries_with_version() {
        let meta = Metadata::default();
        let ctx = MetadataContext {
            version: Some("1.0.0"),
            ..Default::default()
        };
        let entries = build_category_entries(MetadataCategory::KindleAudit, &meta, &ctx);

        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "file_creator" && v == "bokai")
        );
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "creator_version" && v == "1.0.0")
        );
    }

    #[test]
    fn test_author_and_pronunciation_repeat_per_author() {
        // Amazon's shape: one `author` and one `author_pronunciation` entry
        // per author, positionally aligned.
        let meta = Metadata {
            title: "星の王子さま".to_string(),
            language: "ja".to_string(),
            authors: vec!["サン・テグジュペリ".to_string(), "管 啓次郎".to_string()],
            author_sorts: vec![
                "サン テグジュペリ".to_string(),
                "スガ ケイジロウ".to_string(),
            ],
            ..Default::default()
        };
        let ctx = MetadataContext::default();
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);

        let values = |key: &str| -> Vec<&str> {
            entries
                .iter()
                .filter(|(k, _)| *k == key)
                .filter_map(|(_, v)| match v {
                    MetadataValue::Text(s) => Some(s.as_str()),
                    MetadataValue::Bool(_) | MetadataValue::Int(_) => None,
                })
                .collect()
        };
        assert_eq!(values("author"), ["サン・テグジュペリ", "管 啓次郎"]);
        assert_eq!(
            values("author_pronunciation"),
            ["サン テグジュペリ", "スガ ケイジロウ"]
        );
    }

    #[test]
    fn test_build_entries_with_cover_image() {
        let meta = Metadata {
            title: "Test".to_string(),
            language: "en".to_string(),
            cover_image: Some("images/cover.jpg".to_string()),
            ..Default::default()
        };

        // An empty `MetadataContext` names no resource.
        let ctx = MetadataContext::default();
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);
        assert!(!entries.iter().any(|(k, _)| *k == "cover_image"));

        // A `MetadataContext` naming a resource.
        let ctx = MetadataContext {
            cover_resource_name: Some("e6"),
            ..Default::default()
        };
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "cover_image" && v == "e6")
        );
    }

    #[test]
    fn test_build_entries_with_issue_date() {
        let meta = Metadata {
            title: "Test".to_string(),
            language: "en".to_string(),
            date: Some("2022-05-26".to_string()),
            ..Default::default()
        };

        let ctx = MetadataContext::default();
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "issue_date" && v == "2022-05-26")
        );
    }

    #[test]
    fn test_category_strings() {
        assert_eq!(
            MetadataCategory::KindleTitle.as_str(),
            "kindle_title_metadata"
        );
        assert_eq!(
            MetadataCategory::KindleEbook.as_str(),
            "kindle_ebook_metadata"
        );
        assert_eq!(
            MetadataCategory::KindleAudit.as_str(),
            "kindle_audit_metadata"
        );
    }

    #[test]
    fn test_generate_book_id_format() {
        let id = super::generate_book_id("urn:uuid:12345678-1234-1234-1234-123456789abc");

        assert_eq!(id.len(), 23, "book_id should be 23 characters");

        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "book_id should only contain URL-safe Base64 characters"
        );
    }

    #[test]
    fn test_generate_book_id_deterministic() {
        let id1 = super::generate_book_id("urn:uuid:12345678-1234-1234-1234-123456789abc");
        let id2 = super::generate_book_id("urn:uuid:12345678-1234-1234-1234-123456789abc");

        assert_eq!(id1, id2, "book_id should be deterministic");
    }

    #[test]
    fn test_generate_book_id_different_inputs() {
        let id1 = super::generate_book_id("urn:uuid:aaaaaaaa-1234-1234-1234-123456789abc");
        let id2 = super::generate_book_id("urn:uuid:bbbbbbbb-1234-1234-1234-123456789abc");

        assert_ne!(
            id1, id2,
            "different identifiers should produce different book_ids"
        );
    }

    #[test]
    fn test_cde_content_type_is_pdoc() {
        let meta = Metadata {
            title: "Test".to_string(),
            language: "en".to_string(),
            ..Default::default()
        };

        let ctx = MetadataContext::default();
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);

        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "cde_content_type" && v == "PDOC")
        );
        // The periodical keys are absent entirely — an empty value declares the
        // book a magazine.
        for key in ["itemType", "periodicals_generation_V2"] {
            assert!(
                !entries.iter().any(|(k, _)| *k == key),
                "a book declares no {key}"
            );
        }
    }

    /// A periodical is a personal document like everything else this writer
    /// produces; its genre never reaches `cde_content_type`. See that rule for
    /// why declaring `MAGZ`/`NWPR`/`FEED` blanks the cover.
    #[test]
    fn a_periodical_is_still_declared_pdoc() {
        use crate::model::PeriodicalKind;
        for kind in [
            PeriodicalKind::Magazine,
            PeriodicalKind::Newspaper,
            PeriodicalKind::Blog,
        ] {
            let meta = Metadata {
                title: "The New Yorker".to_string(),
                language: "en".to_string(),
                periodical: Some(kind),
                ..Default::default()
            };
            let entries = build_category_entries(
                MetadataCategory::KindleTitle,
                &meta,
                &MetadataContext::default(),
            );
            assert!(
                entries
                    .iter()
                    .any(|(k, v)| *k == "cde_content_type" && v == "PDOC"),
                "{kind:?} is declared PDOC"
            );
            for key in ["itemType", "periodicals_generation_V2"] {
                assert!(
                    !entries.iter().any(|(k, _)| *k == key),
                    "{kind:?} declares no {key}"
                );
            }
        }
    }

    #[test]
    fn test_is_sample_and_override_kindle_font_are_ion_bools() {
        let meta = Metadata {
            title: "Test".to_string(),
            language: "en".to_string(),
            ..Default::default()
        };
        let entry = |ctx: &MetadataContext, key: &str| {
            build_category_entries(MetadataCategory::KindleTitle, &meta, ctx)
                .into_iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v)
        };

        let plain = MetadataContext::default();
        assert!(matches!(
            entry(&plain, "is_sample"),
            Some(MetadataValue::Bool(false))
        ));
        // No typefaces of its own: the device fonts are the only choice, with
        // no Publisher Font entry.
        assert!(matches!(
            entry(&plain, "override_kindle_font"),
            Some(MetadataValue::Bool(false))
        ));

        let with_fonts = MetadataContext {
            has_publisher_fonts: true,
            ..Default::default()
        };
        assert!(matches!(
            entry(&with_fonts, "override_kindle_font"),
            Some(MetadataValue::Bool(true))
        ));
    }

    #[test]
    fn export_asin_is_synthesized_even_from_a_catalogue_source() {
        // A conversion is not the catalogue item it was made from, and a
        // Kindle keys its catalog on this value: stamped with the original's
        // ASIN, the two are one entry to the device.
        let from_store = Metadata {
            asin: Some("B0CPJ2B88T".to_string()),
            identifier: "urn:uuid:9f1c".to_string(),
            ..Default::default()
        };
        let stamped = resolve_export_asin(&from_store).unwrap();
        assert_ne!(stamped, "B0CPJ2B88T");
        assert_eq!(stamped, generate_content_id("urn:uuid:9f1c"));
        assert!(!looks_like_real_amazon_asin(&stamped));

        // Derived from the identifier alone: the same book converted twice
        // keeps the reading position bound to it.
        let no_asin = Metadata {
            identifier: "urn:uuid:9f1c".to_string(),
            ..Default::default()
        };
        assert_eq!(resolve_export_asin(&no_asin).as_deref(), Some(&*stamped));

        // Nothing to derive from: the caller has to supply an identifier.
        assert_eq!(resolve_export_asin(&Metadata::default()), None);
    }

    #[test]
    fn test_asin_and_content_id_are_independent() {
        let meta = Metadata {
            title: "Test".to_string(),
            language: "en".to_string(),
            ..Default::default()
        };
        // Both populated: real ASIN passed through, synthesized content_id
        // distinct from it. Common shape for KFX → EPUB → KFX where the
        // source carried a catalogue ASIN.
        let ctx = MetadataContext {
            asin: Some("B0CPJ2B88T".to_string()),
            content_id: Some("GPAAHSEAGDCDOFL5OHPUACEIJSCLNRF2".to_string()),
            ..Default::default()
        };
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "ASIN" && v == "B0CPJ2B88T")
        );
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "content_id" && v == "GPAAHSEAGDCDOFL5OHPUACEIJSCLNRF2")
        );

        // content_id present, ASIN absent: typical PDOC sideload from an
        // EPUB without a catalogue ASIN.
        let ctx_pdoc = MetadataContext {
            content_id: Some("GPAAHSEAGDCDOFL5OHPUACEIJSCLNRF2".to_string()),
            ..Default::default()
        };
        let entries_pdoc = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx_pdoc);
        assert!(!entries_pdoc.iter().any(|(k, _)| *k == "ASIN"));
        assert!(
            entries_pdoc
                .iter()
                .any(|(k, v)| *k == "content_id" && v == "GPAAHSEAGDCDOFL5OHPUACEIJSCLNRF2")
        );

        // Both absent (no identifier at all).
        let ctx_empty = MetadataContext::default();
        let entries_empty =
            build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx_empty);
        assert!(!entries_empty.iter().any(|(k, _)| *k == "ASIN"));
        assert!(!entries_empty.iter().any(|(k, _)| *k == "content_id"));
    }

    #[test]
    fn test_kindle_capability_category_string() {
        assert_eq!(
            MetadataCategory::KindleCapability.as_str(),
            "kindle_capability_metadata"
        );
    }

    #[test]
    fn test_build_entries_with_asset_id_and_book_id() {
        let meta = Metadata {
            title: "Test".to_string(),
            language: "en".to_string(),
            identifier: "urn:uuid:test-id".to_string(),
            ..Default::default()
        };

        let ctx = MetadataContext {
            asset_id: Some("CR!ABCDEFGHIJKLMNOPQRSTUVWXYZ12"),
            book_id: Some("BtestBookId12345678901".to_string()),
            ..Default::default()
        };
        let entries = build_category_entries(MetadataCategory::KindleTitle, &meta, &ctx);

        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "asset_id" && v == "CR!ABCDEFGHIJKLMNOPQRSTUVWXYZ12")
        );
        assert!(
            entries
                .iter()
                .any(|(k, v)| *k == "book_id" && v == "BtestBookId12345678901")
        );
    }
}
