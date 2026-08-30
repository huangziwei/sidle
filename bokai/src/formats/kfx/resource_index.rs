//! Image resource indexing shared by every KFX→EPUB route.

use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;

/// Image format strings KFX may set on `external_resource.format`.
pub const FORMAT_JPG: &str = "jpg";
pub const FORMAT_PNG: &str = "png";
pub const FORMAT_GIF: &str = "gif";
pub const FORMAT_WEBP: &str = "webp";
pub const FORMAT_BMP: &str = "bmp";
pub const FORMAT_SVG: &str = "svg";
pub const FORMAT_JXR: &str = "jxr";

/// `format` values a cover can be: raster only, excluding `pdf`, `kvg`,
/// `svg` and anything unrecognised.
pub const RASTER_COVER_FORMATS: [&str; 7] = ["jpg", "jpeg", "jxr", "png", "gif", "webp", "bmp"];

/// Every `format` value an `external_resource` states: the seven raster
/// formats plus `svg`, and `pdf`/`kvg` for non-raster page content.
pub const DECLARED_FORMATS: [&str; 10] = [
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "jxr", "pdf", "kvg",
];

/// One image `external_resource`, resolved to its final exported identity.
#[derive(Debug, Clone)]
pub struct ImageResource {
    /// KFX `external_resource.resource_name` (e.g. "content_30", "eF") —
    /// what storyline image elements reference.
    pub resource_name: String,
    /// `location` field: the `bcRawMedia` key holding the source bytes
    /// (e.g. "resource/rsrc562").
    pub location: String,
    /// File path under `OEBPS/` (e.g. "image_rsrc562.jpg"). [`cover_filename`]
    /// renames a cover to `cover.<ext>` in a separate step.
    pub filename: String,
    /// Predicted final MIME type (JXR predicts `image/jpeg`; a JXR that
    /// fails to decode passes through as `image/jxr` at transcode time).
    pub mime: String,
    /// KFX `format` field as declared ("" when absent). `mime` and `filename`
    /// fold in byte sniffing; this one does not.
    pub declared_format: String,
    /// Declared pixel dimensions when present.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// True iff the source bytes are JPEG-XR (by `format` or file magic).
    pub is_jxr: bool,
}

/// Fragment id for an entity: its id's resolved symbol name, or a distinct
/// `#entity_N` fallback. Resource processing sorts on this string.
pub fn entity_fid(entity_id: u64, symbols: &SymbolTable) -> String {
    let name = symbols.resolve(entity_id);
    if name.is_empty() || name == "?" {
        format!("#entity_{entity_id}")
    } else {
        name.to_string()
    }
}

/// The canonical image list, sorted by fid, from the unsorted `(fid,
/// fragment)` set. `peek_raw` maps a `location` to ≥ 12 leading `bcRawMedia`
/// bytes for sniffing; its `None` skips the resource with a warning.
pub fn build_image_index<'a, F>(
    resources: Vec<(&'a str, &'a IonValue)>,
    symbols: &SymbolTable,
    mut peek_raw: F,
) -> Vec<ImageResource>
where
    F: FnMut(&str) -> Option<Vec<u8>>,
{
    let mut sorted = resources;
    sorted.sort_by(|a, b| a.0.cmp(b.0));

    let mut images: Vec<ImageResource> = Vec::new();
    let mut used_names: Vec<String> = Vec::new();

    for (fid, raw) in sorted {
        let inner = raw.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            continue;
        };

        let resource_name = get_field(fields, KfxSymbol::ResourceName as u64)
            .and_then(|v| symbols.text_of(v))
            .map(|s| s.to_string())
            .unwrap_or_else(|| fid.to_string());

        let format_raw = get_field(fields, KfxSymbol::Format as u64)
            .and_then(|v| symbols.text_of(v))
            .map(|s| s.to_string());

        let mime_raw = get_field(fields, KfxSymbol::Mime as u64)
            .and_then(|v| v.as_string())
            .map(|s| s.to_string());

        let Some(location) = get_field(fields, KfxSymbol::Location as u64)
            .and_then(|v| v.as_string())
            .map(|s| s.to_string())
        else {
            continue;
        };

        let width = get_field(fields, KfxSymbol::ResourceWidth as u64)
            .and_then(|v| v.as_int())
            .map(|n| n as u32);
        let height = get_field(fields, KfxSymbol::ResourceHeight as u64)
            .and_then(|v| v.as_int())
            .map(|n| n as u32);

        // Skip non-image formats (fonts arrive via the fonts pass).
        let format_str = format_raw.as_deref().unwrap_or("");
        if !is_image_format_symbol(format_str, mime_raw.as_deref()) {
            continue;
        }

        let Some(head) = peek_raw(&location) else {
            eprintln!("kfx image index: missing bcRawMedia at {location:?}");
            continue;
        };

        let is_jxr = format_str == FORMAT_JXR || sniff_format(&head).as_deref() == Some(FORMAT_JXR);

        // Predict the final format without decoding: a JXR will be transcoded
        // to JPEG; everything else passes through as sniffed, falling back to
        // the KFX `format` field.
        let final_format = if is_jxr {
            FORMAT_JPG.to_string()
        } else {
            sniff_format(&head).unwrap_or_else(|| format_str.to_string())
        };
        let final_mime = format_to_mime(&final_format);
        let filename = build_image_filename(&location, &final_format, |candidate| {
            used_names.iter().any(|n| n == candidate)
        });
        used_names.push(filename.clone());

        images.push(ImageResource {
            resource_name,
            location,
            filename,
            mime: final_mime,
            declared_format: format_str.to_string(),
            width,
            height,
            is_jxr,
        });
    }

    images
}

