//! Detect and extract PDF-backed (container) KFX — the return leg of the
//! PDF↔KFX dual format.
//!
//! A PDF-backed KFX (Amazon "Send to Kindle" PDF, or bokai's `pdf_to_kfx`
//! output) embeds the source PDF verbatim as a single `bcRawMedia` and points
//! at it from per-page `external_resource` fragments with `format: pdf`. Such a
//! KFX round-trips through **PDF**, not EPUB: an EPUB conversion reflows a
//! page image into text. The embedded bytes are verbatim, and extraction is
//! exact: `pdf → kfx → pdf` reproduces the original.

use super::container::{
    EntityLoc, entity_media, parse_container_header, parse_container_info, parse_entity,
    parse_index_table, skip_enty_header,
};
use super::ion::{IonParser, IonValue};
use super::symbols::KfxSymbol;
use crate::io::ByteSource;

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
    /// Multiple embedded PDF blobs (per-page slices) — not handled. Raised
    /// on detection; no blob is picked.
    MultipleSlices(usize),
}

impl std::fmt::Display for PdfExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PdfExtractError::NotKfx => write!(f, "not a valid KFX container"),
            PdfExtractError::NotPdfBacked => {
                write!(
                    f,
                    "KFX is not PDF-backed (no PDF resource) — convert to EPUB instead"
                )
            }
            PdfExtractError::NoPdfBlob => {
                write!(
                    f,
                    "KFX declares a PDF resource but no embedded PDF was found"
                )
            }
            PdfExtractError::MultipleSlices(n) => {
                write!(
                    f,
                    "KFX embeds {n} separate PDF blobs (per-page slices); not supported"
                )
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
/// The routing signal: on `true` the canonical sibling format is PDF and
/// KFX→EPUB conversion is skipped.
pub fn kfx_is_pdf_backed(kfx: &[u8]) -> bool {
    let Some(ents) = entities(kfx) else {
        return false;
    };
    let ext_type = KfxSymbol::ExternalResource as u32;
    ents.iter()
        .filter(|e| e.type_id == ext_type)
        .any(|e| parse_entity(kfx, e).as_ref().is_some_and(is_pdf_resource))
}

/// [`kfx_is_pdf_backed`] over a random-access source, reading the container
/// header, its index table and the `external_resource` fragments alone —
/// kilobytes of a container whose embedded media runs to tens of megabytes.
pub fn source_is_pdf_backed(source: &dyn ByteSource) -> bool {
    let Some(ents) = source_entities(source) else {
        return false;
    };
    let ext_type = KfxSymbol::ExternalResource as u32;
    ents.iter().filter(|e| e.type_id == ext_type).any(|e| {
        source
            .read_at(e.offset as u64, e.length)
            .ok()
            .and_then(|raw| IonParser::new(skip_enty_header(&raw)).parse().ok())
            .as_ref()
            .is_some_and(is_pdf_resource)
    })
}

/// [`entities`] read through a byte source.
fn source_entities(source: &dyn ByteSource) -> Option<Vec<EntityLoc>> {
    // `parse_container_header` reads the fixed 18-byte `CONT` header.
    let head = source.read_at(0, 18).ok()?;
    let header = parse_container_header(&head).ok()?;
    let info_bytes = source
        .read_at(
            header.container_info_offset as u64,
            header.container_info_length,
        )
        .ok()?;
    let info = parse_container_info(&info_bytes).ok()?;
    let (idx_off, idx_len) = info.index?;
    let idx_bytes = source.read_at(idx_off as u64, idx_len).ok()?;
    Some(parse_index_table(&idx_bytes, header.header_len))
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

    // The PDF is stored as a `bcRawMedia` blob, one whole-PDF entity in both
    // `pdf_to_kfx` output and Amazon S2K. The `%PDF-` magic locates it across
    // encoders that name symbols differently.
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

#[cfg(all(test, feature = "pdf"))]
mod tests {
    use super::*;
    use crate::export::{PdfKfxMeta, pdf_to_kfx};
    use crate::formats::pdf::structure::{PdfDoc, PdfOutlineItem, PdfPage};

    /// A byte string that passes the `%PDF-` magic check. The round-trip gate
    /// is about *embedding fidelity*: a hand-built `PdfDoc` carries these bytes
    /// directly, bypassing `probe_pdf`, and the test asserts they come back.
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
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
                PdfPage {
                    width: 595.0,
                    height: 842.0,
                    rotation: 0,
                },
            ],
            title: Some("Round Trip".to_string()),
            author: Some("Tester".to_string()),
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: doc.title.clone().unwrap(),
            author: doc.author.clone(),
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };

        // No cover/text here: neither affects embedded-PDF extraction
        // (both need the PDFKit engine, exercised by the gitignored harness).
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");
        assert!(
            kfx_is_pdf_backed(&kfx),
            "bokai PDF KFX must be detected as PDF-backed"
        );

        let extracted = kfx_extract_pdf(&kfx).expect("extraction should succeed");
        assert_eq!(
            extracted, bytes,
            "extracted PDF must be byte-identical to the source"
        );
    }

    #[test]
    fn text_layer_adds_text_storyline_without_breaking_extraction() {
        use crate::formats::kfx::symbols::KfxSymbol;
        use crate::formats::pdf::render::{PageText, StyleSeg, TextRun};

        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("T".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "T".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        // One page, one run "THE FOX" → word / space / word.
        let text = vec![PageText {
            runs: vec![TextRun {
                content: "THE FOX".to_string(),
                left: 7000,
                top: 17000,
                width: 12000,
                height: 1700,
                baseline: 18700,
                words: vec![
                    StyleSeg {
                        offset: 0,
                        length: 3,
                        width: 4000,
                        is_word: true,
                    },
                    StyleSeg {
                        offset: 3,
                        length: 1,
                        width: 600,
                        is_word: false,
                    },
                    StyleSeg {
                        offset: 4,
                        length: 3,
                        width: 4200,
                        is_word: true,
                    },
                ],
            }],
        }];

        let with_text = pdf_to_kfx(&doc, &meta, None, Some(&text)).expect("pdf_to_kfx");
        let without = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");

        // The text layer must not disturb the embedded PDF (byte-identical out).
        assert!(kfx_is_pdf_backed(&with_text));
        assert_eq!(kfx_extract_pdf(&with_text).unwrap(), bytes);

        let count = |k: &[u8], t: u32| {
            entities(k)
                .unwrap()
                .iter()
                .filter(|e| e.type_id == t)
                .count()
        };
        let storyline = KfxSymbol::Storyline as u32;
        let aux = KfxSymbol::AuxiliaryData as u32;
        // Every page carries an image storyline and an invisible text
        // storyline, the run-less page's holding one empty page-sized
        // container. Amazon emits the same pair.
        assert_eq!(
            count(&without, storyline),
            2,
            "no-text page: image storyline + empty text storyline"
        );
        assert_eq!(
            count(&with_text, storyline),
            2,
            "text page: image + text storyline"
        );
        // auxiliary_data is one `<section>-ad` per page stating `page_rotation`,
        // and nothing else — a text layer adds none.
        assert_eq!(
            count(&with_text, aux),
            count(&without, aux),
            "the text layer adds no auxiliary_data"
        );
    }

    #[test]
    fn fixed_layout_position_maps_resolve_for_selection() {
        use crate::formats::kfx::container::parse_entity;
        use crate::formats::kfx::ion::IonValue;
        use crate::formats::kfx::symbols::KfxSymbol;
        use crate::formats::pdf::render::{PageText, StyleSeg, TextRun};

        let bytes = fake_pdf();
        let mk_run = |content: &str| TextRun {
            content: content.to_string(),
            left: 100,
            top: 100,
            width: 1000,
            height: 100,
            baseline: 180,
            words: vec![StyleSeg {
                offset: 0,
                length: content.encode_utf16().count(),
                width: 1000,
                is_word: true,
            }],
        };
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![
                PdfPage {
                    width: 600.0,
                    height: 800.0,
                    rotation: 0,
                },
                PdfPage {
                    width: 600.0,
                    height: 800.0,
                    rotation: 0,
                },
            ],
            title: Some("T".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "T".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        // Page 0: two runs ("hello"=5, "worldly"=7); page 1: no text.
        let text = vec![
            PageText {
                runs: vec![mk_run("hello"), mk_run("worldly")],
            },
            PageText { runs: vec![] },
        ];
        let kfx = pdf_to_kfx(&doc, &meta, None, Some(&text)).expect("pdf_to_kfx");
        let ents = entities(&kfx).unwrap();

        let field = |s: &IonValue, k: KfxSymbol| -> Option<IonValue> {
            match s {
                IonValue::Struct(fs) => fs
                    .iter()
                    .find(|(id, _)| *id == k as u64)
                    .map(|(_, v)| v.clone()),
                _ => None,
            }
        };
        let int = |v: &IonValue| -> i64 {
            match v {
                IonValue::Int(n) => *n,
                other => panic!("expected int, got {other:?}"),
            }
        };

        // position_id_map ($265): section-keyed {section_name, pid, length}.
        let pim = ents
            .iter()
            .find(|e| e.type_id == KfxSymbol::PositionIdMap as u32)
            .and_then(|e| parse_entity(&kfx, e))
            .expect("position_id_map present");
        let secs = match field(&pim, KfxSymbol::Contains) {
            Some(IonValue::List(l)) => l,
            _ => panic!("position_id_map has no contains list"),
        };
        assert_eq!(secs.len(), 2, "one position_id_map entry per section");
        let lengths: Vec<i64> = secs
            .iter()
            .map(|s| int(&field(s, KfxSymbol::Length).expect("length")))
            .collect();
        let pids: Vec<i64> = secs
            .iter()
            .map(|s| int(&field(s, KfxSymbol::Pid).expect("pid")))
            .collect();
        // Page 0 spans anchor+container+image+textref+anchor_end (5) + 5 + 7.
        // Page 1 carries no text and spans 6: the same 5, plus the one empty
        // page-sized container its text storyline holds.
        assert_eq!(lengths, vec![5 + 5 + 7, 5 + 1]);
        assert_eq!(pids[0], 0, "first section starts at pid 0");
        assert_eq!(pids[1], lengths[0], "pids are cumulative");

        // section_position_id_map ($609): one per section; the summed advances
        // (terminator pid) MUST equal the matching position_id_map length — the
        // invariant the fixed-layout reader relies on to resolve a selection.
        let spms: Vec<IonValue> = ents
            .iter()
            .filter(|e| e.type_id == KfxSymbol::SectionPositionIdMap as u32)
            .filter_map(|e| parse_entity(&kfx, e))
            .collect();
        assert_eq!(spms.len(), 2, "one section_position_id_map per section");
        for (k, spm) in spms.iter().enumerate() {
            let contains = match field(spm, KfxSymbol::Contains) {
                Some(IonValue::List(l)) => l,
                _ => panic!("section_position_id_map has no contains list"),
            };
            let sum: i64 = contains
                .iter()
                .map(|el| match el {
                    IonValue::Int(n) => *n,
                    IonValue::List(pair) => int(&pair[0]),
                    other => panic!("bad section map element: {other:?}"),
                })
                .sum();
            assert_eq!(
                sum, lengths[k],
                "section {k}: section_position_id_map terminator pid must equal position_id_map length"
            );
        }
    }

    #[test]
    fn cover_jpeg_does_not_break_pdf_extraction() {
        // With a cover, the KFX has *two* bcRawMedia blobs (the PDF and the
        // cover JPEG). `kfx_extract_pdf` returns the PDF, picking it by
        // the `%PDF-` magic and skipping the JPEG — not raise `MultipleSlices`.
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("With Cover".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "With Cover".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };

        // A stand-in cover blob with JPEG magic (real rendering needs PDFKit).
        let cover = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 0xFF, 0xD9];
        let kfx = pdf_to_kfx(&doc, &meta, Some(&cover), None).expect("pdf_to_kfx");

        assert!(kfx_is_pdf_backed(&kfx), "still PDF-backed with a cover");
        let extracted = kfx_extract_pdf(&kfx).expect("extraction should succeed past the cover");
        assert_eq!(
            extracted, bytes,
            "extracted PDF must skip the cover JPEG and be exact"
        );
        assert!(
            kfx.windows(cover.len()).any(|w| w == cover.as_slice()),
            "the cover JPEG must be embedded verbatim in the KFX"
        );
    }

    #[test]
    fn outline_becomes_nested_toc_in_kfx() {
        // A 2-page book with a one-level-nested outline gives a
        // book_navigation TOC whose labels are embedded verbatim, over a
        // byte-identical PDF round-trip.
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
            ],
            title: Some("Nav".to_string()),
            author: None,
            outline: vec![PdfOutlineItem {
                title: "Chapter One".to_string(),
                page_index: 0,
                children: vec![PdfOutlineItem {
                    title: "Section 1.1".to_string(),
                    page_index: 1,
                    children: Vec::new(),
                }],
            }],
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "Nav".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");

        let has = |needle: &[u8]| kfx.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"Chapter One"), "TOC parent label must be embedded");
        assert!(
            has(b"Section 1.1"),
            "nested TOC child label must be embedded"
        );
        assert_eq!(
            kfx_extract_pdf(&kfx).unwrap(),
            bytes,
            "TOC must not disturb the byte-identical PDF round-trip"
        );
    }

    #[test]
    fn edited_date_and_publisher_land_in_book_metadata() {
        // An edited year/publisher must reach the PDOC book_metadata (as
        // `issue_date`/`publisher`), not only the renamed filename. The date is
        // truncated to YYYY-MM-DD.
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes,
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("Dated".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "Dated".to_string(),
            author: None,
            language: "en".to_string(),
            date: Some("2021-03-15T09:00:00Z".to_string()),
            publisher: Some("Acme Press".to_string()),
            page_progression_direction: None,
        };
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");
        let has = |needle: &[u8]| kfx.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"issue_date"), "issue_date key must be emitted");
        assert!(
            has(b"2021-03-15"),
            "date truncated to YYYY-MM-DD must be present"
        );
        assert!(!has(b"T09:00:00"), "the ISO time part must be stripped");
        assert!(has(b"Acme Press"), "edited publisher must be embedded");
    }

    #[test]
    fn no_date_omits_issue_date() {
        // `None` date ⇒ no `issue_date` entry at all (it's optional for PDOC).
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes,
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("Undated".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = PdfKfxMeta {
            title: "Undated".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");
        assert!(
            !kfx.windows(b"issue_date".len()).any(|w| w == b"issue_date"),
            "no issue_date entry when the library has no date"
        );
    }

    #[test]
    fn page_labels_become_page_list_alongside_toc() {
        // Page labels → a `page_list` nav_container (`npag`) that coexists with
        // the TOC (`ntoc`); both are referenced and the labels land verbatim.
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes: bytes.clone(),
            pages: vec![
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
                PdfPage {
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                },
            ],
            title: Some("Paged".to_string()),
            author: None,
            outline: vec![PdfOutlineItem {
                title: "Chapter".to_string(),
                page_index: 0,
                children: Vec::new(),
            }],
            page_labels: vec!["Cover".to_string(), "xvii".to_string()],
        };
        let meta = PdfKfxMeta {
            title: "Paged".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");
        let has = |n: &[u8]| kfx.windows(n.len()).any(|w| w == n);
        assert!(has(b"npag"), "page_list container must exist");
        assert!(has(b"ntoc"), "toc container must coexist");
        assert!(has(b"Cover"), "first page label embedded");
        assert!(has(b"xvii"), "second page label embedded");
        assert_eq!(
            kfx_extract_pdf(&kfx).unwrap(),
            bytes,
            "nav must not disturb the byte-identical PDF round-trip"
        );
    }

    #[test]
    fn page_list_emitted_without_an_outline() {
        // No bookmarks ⇒ a `page_list` (page-number nav is unconditional),
        // and no `ntoc`.
        let bytes = fake_pdf();
        let doc = PdfDoc {
            bytes,
            pages: vec![PdfPage {
                width: 612.0,
                height: 792.0,
                rotation: 0,
            }],
            title: Some("NoToc".to_string()),
            author: None,
            outline: Vec::new(),
            page_labels: vec!["folio-7".to_string()],
        };
        let meta = PdfKfxMeta {
            title: "NoToc".to_string(),
            author: None,
            language: "en".to_string(),
            date: None,
            publisher: None,
            page_progression_direction: None,
        };
        let kfx = pdf_to_kfx(&doc, &meta, None, None).expect("pdf_to_kfx");
        let has = |n: &[u8]| kfx.windows(n.len()).any(|w| w == n);
        assert!(has(b"npag"), "page_list present without an outline");
        assert!(has(b"folio-7"), "page label embedded");
        assert!(
            !has(b"ntoc"),
            "no toc container when there are no bookmarks"
        );
    }

    #[test]
    fn garbage_is_not_pdf_backed() {
        assert!(!kfx_is_pdf_backed(b"not a kfx at all"));
        assert_eq!(kfx_extract_pdf(b"not a kfx"), Err(PdfExtractError::NotKfx));
    }

    #[test]
    fn reflowable_kfx_is_not_pdf_backed() {
        // A reflowable EPUB→KFX is no PDF-backed container, and
        // callers keep routing it through the normal EPUB path.
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
