//! Attribute transformers for bidirectional value conversion.

use std::collections::HashMap;
use std::fmt::Debug;

use crate::formats::kfx::context::ResourceRegistry;

/// Context provided during import transformation.
#[derive(Debug, Default)]
pub struct ImportContext<'a> {
    /// Current chapter/section ID if known.
    pub chapter_id: Option<&'a str>,
    /// Anchor map: anchor_name → uri (for resolving external links).
    pub anchors: Option<&'a HashMap<String, String>>,
}

/// Context provided during export transformation.
#[derive(Debug, Default)]
pub struct ExportContext<'a> {
    /// Spine map for resolving chapter references to positions.
    pub spine_map: Option<&'a std::collections::HashMap<String, u32>>,
    /// Resource registry for looking up short resource names.
    pub resource_registry: Option<&'a ResourceRegistry>,
}

/// Result of parsing an attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedAttribute {
    /// A simple string value (passthrough).
    String(String),
    /// A parsed link target.
    Link(LinkData),
    /// An anchor/fragment ID.
    Anchor(String),
}

/// Parsed link data that can represent various link types.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkData {
    /// External URL (http://, https://, mailto:, etc.)
    External(String),
    /// Internal reference by ID/anchor.
    Internal(String),
    /// Kindle position-based link (fid:off format).
    KindlePosition {
        /// Fragment ID (base32 encoded in raw format).
        fid: u32,
        /// Byte offset within fragment.
        offset: u32,
    },
}

impl LinkData {
    /// Convert to a string suitable for IR storage.
    pub fn to_href(&self) -> String {
        match self {
            LinkData::External(url) => url.clone(),
            LinkData::Internal(id) => format!("#{}", id),
            LinkData::KindlePosition { fid, offset } => {
                // Store as a normalized internal format
                format!("kindle:fid:{}:off:{}", fid, offset)
            }
        }
    }
}

/// Trait for bidirectional attribute value transformation.
pub trait AttributeTransform: Send + Sync + Debug {
    /// Import: Convert raw KFX value to structured data.
    fn import(&self, raw_value: &str, context: &ImportContext) -> ParsedAttribute;

    /// Export: Convert structured data back to raw KFX value.
    fn export(&self, data: &ParsedAttribute, context: &ExportContext) -> String;

    /// Clone this transformer into a boxed trait object.
    fn clone_box(&self) -> Box<dyn AttributeTransform>;
}

impl Clone for Box<dyn AttributeTransform> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// ============================================================================

/// Identity transformer: passes values through unchanged.
#[derive(Debug, Clone)]
pub struct IdentityTransform;

impl AttributeTransform for IdentityTransform {
    fn import(&self, raw_value: &str, _context: &ImportContext) -> ParsedAttribute {
        ParsedAttribute::String(raw_value.to_string())
    }

    fn export(&self, data: &ParsedAttribute, _context: &ExportContext) -> String {
        match data {
            ParsedAttribute::String(s) => s.clone(),
            ParsedAttribute::Link(link) => link.to_href(),
            ParsedAttribute::Anchor(id) => id.clone(),
        }
    }

    fn clone_box(&self) -> Box<dyn AttributeTransform> {
        Box::new(self.clone())
    }
}

/// Transformer for KFX link references.
///
/// Handles conversion between:
#[derive(Debug, Clone)]
pub struct KfxLinkTransform;

impl AttributeTransform for KfxLinkTransform {
    fn import(&self, raw_value: &str, context: &ImportContext) -> ParsedAttribute {
        let link = parse_kfx_link(raw_value, context.anchors);
        ParsedAttribute::Link(link)
    }

    fn export(&self, data: &ParsedAttribute, _context: &ExportContext) -> String {
        match data {
            ParsedAttribute::Link(link) => encode_kfx_link(link),
            ParsedAttribute::String(s) => s.clone(),
            ParsedAttribute::Anchor(id) => id.clone(),
        }
    }

    fn clone_box(&self) -> Box<dyn AttributeTransform> {
        Box::new(self.clone())
    }
}

/// Parse a KFX link value into structured data.
///
/// If an anchor map is provided, anchor names are resolved to external URIs.
fn parse_kfx_link(raw: &str, anchors: Option<&HashMap<String, String>>) -> LinkData {
    // Check for Kindle position format: kindle:pos:fid:XXXX:off:YYYYYYYY
    if raw.starts_with("kindle:pos:fid:")
        && let Some(link) = parse_kindle_position(raw)
    {
        return link;
    }

    // Check for external URLs (already resolved)
    if raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw.starts_with("mailto:")
        || raw.starts_with("tel:")
    {
        return LinkData::External(raw.to_string());
    }

    // Check anchor map for external URI resolution
    if let Some(anchor_map) = anchors
        && let Some(uri) = anchor_map.get(raw)
    {
        return LinkData::External(uri.clone());
    }

    // Default: treat as internal anchor reference
    LinkData::Internal(raw.to_string())
}