/// The cover's exported filename, derived from its canonical image filename:
/// `cover.<ext>`, with `jpg` widened to `jpeg`. The rename runs after every
/// image is registered, leaving collision-suffix allocation untouched.
pub fn cover_filename(canonical_filename: &str) -> String {
    let ext = std::path::Path::new(canonical_filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| if e == "jpg" { "jpeg" } else { e })
        .unwrap_or("jpeg");
    format!("cover.{ext}")
}

/// True if `name` names an image whose *declared* KFX format is a raster
/// type a cover can be. Guards the first-section cover fallback.
pub fn is_raster_cover(images: &[ImageResource], name: &str) -> bool {
    images.iter().any(|img| {
        img.resource_name == name && RASTER_COVER_FORMATS.contains(&img.declared_format.as_str())
    })
}

/// First `resource_name` ($175) found anywhere in a storyline content tree.
/// A cover storyline lays out exactly one image.
pub fn first_content_resource_name(value: &IonValue, symbols: &SymbolTable) -> Option<String> {
    match value.unwrap_annotated() {
        IonValue::List(items) => items
            .iter()
            .find_map(|it| first_content_resource_name(it, symbols)),
        IonValue::Struct(fields) => {
            if let Some(name) =
                get_field(fields, KfxSymbol::ResourceName as u64).and_then(|v| symbols.text_of(v))
            {
                return Some(name.to_string());
            }
            fields.iter().find_map(|(_, v)| {
                matches!(
                    v.unwrap_annotated(),
                    IonValue::List(_) | IonValue::Struct(_)
                )
                .then(|| first_content_resource_name(v, symbols))
                .flatten()
            })
        }
        _ => None,
    }
}

/// True when the `format` symbol or declared MIME marks an image resource.
pub fn is_image_format_symbol(format: &str, mime: Option<&str>) -> bool {
    matches!(
        format,
        FORMAT_JPG | FORMAT_PNG | FORMAT_GIF | FORMAT_WEBP | FORMAT_BMP | FORMAT_SVG | FORMAT_JXR
    ) || mime.is_some_and(|m| m.starts_with("image/"))
}

/// Detect image format from leading bytes (≥ 12 bytes decide every case).
/// The returned name is the bare extension, matching the KFX `format`
/// vocabulary.
pub fn sniff_format(bytes: &[u8]) -> Option<String> {
    crate::image::ImageFormat::sniff(bytes).map(|f| f.extension().to_string())
}

pub fn format_to_mime(format: &str) -> String {
    match format {
        FORMAT_JPG => "image/jpeg".into(),
        FORMAT_PNG => "image/png".into(),
        FORMAT_GIF => "image/gif".into(),
        FORMAT_WEBP => "image/webp".into(),
        FORMAT_BMP => "image/bmp".into(),
        FORMAT_SVG => "image/svg+xml".into(),
        FORMAT_JXR => "image/jxr".into(),
        _ => "application/octet-stream".into(),
    }
}

pub fn format_to_ext(format: &str) -> &'static str {
    match format {
        FORMAT_JPG => ".jpg",
        FORMAT_PNG => ".png",
        FORMAT_GIF => ".gif",
        FORMAT_WEBP => ".webp",
        FORMAT_BMP => ".bmp",
        FORMAT_SVG => ".svg",
        FORMAT_JXR => ".jxr",
        _ => ".bin",
    }
}

