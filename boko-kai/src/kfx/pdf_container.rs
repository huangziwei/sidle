//! Detect and extract PDF-backed (container) KFX — the return leg of the
//! PDF↔KFX dual format. See `.claude/plans/pdf-to-kfx.md`.
//!
//! A PDF-backed KFX (Amazon "Send to Kindle" PDF, or boko's `pdf_to_kfx`
//! output) embeds the source PDF verbatim as a single `bcRawMedia` and points
//! at it from per-page `external_resource` fragments with `format: pdf`. Such a
//! KFX must round-trip through **PDF**, not EPUB — running `kfx_to_epub` on one
//! would mangle a PDF into reflowed text. Because the bytes are embedded
//! verbatim, extraction is exact: `pdf → kfx → pdf` reproduces the original.

use super::container::{
    EntityLoc, entity_media, parse_container_header, parse_container_info, parse_entity,
    parse_index_table,
};
use super::ion::IonValue;
use super::symbols::KfxSymbol;

/// PDF file magic.
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Why a KFX could not be extracted to a PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfExtractError {
    /// Not a parseable KFX container.
    NotKfx,
    /// A KFX, but not PDF-backed (no `external_resource` with `format: pdf`).
    NotPdfBacked,
    /// PDF-backed, but no embedded PDF blob was found (corrupt container).
    NoPdfBlob,
    /// Multiple embedded PDF blobs (per-page slices) — not handled. We
    /// detect-and-raise rather than silently pick one.
    MultipleSlices(usize),
}

impl std::fmt::Display for PdfExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PdfExtractError::NotKfx => write!(f, "not a valid KFX container"),
            PdfExtractError::NotPdfBacked => {
                write!(f, "KFX is not PDF-backed (no PDF resource) — convert to EPUB instead")
            }
            PdfExtractError::NoPdfBlob => {
                write!(f, "KFX declares a PDF resource but no embedded PDF was found")
            }
            PdfExtractError::MultipleSlices(n) => {
                write!(f, "KFX embeds {n} separate PDF blobs (per-page slices); not supported")
            }
        }
    }
}

impl std::error::Error for PdfExtractError {}

/// Parse the container into its entity table, or `None` if it isn't a KFX.
fn entities(kfx: &[u8]) -> Option<Vec<EntityLoc>> {
    let header = parse_container_header(kfx).ok()?;
    let ci_end = header
        .container_info_offset
        .checked_add(header.container_info_length)?;
    if ci_end > kfx.len() {
        return None;
    }
    let info = parse_container_info(&kfx[header.container_info_offset..ci_end]).ok()?;
    let (idx_off, idx_len) = info.index?;
    let idx_end = idx_off.checked_add(idx_len)?;
    if idx_end > kfx.len() {
        return None;
    }
    Some(parse_index_table(&kfx[idx_off..idx_end], header.header_len))
}

/// True if an `external_resource` fragment declares `format: pdf`.
fn is_pdf_resource(v: &IonValue) -> bool {
    v.unwrap_annotated()
        .get(KfxSymbol::Format as u64)
        .and_then(|f| f.as_symbol())
        == Some(KfxSymbol::Pdf as u64)
}

/// Whether `kfx` is a PDF-backed container: it has at least one
/// `external_resource` with `format: pdf` (calibre's `has_pdf_resource`).
///
/// This is the routing signal — a `true` here means the canonical sibling
/// format is PDF, and KFX→EPUB conversion must be skipped.
pub fn kfx_is_pdf_backed(kfx: &[u8]) -> bool {
    let Some(ents) = entities(kfx) else {
        return false;
    };
    let ext_type = KfxSymbol::ExternalResource as u32;
    ents.iter()
        .filter(|e| e.type_id == ext_type)
        .any(|e| parse_entity(kfx, e).as_ref().is_some_and(is_pdf_resource))
}