/// Parse kindle:pos:fid:XXXX:off:YYYYYYYY format.
fn parse_kindle_position(raw: &str) -> Option<LinkData> {
    // Format: kindle:pos:fid:XXXX:off:YYYYYYYY

    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() < 6 {
        return None;
    }

    // parts[0] = "kindle"

    if parts[2] != "fid" || parts[4] != "off" {
        return None;
    }

    let fid = decode_base32(parts[3])?;
    let offset = u32::from_str_radix(parts[5], 16).ok()?;

    Some(LinkData::KindlePosition { fid, offset })
}

/// Decode a base32 string to u32 (Kindle's variant).
fn decode_base32(s: &str) -> Option<u32> {
    // Kindle uses a custom base32 alphabet: 0-9, A-V (case insensitive)
    let mut result: u32 = 0;
    for c in s.chars() {
        let digit = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'A'..='V' => c as u32 - 'A' as u32 + 10,
            'a'..='v' => c as u32 - 'a' as u32 + 10,
            _ => return None,
        };
        result = result.checked_mul(32)?.checked_add(digit)?;
    }
    Some(result)
}

/// Encode a u32 to base32 string (Kindle's variant).
pub fn encode_base32(mut value: u32, min_digits: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";
    let mut result = Vec::new();

    if value == 0 {
        result.push(b'0');
    } else {
        while value > 0 {
            result.push(ALPHABET[(value % 32) as usize]);
            value /= 32;
        }
    }

    // Pad to minimum digits
    while result.len() < min_digits {
        result.push(b'0');
    }

    result.reverse();
    String::from_utf8(result).unwrap()
}

/// Encode LinkData back to KFX format.
fn encode_kfx_link(link: &LinkData) -> String {
    match link {
        LinkData::External(url) => url.clone(),
        LinkData::Internal(id) => id.clone(),
        LinkData::KindlePosition { fid, offset } => {
            format!(
                "kindle:pos:fid:{}:off:{:08X}",
                encode_base32(*fid, 4),
                offset
            )
        }
    }
}

/// Transformer for image resource references.
///
/// On import: passes through the short resource name (e.g., "e6")
#[derive(Debug, Clone)]
pub struct ResourceTransform;

impl AttributeTransform for ResourceTransform {
    fn import(&self, raw_value: &str, _context: &ImportContext) -> ParsedAttribute {
        // Resource names are typically symbol IDs resolved to strings
        ParsedAttribute::String(raw_value.to_string())
    }

    fn export(&self, data: &ParsedAttribute, context: &ExportContext) -> String {
        match data {
            ParsedAttribute::String(s) => context
                .resource_registry
                .and_then(|registry| registry.get_name(s))
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        }
    }

    fn clone_box(&self) -> Box<dyn AttributeTransform> {
        Box::new(self.clone())
    }
}

// ============================================================================

use crate::formats::kfx::symbols::KfxSymbol;
use crate::util::MediaFormat;

/// Convert a MediaFormat to the corresponding KFX symbol ID.
pub fn format_to_kfx_symbol(format: MediaFormat) -> u64 {
    match format {
        MediaFormat::Jpeg => KfxSymbol::Jpg as u64,
        MediaFormat::Png => KfxSymbol::Png as u64,
        MediaFormat::Gif => KfxSymbol::Gif as u64,
        MediaFormat::Jxr => KfxSymbol::Jxr as u64,
        // SVG and WebP are rasterized to JPEG before bundling, so the symbol is
        MediaFormat::Svg => KfxSymbol::Jpg as u64,
        MediaFormat::WebP => KfxSymbol::Jpg as u64,
        MediaFormat::Ttf => KfxSymbol::Jpg as u64,
        MediaFormat::Otf => KfxSymbol::Jpg as u64,
        MediaFormat::Binary => KfxSymbol::Jpg as u64,
    }
}

// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A path the book holds an asset for exports as its short name; a path it
    /// holds none for exports as the empty string.
    #[test]
    fn a_resource_name_is_only_given_for_an_asset_the_book_holds() {
        let mut registry = ResourceRegistry::new();
        let mut symbols = crate::formats::kfx::context::SymbolTable::new();
        registry.register("images/cover.jpg", &mut symbols);
        let name = registry.get_or_create_name("images/cover.jpg");

        let context = ExportContext {
            spine_map: None,
            resource_registry: Some(&registry),
        };
        let held = ParsedAttribute::String("images/cover.jpg".to_string());
        let absent = ParsedAttribute::String("images/plate.png".to_string());

        assert_eq!(ResourceTransform.export(&held, &context), name);
        assert_eq!(ResourceTransform.export(&absent, &context), "");
    }

    #[test]
    fn test_decode_base32() {
        assert_eq!(decode_base32("0"), Some(0));
        assert_eq!(decode_base32("1"), Some(1));
        assert_eq!(decode_base32("A"), Some(10));
        assert_eq!(decode_base32("V"), Some(31));
        assert_eq!(decode_base32("10"), Some(32));
        assert_eq!(decode_base32("0001"), Some(1));
    }

    #[test]
    fn test_encode_base32() {
        assert_eq!(encode_base32(0, 1), "0");
        assert_eq!(encode_base32(1, 4), "0001");
        assert_eq!(encode_base32(32, 2), "10");
        assert_eq!(encode_base32(31, 1), "V");
    }

    #[test]
    fn test_parse_kindle_position() {
        let raw = "kindle:pos:fid:0001:off:0000012A";
        let link = parse_kfx_link(raw, None);

        assert_eq!(
            link,
            LinkData::KindlePosition {
                fid: 1,
                offset: 0x12A
            }
        );
    }

    #[test]
    fn test_encode_kindle_position() {
        let link = LinkData::KindlePosition {
            fid: 1,
            offset: 0x12A,
        };
        let encoded = encode_kfx_link(&link);
        assert_eq!(encoded, "kindle:pos:fid:0001:off:0000012A");
    }

    #[test]
    fn test_roundtrip_kindle_position() {
        let original = "kindle:pos:fid:0001:off:0000012A";
        let parsed = parse_kfx_link(original, None);
        let encoded = encode_kfx_link(&parsed);
        assert_eq!(original, encoded);
    }

    #[test]
    fn test_parse_external_url() {
        assert_eq!(
            parse_kfx_link("https://example.com", None),
            LinkData::External("https://example.com".to_string())
        );
        assert_eq!(
            parse_kfx_link("mailto:test@example.com", None),
            LinkData::External("mailto:test@example.com".to_string())
        );
    }

    #[test]
    fn test_parse_internal_anchor() {
        assert_eq!(
            parse_kfx_link("chapter2", None),
            LinkData::Internal("chapter2".to_string())
        );
    }

    #[test]
    fn test_parse_anchor_with_uri() {
        let mut anchors = HashMap::new();
        anchors.insert("a17H".to_string(), "https://example.com".to_string());

        // With anchor map, anchor name resolves to external URL
        assert_eq!(
            parse_kfx_link("a17H", Some(&anchors)),
            LinkData::External("https://example.com".to_string())
        );

        // Without anchor map, same anchor name is internal
        assert_eq!(
            parse_kfx_link("a17H", None),
            LinkData::Internal("a17H".to_string())
        );
    }

    #[test]
    fn test_kfx_link_transform() {
        let transform = KfxLinkTransform;
        let ctx = ImportContext::default();

        let parsed = transform.import("kindle:pos:fid:0001:off:0000012A", &ctx);
        assert!(matches!(
            parsed,
            ParsedAttribute::Link(LinkData::KindlePosition { .. })
        ));

        let export_ctx = ExportContext::default();
        let exported = transform.export(&parsed, &export_ctx);
        assert_eq!(exported, "kindle:pos:fid:0001:off:0000012A");
    }

    // ========================================================================

    #[test]
    fn test_format_to_kfx_symbol() {
        use super::format_to_kfx_symbol;
        use crate::formats::kfx::symbols::KfxSymbol;
        use crate::util::MediaFormat;

        assert_eq!(
            format_to_kfx_symbol(MediaFormat::Jpeg),
            KfxSymbol::Jpg as u64
        );
        assert_eq!(
            format_to_kfx_symbol(MediaFormat::Png),
            KfxSymbol::Png as u64
        );
        assert_eq!(
            format_to_kfx_symbol(MediaFormat::Gif),
            KfxSymbol::Gif as u64
        );
    }

    #[test]
    fn test_format_classification() {
        use crate::util::MediaFormat;

        assert!(MediaFormat::Jpeg.is_image());
        assert!(MediaFormat::Png.is_image());
        assert!(MediaFormat::Gif.is_image());
        assert!(MediaFormat::Svg.is_image());
        assert!(MediaFormat::WebP.is_image());
        assert!(!MediaFormat::Ttf.is_image());
        assert!(!MediaFormat::Binary.is_image());

        assert!(MediaFormat::Ttf.is_font());
        assert!(MediaFormat::Otf.is_font());
        assert!(!MediaFormat::Jpeg.is_font());
    }
}