/// `"resource/rsrc562"` → `"image_rsrc562.jpg"`: the basename, the
/// resource-type prefix, the extension. `taken` reports a filename in use; a
/// collision appends `-0`, `-1`, ….
pub fn build_image_filename(
    location: &str,
    format: &str,
    mut taken: impl FnMut(&str) -> bool,
) -> String {
    let ext = format_to_ext(format);
    // Sanitise: only `[A-Za-z0-9_/.-]` survives; everything else → `_`.
    let safe: String = location
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '/' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Split path / name; pull the basename's root (no extension).
    let name = safe.rsplit('/').next().unwrap_or(&safe);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    // Unique part: the basename split above strips the `resource/` prefix.
    let unique = stem
        .strip_prefix("rsrc")
        .map(|r| format!("rsrc{r}"))
        .unwrap_or_else(|| stem.to_string());

    // Resource-type prefix: an image extension takes "image_<unique>".
    let prefixed = if unique.is_empty() {
        "image".to_string()
    } else {
        format!("image_{unique}")
    };

    let mut candidate = format!("{prefixed}{ext}");
    let mut n = 0;
    while taken(&candidate) {
        candidate = format!("{prefixed}-{n}{ext}");
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_resource_fragment(name_sym: u64, location: &str, format_sym: u64) -> IonValue {
        IonValue::Struct(vec![
            (KfxSymbol::ResourceName as u64, IonValue::Symbol(name_sym)),
            (
                KfxSymbol::Location as u64,
                IonValue::String(location.to_string()),
            ),
            (KfxSymbol::Format as u64, IonValue::Symbol(format_sym)),
            (KfxSymbol::ResourceWidth as u64, IonValue::Int(640)),
            (KfxSymbol::ResourceHeight as u64, IonValue::Int(480)),
        ])
    }

    #[test]
    fn filename_mirrors_calibre_location_convention() {
        assert_eq!(
            build_image_filename("resource/rsrc562", "jpg", |_| false),
            "image_rsrc562.jpg"
        );
        assert_eq!(
            build_image_filename("eF!", "png", |_| false),
            "image_eF_.png"
        );
        let mut taken = vec!["image_rsrc5.jpg".to_string()];
        let second =
            build_image_filename("resource/rsrc5", "jpg", |c| taken.iter().any(|t| t == c));
        assert_eq!(second, "image_rsrc5-0.jpg");
        taken.push(second);
        assert_eq!(
            build_image_filename("resource/rsrc5", "jpg", |c| taken.iter().any(|t| t == c)),
            "image_rsrc5-1.jpg"
        );
    }

    #[test]
    fn index_sorts_by_fid_predicts_jxr_and_skips_missing_media() {
        let symbols = SymbolTable::new(
            0,
            vec![
                "eA".into(),  // id 0
                "eB".into(),  // id 1
                "jpg".into(), // id 2
                "jxr".into(), // id 3
            ],
        );
        let frag_a = image_resource_fragment(0, "resource/rsrc2", 2);
        let frag_b = image_resource_fragment(1, "resource/rsrc10", 3);
        let frag_missing = image_resource_fragment(0, "resource/gone", 2);
        // Unsorted input; fid sort is lexicographic ("rsrc10" < "rsrc2").
        let resources = vec![
            ("rsrc2", &frag_a),
            ("rsrc10", &frag_b),
            ("rsrc_gone", &frag_missing),
        ];
        let jpeg_head = vec![0xFF, 0xD8, 0xFF, 0xE0];
        let jxr_head = vec![0x49, 0x49, 0xBC, 0x01];
        let images = build_image_index(resources, &symbols, |loc| match loc {
            "resource/rsrc2" => Some(jpeg_head.clone()),
            "resource/rsrc10" => Some(jxr_head.clone()),
            _ => None,
        });
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].filename, "image_rsrc10.jpg"); // JXR → predicted JPEG
        assert!(images[0].is_jxr);
        assert_eq!(images[0].mime, "image/jpeg");
        assert_eq!(images[1].filename, "image_rsrc2.jpg");
        assert!(!images[1].is_jxr);
        assert_eq!(images[1].resource_name, "eA");
        assert_eq!(images[1].width, Some(640));
    }

    #[test]
    fn cover_rename_widens_jpg_to_jpeg() {
        assert_eq!(cover_filename("image_rsrc1.jpg"), "cover.jpeg");
        assert_eq!(cover_filename("image_rsrc1.png"), "cover.png");
        assert_eq!(cover_filename("image_rsrc1"), "cover.jpeg");
        let images = vec![ImageResource {
            resource_name: "eF".into(),
            location: "resource/rsrc1".into(),
            filename: "image_rsrc1.jpg".into(),
            mime: "image/jpeg".into(),
            declared_format: "jxr".into(),
            width: None,
            height: None,
            is_jxr: true,
        }];
        assert!(is_raster_cover(&images, "eF"));
        assert!(!is_raster_cover(&images, "nope"));
    }

    #[test]
    fn first_resource_name_walks_nested_content() {
        let symbols = SymbolTable::new(0, vec!["eK".into()]);
        let tree = IonValue::List(vec![IonValue::Struct(vec![(
            KfxSymbol::ContentList as u64,
            IonValue::List(vec![IonValue::Struct(vec![(
                KfxSymbol::ResourceName as u64,
                IonValue::Symbol(0),
            )])]),
        )])]);
        assert_eq!(
            first_content_resource_name(&tree, &symbols),
            Some("eK".to_string())
        );
    }
}
