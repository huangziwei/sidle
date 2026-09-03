//! Pull the declared cover image out of a KFX container, in memory.

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::jxr as transcode;
use crate::formats::kfx::loader;
use crate::formats::kfx::symbols::KfxSymbol;

/// Extract the declared cover's `(bytes, extension)` from an in-memory KFX.
pub fn kfx_extract_cover(kfx_bytes: &[u8]) -> Result<Option<(Vec<u8>, &'static str)>, KfxError> {
    let book = loader::load(kfx_bytes)?;
    let Some(cover_name) = book.metadata.cover_resource_name.clone() else {
        return Ok(None); // no cover declared — coverless book
    };
    let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return Ok(None);
    };

    // Find the external_resource whose resource_name matches the declared cover and
    // read its `location` + `format`. Match on the field: `by_type` keys on fid.
    let mut location: Option<String> = None;
    let mut format: Option<String> = None;
    for v in resources.values() {
        let Some(fields) = v.unwrap_annotated().as_struct() else {
            continue;
        };
        let rn =
            get_field(fields, KfxSymbol::ResourceName as u64).and_then(|x| book.symbols.text_of(x));
        if rn != Some(cover_name.as_str()) {
            continue;
        }
        location = get_field(fields, KfxSymbol::Location as u64)
            .and_then(IonValue::as_string)
            .map(str::to_string);
        format = get_field(fields, KfxSymbol::Format as u64)
            .and_then(|x| book.symbols.text_of(x))
            .map(str::to_string);
        break;
    }
    let Some(location) = location else {
        return Ok(None);
    };
    let Some(raw) = book.raw_media.get(&location) else {
        return Ok(None);
    };

    // JPEG-XR → JPEG (the same transcode the EPUB resource pass runs); other
    // formats pass through. Detect JXR by the declared format or the II-BC magic.
    let is_jxr = format.as_deref() == Some("jxr") || raw.starts_with(&[0x49, 0x49, 0xBC]);
    if is_jxr {
        let (bytes, final_format, _timing) = transcode::transcode(raw, &cover_name)
            .map_err(|e| KfxError::JxrDecode(e.to_string()))?;
        // `transcode` passes the original bytes through with format "jxr" on a
        // decode failure; an undisplayable JXR sidecar is no better than none.
        if final_format == "jxr" {
            return Ok(None);
        }
        return Ok(Some((bytes, "jpg")));
    }

    Ok(Some((
        raw.clone(),
        ext_for(format.as_deref().unwrap_or(""), raw),
    )))
}

/// Map a KFX `format` symbol (or, when absent, the file magic) to a sidecar
/// extension. Defaults to `jpg` — covers are overwhelmingly JPEG and the sidecar
/// is only a hint for the picker/thumbnail step.
fn ext_for(format: &str, bytes: &[u8]) -> &'static str {
    match format {
        "jpg" | "jpeg" => "jpg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        _ => sniff_ext(bytes),
    }
}

fn sniff_ext(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return "jpg";
    }
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return "png";
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return "gif";
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "webp";
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return "bmp";
    }
    "jpg"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_cover_from_reflowable_kfx() {
        // A real EPUB→KFX (reflowable) carries the EPUB's cover; extraction must
        // return displayable image bytes with a sane extension.
        use crate::Book;
        use crate::export::{Exporter, KfxExporter};
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        KfxExporter::new().export(&mut book, &mut buf).unwrap();
        let kfx = buf.into_inner();

        let (bytes, ext) = kfx_extract_cover(&kfx)
            .expect("valid KFX")
            .expect("reflowable KFX has a built-in cover");
        assert!(!bytes.is_empty(), "cover bytes must be non-empty");
        // The fixture's cover is a JPEG; after any transcode the sidecar is jpg.
        assert_eq!(ext, "jpg");
        assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "cover must be a JPEG");
    }

    #[test]
    fn extracts_cover_from_fixture_kfx_matching_cover_replace() {
        // The committed KFX fixture declares a cover at `resource/rsrc7`; the
        // extractor must return the same backing bytes `cover_replace` resolves.
        let kfx = std::fs::read("tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx")
            .expect("read fixture");
        let book = loader::load(&kfx).expect("load fixture");
        let cover_name = book
            .metadata
            .cover_resource_name
            .clone()
            .expect("fixture has a cover");
        let resources = book
            .by_type
            .get(&(KfxSymbol::ExternalResource as u64))
            .unwrap();
        let mut want: Option<Vec<u8>> = None;
        for v in resources.values() {
            let fields = v.unwrap_annotated().as_struct().unwrap();
            let rn = get_field(fields, KfxSymbol::ResourceName as u64)
                .and_then(|x| book.symbols.text_of(x));
            if rn == Some(cover_name.as_str()) {
                let loc = get_field(fields, KfxSymbol::Location as u64)
                    .and_then(IonValue::as_string)
                    .unwrap();
                want = book.raw_media.get(loc).cloned();
                break;
            }
        }
        let want = want.expect("fixture cover has backing bytes");

        let (got, _ext) = kfx_extract_cover(&kfx).unwrap().unwrap();
        // Fixture cover is a plain JPEG (no JXR), so extraction is byte-exact.
        assert_eq!(
            got, want,
            "extracted cover must be the declared resource bytes"
        );
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn extracts_cover_from_pdf_backed_kfx() {
        // `pdf_to_kfx` embeds the cover the same way the
        // reflowable path does (book_metadata.cover_image → external_resource →
        // bcRawMedia), so the extractor must return that embedded JPEG verbatim.
        use crate::export::{PdfKfxMeta, pdf_to_kfx};
        use crate::formats::pdf::structure::{PdfDoc, PdfPage};

        let pdf_bytes = b"%PDF-1.4\n% fixture\n%%EOF\n".to_vec();
        let doc = PdfDoc {
            bytes: pdf_bytes,
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("Backed".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "Backed".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        // A stand-in JPEG cover (magic + EOI). `pdf_to_kfx` declares it
        // `format: jpg`, so the extractor passes it through without transcoding.
        let cover = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 0xFF, 0xD9];
        let kfx = pdf_to_kfx(&doc, &meta, Some(&cover), None).expect("pdf_to_kfx");
        assert!(
            crate::formats::kfx::pdf_container::kfx_is_pdf_backed(&kfx),
            "fixture must be PDF-backed"
        );

        let (bytes, ext) = kfx_extract_cover(&kfx)
            .expect("valid KFX")
            .expect("PDF-backed KFX has an embedded cover");
        assert_eq!(ext, "jpg");
        assert_eq!(
            bytes, cover,
            "extracted cover must be the embedded JPEG, verbatim"
        );
    }

    #[cfg(feature = "pdf")]
    #[test]
    fn pdf_backed_without_cover_is_none() {
        // A PDF-backed KFX built with no cover declares no cover_image; the
        // extractor returns Ok(None), not an error.
        use crate::export::{PdfKfxMeta, pdf_to_kfx};
        use crate::formats::pdf::structure::{PdfDoc, PdfPage};

        let doc = PdfDoc {
            bytes: b"%PDF-1.4\n%%EOF\n".to_vec(),
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("NoCover".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "NoCover".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");
        assert_eq!(kfx_extract_cover(&kfx).expect("valid KFX"), None);
    }

    #[test]
    fn non_kfx_bytes_error() {
        assert!(kfx_extract_cover(b"not a kfx container").is_err());
    }
}