/// Extract the embedded PDF from a PDF-backed KFX, verbatim.
///
/// Returns the original PDF bytes (byte-identical to what was embedded). Errors
/// if the KFX isn't PDF-backed or the blob can't be located unambiguously.
pub fn kfx_extract_pdf(kfx: &[u8]) -> Result<Vec<u8>, PdfExtractError> {
    let ents = entities(kfx).ok_or(PdfExtractError::NotKfx)?;

    let ext_type = KfxSymbol::ExternalResource as u32;
    let pdf_backed = ents
        .iter()
        .filter(|e| e.type_id == ext_type)
        .any(|e| parse_entity(kfx, e).as_ref().is_some_and(is_pdf_resource));
    if !pdf_backed {
        return Err(PdfExtractError::NotPdfBacked);
    }

    // The PDF is stored as a `bcRawMedia` blob; for our output and Amazon's S2K
    // it's a single whole-PDF entity. Locate it by its `%PDF-` magic, which is
    // robust across symbol-naming differences between encoders.
    let raw_type = KfxSymbol::Bcrawmedia as u32;
    let mut pdfs = ents
        .iter()
        .filter(|e| e.type_id == raw_type)
        .filter_map(|e| entity_media(kfx, e))
        .filter(|m| m.starts_with(PDF_MAGIC));

    let first = pdfs.next().ok_or(PdfExtractError::NoPdfBlob)?;
    let extra = pdfs.count();
    if extra > 0 {
        return Err(PdfExtractError::MultipleSlices(extra + 1));
    }
    Ok(first.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{PdfKfxMeta, pdf_to_kfx};
    use crate::import::pdf::{PdfDoc, PdfPage};

    /// A byte string that passes the `%PDF-` magic check. The round-trip gate
    /// is about *embedding fidelity*, so we embed these bytes directly via a
    /// hand-built `PdfDoc` (bypassing `probe_pdf`) and assert we get them back.
    fn fake_pdf() -> Vec<u8> {
        let mut v = b"%PDF-1.4\n% round-trip fixture\n".to_vec();
        v.extend_from_slice(&[0u8, 1, 2, 3, 255, 254, 253]); // arbitrary binary
        v.extend_from_slice(b"\n%%EOF\n");
        v
    }

    #[test]
    fn pdf_to_kfx_to_pdf_is_byte_identical() {
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![
                PdfPage { width: 612.0, height: 792.0 },
                PdfPage { width: 595.0, height: 842.0 },
            ],
            title: Some("Round Trip".to_string()),
            author: Some("Tester".to_string()),
        };
        let meta = PdfKfxMeta {
            title: doc.title.clone().unwrap(),
            author: doc.author.clone(),
            language: "en".to_string(),
        };

        // No cover here: it doesn't affect the embedded-PDF extraction we test
        // (rendering needs pdfium, which isn't available in unit tests).
        let kfx = pdf_to_kfx(&doc, &meta, None);
        assert!(kfx_is_pdf_backed(&kfx), "boko PDF KFX must be detected as PDF-backed");

        let extracted = kfx_extract_pdf(&kfx).expect("extraction should succeed");
        assert_eq!(extracted, bytes, "extracted PDF must be byte-identical to the source");
    }

    #[test]
    fn cover_jpeg_does_not_break_pdf_extraction() {
        // With a cover, the KFX has *two* bcRawMedia blobs (the PDF and the
        // cover JPEG). `kfx_extract_pdf` must still return the PDF, picking it by
        // the `%PDF-` magic and skipping the JPEG — not raise `MultipleSlices`.
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![PdfPage { width: 612.0, height: 792.0 }],
            title: Some("With Cover".to_string()),
            author: None,
        };
        let meta = PdfKfxMeta {
            title: "With Cover".to_string(),
            author: None,
            language: "en".to_string(),
        };

        // A stand-in cover blob with JPEG magic (real rendering needs pdfium).
        let cover = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 0xFF, 0xD9];
        let kfx = pdf_to_kfx(&doc, &meta, Some(&cover));

        assert!(kfx_is_pdf_backed(&kfx), "still PDF-backed with a cover");
        let extracted = kfx_extract_pdf(&kfx).expect("extraction should succeed past the cover");
        assert_eq!(extracted, bytes, "extracted PDF must skip the cover JPEG and be exact");
        assert!(
            kfx.windows(cover.len()).any(|w| w == cover.as_slice()),
            "the cover JPEG must be embedded verbatim in the KFX"
        );
    }

    #[test]
    fn garbage_is_not_pdf_backed() {
        assert!(!kfx_is_pdf_backed(b"not a kfx at all"));
        assert_eq!(kfx_extract_pdf(b"not a kfx"), Err(PdfExtractError::NotKfx));
    }

    #[test]
    fn reflowable_kfx_is_not_pdf_backed() {
        // A real EPUB→KFX (reflowable) must NOT be flagged as PDF-backed, so
        // Sidle/boko still route it through EPUB.
        use crate::Book;
        use crate::export::{Exporter, KfxExporter};
        let mut book = Book::open("tests/fixtures/[太宰 治] 人間失格.epub").unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        KfxExporter::new().export(&mut book, &mut buf).unwrap();
        let kfx = buf.into_inner();
        assert!(!kfx_is_pdf_backed(&kfx));
        assert_eq!(kfx_extract_pdf(&kfx), Err(PdfExtractError::NotPdfBacked));
    }
}
