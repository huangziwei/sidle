//! Give a PDF a cover page — replace its first page with an image, or insert
//! one in front.
//!
//! ## Why this, and not "embed a cover image"
//!
//! EPUB and KFX carry a cover as a *resource* the format designates
//! (`<meta name="cover">`, `$164 external_resource`), so their cover editors
//! swap that resource. PDF has no such concept: a PDF's cover **is its first
//! page**. Everything downstream already agrees — `pdf_to_kfx` builds a
//! PDOC's library tile by rendering page 1 (`render::render_pdf_page_jpeg(.., 0, ..)`),
//! and any viewer shows page 1 first. So the honest operation is to edit that
//! page, after which every derived artifact follows for free.
//!
//! Two modes, because a PDF either has a cover page or it doesn't:
//! - [`CoverMode::Replace`] — the book opens on a cover page already (a scan of
//!   the jacket, a title card) and you want a better image in its place.
//! - [`CoverMode::Insert`] — the book opens straight onto body text; it needs a
//!   cover page it never had.
//!
//! ## What gets written
//!
//! An Image XObject (`/DCTDecode`, `/DeviceRGB`) plus a content stream that
//! draws it, letterboxed to preserve aspect, centred on a page the same size as
//! the book's existing first page — so the cover matches the rest of the book
//! rather than jarring the page size. Everything rides [`PdfPackage`], so the
//! original bytes are untouched and only the new objects are appended.
//!
//! `/Rotate 0` is set explicitly on the page we write. It is inheritable from
//! the page tree, so a book whose `/Pages` node declares `/Rotate 90` would
//! otherwise turn our cover on its side.

use std::io;

use lopdf::{Dictionary, Object, ObjectId, Stream};

use super::doc::{deref, encode_pdf_string, page_dimensions};
use super::edit::PdfPackage;

/// JPEG quality for the embedded cover. 88 sits above the 85 the library's own
/// cover renderer uses (`render::COVER_JPEG_QUALITY`) — this image is the
/// archival copy inside the book, and the render downstream of it is lossy
/// again, so it's worth a couple of points.
const COVER_JPEG_QUALITY: u8 = 88;

/// How to give the PDF a cover page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverMode {
    /// Overwrite the existing first page. Page count is unchanged, so nothing
    /// downstream shifts.
    Replace,
    /// Add a new first page in front of the book. The page count grows by one —
    /// see the note on `/PageLabels` in [`set_cover_page`].
    Insert,
}

/// Write `image` as the PDF's cover page, returning the edited bytes.
///
/// `image` may be any raster the `image` crate decodes (JPEG/PNG/GIF/WebP/BMP);
/// it is re-encoded to a baseline RGB JPEG so the embedded `/DeviceRGB` +
/// `/DCTDecode` declaration is always truthful.
///
/// [`CoverMode::Insert`] shifts every subsequent page by one. Page *targets* are
/// unaffected — outline destinations and link annotations reference page
/// *objects*, which don't move — but the catalog's `/PageLabels` number tree is
/// index-keyed, so it is re-indexed here and given a `Cover` label at 0.
///
/// Errors if the bytes aren't a readable PDF, the PDF is encrypted (see
/// [`PdfPackage::parse`]), the image can't be decoded, or the page tree has no
/// pages.
pub fn set_cover_page(pdf_bytes: &[u8], image: &[u8], mode: CoverMode) -> io::Result<Vec<u8>> {
    let (jpeg, iw, ih) = crate::image::jpeg::to_baseline_rgb_jpeg(image, COVER_JPEG_QUALITY)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cover image couldn't be decoded (expected JPEG, PNG, GIF, WebP or BMP)",
            )
        })?;

    let mut pkg = PdfPackage::parse(pdf_bytes)?;
    let first_page = pkg
        .original()
        .get_pages()
        .values()
        .next()
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "PDF has no pages"))?;

    // Match the book's own page size (rotation already applied), so the cover
    // sits in the same frame as everything after it.
    let (pw, ph) = page_dimensions(pkg.original(), first_page);

    let image_id = pkg.add_object(Object::Stream(image_xobject(&jpeg, iw, ih)));
    let content_id = pkg.add_object(Object::Stream(
        Stream::new(Dictionary::new(), draw_ops(iw, ih, pw, ph).into_bytes())
            .with_compression(false),
    ));

    match mode {
        CoverMode::Replace => {
            // Overwrite the page object in place: no page-tree surgery, so the
            // count, the labels, and every other page's identity are untouched.
            // Keep its `/Parent` — that's the one field the tree needs back.
            let parent = pkg
                .original()
                .get_object(first_page)
                .and_then(Object::as_dict)
                .ok()
                .and_then(|d| d.get(b"Parent").ok())
                .and_then(|o| o.as_reference().ok());
            let page = page_dict(parent, image_id, content_id, pw, ph);
            *pkg.edit_dict(first_page)? = page;
        }
        CoverMode::Insert => {
            let pages_id = pages_root(&pkg)?;
            let page_id = pkg.add_object(page_dict(Some(pages_id), image_id, content_id, pw, ph));

            let root = pkg.edit_dict(pages_id)?;
            let mut kids = root
                .get(b"Kids")
                .and_then(Object::as_array)
                .cloned()
                .unwrap_or_default();
            kids.insert(0, Object::Reference(page_id));
            let count = root.get(b"Count").and_then(Object::as_i64).unwrap_or(0);
            root.set("Kids", Object::Array(kids));
            root.set("Count", Object::Integer(count + 1));

            shift_page_labels(&mut pkg)?;
        }
    }

    pkg.into_bytes()
}

/// The catalog's root `/Pages` node.
fn pages_root(pkg: &PdfPackage) -> io::Result<ObjectId> {
    let catalog_id = pkg.catalog_id()?;
    pkg.original()
        .get_object(catalog_id)
        .and_then(Object::as_dict)
        .ok()
        .and_then(|c| c.get(b"Pages").ok())
        .and_then(|o| o.as_reference().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "PDF catalog has no /Pages tree root",
            )
        })
}

/// The cover image as a PDF Image XObject. The JPEG is embedded verbatim under
/// `/DCTDecode` — PDF speaks JPEG natively, so there's no pixel re-encoding
/// here; `to_baseline_rgb_jpeg` already guaranteed the colorspace.
fn image_xobject(jpeg: &[u8], w: u32, h: u32) -> Stream {
    let mut d = Dictionary::new();
    d.set("Type", Object::Name(b"XObject".to_vec()));
    d.set("Subtype", Object::Name(b"Image".to_vec()));
    d.set("Width", Object::Integer(w as i64));
    d.set("Height", Object::Integer(h as i64));
    d.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
    d.set("BitsPerComponent", Object::Integer(8));
    d.set("Filter", Object::Name(b"DCTDecode".to_vec()));
    // `Stream::new` sets /Length; `with_compression(false)` keeps any later
    // processing pass from re-filtering already-compressed JPEG data.
    Stream::new(d, jpeg.to_vec()).with_compression(false)
}

/// The content stream that paints the cover: scale the unit image square to fit
/// the page while preserving aspect, centred (letterboxed). PDF user space has
/// its origin bottom-left, and `Do` on an image paints it into the unit square,
/// so the `cm` matrix *is* the placement.
fn draw_ops(iw: u32, ih: u32, pw: f32, ph: f32) -> String {
    let (sw, sh, tx, ty) = fit(iw, ih, pw, ph);
    format!("q\n{sw:.4} 0 0 {sh:.4} {tx:.4} {ty:.4} cm\n/Im0 Do\nQ\n")
}

/// Letterbox `iw × ih` into `pw × ph`: returns the drawn size and the offset
/// that centres it. Degenerate inputs fall back to filling the page rather than
/// dividing by zero.
fn fit(iw: u32, ih: u32, pw: f32, ph: f32) -> (f32, f32, f32, f32) {
    if iw == 0 || ih == 0 || pw <= 0.0 || ph <= 0.0 {
        return (pw.max(1.0), ph.max(1.0), 0.0, 0.0);
    }
    let scale = (pw / iw as f32).min(ph / ih as f32);
    let (sw, sh) = (iw as f32 * scale, ih as f32 * scale);
    ((sw), (sh), (pw - sw) / 2.0, (ph - sh) / 2.0)
}

/// A page whose only content is the cover image.
fn page_dict(
    parent: Option<ObjectId>,
    image_id: ObjectId,
    content_id: ObjectId,
    pw: f32,
    ph: f32,
) -> Dictionary {
    let mut xobj = Dictionary::new();
    xobj.set("Im0", Object::Reference(image_id));
    let mut res = Dictionary::new();
    res.set("XObject", Object::Dictionary(xobj));
    res.set(
        "ProcSet",
        Object::Array(vec![
            Object::Name(b"PDF".to_vec()),
            Object::Name(b"ImageC".to_vec()),
        ]),
    );

    let mut d = Dictionary::new();
    d.set("Type", Object::Name(b"Page".to_vec()));
    if let Some(p) = parent {
        d.set("Parent", Object::Reference(p));
    }
    d.set(
        "MediaBox",
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(pw),
            Object::Real(ph),
        ]),
    );
    // Explicit, not inherited: see the module note.
    d.set("Rotate", Object::Integer(0));
    d.set("Resources", Object::Dictionary(res));
    d.set("Contents", Object::Reference(content_id));
    d
}

/// Re-index the catalog's `/PageLabels` after an insert: every declared run
/// moves one page later, and page 0 — the new cover — gets a `Cover` label,
/// mirroring what Amazon's own PDOC pipeline emits.
///
/// The run dictionaries are carried over **as objects**, untouched: only the
/// integer keys change, so no label semantics (`/S` style, `/St` start, `/P`
/// prefix) can be lost in translation. A nested number tree is flattened to a
/// single `/Nums` array, which is equally legal and simpler to emit.
///
/// A no-op when the PDF declares no labels — then `probe_pdf` falls back to
/// sequential numbering, which stays correct with an extra page.
fn shift_page_labels(pkg: &mut PdfPackage) -> io::Result<()> {
    let catalog_id = pkg.catalog_id()?;
    let Some(root) = pkg
        .original()
        .get_object(catalog_id)
        .and_then(Object::as_dict)
        .ok()
        .and_then(|c| c.get(b"PageLabels").ok())
        .and_then(|o| deref(pkg.original(), o))
        .and_then(|o| o.as_dict().ok())
    else {
        return Ok(());
    };

    let mut runs: Vec<(i64, Object)> = Vec::new();
    collect_runs(pkg.original(), root, &mut runs, 0);
    runs.sort_by_key(|(i, _)| *i);

    let mut nums: Vec<Object> = vec![
        Object::Integer(0),
        Object::Dictionary({
            let mut d = Dictionary::new();
            d.set("P", encode_pdf_string("Cover"));
            d
        }),
    ];
    for (index, run) in runs {
        nums.push(Object::Integer(index + 1));
        nums.push(run);
    }

    let labels_id = pkg.add_object(Object::Dictionary({
        let mut d = Dictionary::new();
        d.set("Nums", Object::Array(nums));
        d
    }));
    pkg.edit_dict(catalog_id)?
        .set("PageLabels", Object::Reference(labels_id));
    Ok(())
}

/// Collect `(index, run_dict)` pairs from a number tree (`/Nums` leaves +
/// `/Kids` sub-nodes). Depth-guarded like the reader's own walk.
fn collect_runs(
    doc: &lopdf::Document,
    node: &Dictionary,
    out: &mut Vec<(i64, Object)>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    if let Some(nums) = node
        .get(b"Nums")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        let mut i = 0;
        while i + 1 < nums.len() {
            if let Ok(idx) = nums[i].as_i64()
                && let Some(run) = deref(doc, &nums[i + 1])
            {
                out.push((idx, run.clone()));
            }
            i += 2;
        }
    }
    if let Some(kids) = node
        .get(b"Kids")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        for kid in kids {
            if let Some(kd) = kid
                .as_reference()
                .ok()
                .and_then(|id| doc.get_dictionary(id).ok())
            {
                collect_runs(doc, kd, out, depth + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::probe_pdf;
    use crate::pdf::doc::load_pdf;

    const MINIMAL: &str = "../sidle/core/tests/fixtures/minimal.pdf";

    fn minimal() -> Vec<u8> {
        std::fs::read(MINIMAL).expect("read minimal.pdf fixture")
    }

    /// A distinctive PNG so we can tell the cover apart from anything else.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        let img = image::RgbImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgb([220, 30, 30])
            } else {
                image::Rgb([30, 30, 220])
            }
        });
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode png");
        buf
    }

    /// Multi-page PDF with a `/PageLabels` tree (roman front-matter, then
    /// decimal) — the layout an insert has to re-index.
    fn labelled_pdf(pages: usize) -> Vec<u8> {
        use lopdf::{Document, dictionary};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let kids: Vec<Object> = (0..pages)
            .map(|_| {
                doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 400.into(), 600.into()],
                })
                .into()
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => pages as i64,
            }),
        );
        let labels = doc.add_object(dictionary! {
            "Nums" => vec![
                0.into(),
                Object::Dictionary(dictionary! { "S" => Object::Name(b"r".to_vec()) }),
                2.into(),
                Object::Dictionary(dictionary! { "S" => Object::Name(b"D".to_vec()), "St" => 1 }),
            ],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id, "PageLabels" => labels,
        });
        doc.trailer.set("Root", catalog_id);
        let mut out = Vec::new();
        doc.save_to(&mut out).expect("save");
        out
    }

    /// Insert: the book grows a first page and everything else shifts down one.
    #[test]
    fn insert_adds_a_first_page() {
        let pdf = labelled_pdf(4);
        let before = probe_pdf(pdf.clone()).expect("probe");
        assert_eq!(before.pages.len(), 4);

        let out = set_cover_page(&pdf, &png(600, 900), CoverMode::Insert).expect("insert");
        assert!(out.starts_with(&pdf), "append-only");

        let after = probe_pdf(out).expect("probe edited");
        assert_eq!(after.pages.len(), 5, "one page added");
        // The cover matches the book's page size, not the image's aspect.
        assert_eq!(
            (after.pages[0].width, after.pages[0].height),
            (before.pages[0].width, before.pages[0].height),
            "cover page is the same size as the book's pages"
        );
    }

    /// Replace: the first page becomes the cover, and the page count is
    /// unchanged — so nothing downstream shifts.
    #[test]
    fn replace_swaps_the_first_page_without_growing_the_book() {
        let pdf = labelled_pdf(4);
        let before = probe_pdf(pdf.clone()).expect("probe");

        let out = set_cover_page(&pdf, &png(400, 400), CoverMode::Replace).expect("replace");
        assert!(out.starts_with(&pdf), "append-only");

        let after = probe_pdf(out.clone()).expect("probe edited");
        assert_eq!(
            after.pages.len(),
            before.pages.len(),
            "page count unchanged"
        );
        assert_eq!(
            after.page_labels, before.page_labels,
            "labels unchanged — replace shifts nothing"
        );

        // The first page now draws an image.
        let doc = load_pdf(&out).expect("reload");
        let first = *doc.get_pages().values().next().unwrap();
        let page = doc.get_dictionary(first).expect("page dict");
        let res = page
            .get(b"Resources")
            .and_then(|o| o.as_dict())
            .expect("resources");
        assert!(
            res.get(b"XObject").is_ok(),
            "first page carries an image XObject"
        );
        assert_eq!(
            page.get(b"Rotate").and_then(|o| o.as_i64()).unwrap(),
            0,
            "rotate pinned so an inherited /Rotate can't turn the cover"
        );
    }

    /// The inserted cover is a real image page: XObject with the JPEG we
    /// embedded, and a content stream that draws it.
    #[test]
    fn inserted_page_embeds_the_jpeg_and_draws_it() {
        let pdf = labelled_pdf(2);
        let out = set_cover_page(&pdf, &png(300, 450), CoverMode::Insert).expect("insert");
        let doc = load_pdf(&out).expect("reload");

        let first = *doc.get_pages().values().next().unwrap();
        let page = doc.get_dictionary(first).expect("page");
        let xobj = page
            .get(b"Resources")
            .and_then(|o| o.as_dict())
            .and_then(|r| r.get(b"XObject"))
            .and_then(|o| o.as_dict())
            .expect("xobject dict");
        let img_id = xobj
            .get(b"Im0")
            .and_then(|o| o.as_reference())
            .expect("Im0");
        let img = match doc.get_object(img_id) {
            Ok(Object::Stream(s)) => s,
            other => panic!("Im0 must be a stream, got {other:?}"),
        };
        assert_eq!(
            img.dict.get(b"Filter").and_then(|o| o.as_name()).unwrap(),
            b"DCTDecode"
        );
        assert_eq!(
            img.dict
                .get(b"ColorSpace")
                .and_then(|o| o.as_name())
                .unwrap(),
            b"DeviceRGB"
        );
        assert_eq!(
            img.dict.get(b"Width").and_then(|o| o.as_i64()).unwrap(),
            300
        );
        assert_eq!(
            img.dict.get(b"Height").and_then(|o| o.as_i64()).unwrap(),
            450
        );
        assert_eq!(&img.content[..3], &[0xFF, 0xD8, 0xFF], "a real JPEG");

        let content_id = page
            .get(b"Contents")
            .and_then(|o| o.as_reference())
            .expect("contents ref");
        let ops = match doc.get_object(content_id) {
            Ok(Object::Stream(s)) => String::from_utf8_lossy(&s.content).to_string(),
            other => panic!("contents must be a stream, got {other:?}"),
        };
        assert!(
            ops.contains("/Im0 Do"),
            "content stream paints the image: {ops}"
        );
        assert!(
            ops.starts_with('q') && ops.trim_end().ends_with('Q'),
            "balanced: {ops}"
        );
    }

    /// Insert re-indexes `/PageLabels`: the cover takes page 0 and every
    /// declared run moves one page later, so labels still name the right pages.
    #[test]
    fn insert_reindexes_page_labels() {
        let pdf = labelled_pdf(4);
        // Before: roman from page 0, decimal from page 2 → i, ii, 1, 2.
        assert_eq!(
            probe_pdf(pdf.clone()).unwrap().page_labels,
            vec!["i", "ii", "1", "2"]
        );

        let out = set_cover_page(&pdf, &png(100, 150), CoverMode::Insert).expect("insert");
        assert_eq!(
            probe_pdf(out).unwrap().page_labels,
            vec!["Cover", "i", "ii", "1", "2"],
            "every original label still names its original page"
        );
    }

    /// A PDF with no `/PageLabels` needs no re-indexing (sequential numbering
    /// stays right with an extra page) and must not gain a bogus tree.
    #[test]
    fn insert_without_labels_stays_sequential() {
        let pdf = minimal();
        let out = set_cover_page(&pdf, &png(50, 50), CoverMode::Insert).expect("insert");
        let after = probe_pdf(out).expect("probe");
        assert_eq!(after.pages.len(), 2);
        assert_eq!(after.page_labels, vec!["1", "2"]);
    }

    /// Letterboxing preserves aspect and centres.
    #[test]
    fn fit_letterboxes_and_centres() {
        // Square image into a tall page: full width, centred vertically.
        let (sw, sh, tx, ty) = fit(100, 100, 400.0, 600.0);
        assert_eq!((sw, sh), (400.0, 400.0));
        assert_eq!(tx, 0.0);
        assert_eq!(ty, 100.0);

        // Wide image into a square page: full width, centred vertically.
        let (sw, sh, tx, ty) = fit(200, 100, 400.0, 400.0);
        assert_eq!((sw, sh), (400.0, 200.0));
        assert_eq!((tx, ty), (0.0, 100.0));

        // Exact aspect match fills the page.
        let (sw, sh, tx, ty) = fit(400, 600, 400.0, 600.0);
        assert_eq!((sw, sh, tx, ty), (400.0, 600.0, 0.0, 0.0));

        // Degenerate input doesn't divide by zero.
        let (sw, sh, ..) = fit(0, 0, 400.0, 600.0);
        assert!(sw > 0.0 && sh > 0.0);
    }

    #[test]
    fn rejects_an_undecodable_image() {
        let err = set_cover_page(&minimal(), b"not an image", CoverMode::Insert)
            .expect_err("must reject");
        assert!(err.to_string().contains("decoded"), "{err}");
    }

    #[test]
    fn rejects_non_pdf() {
        assert!(set_cover_page(b"not a pdf", &png(10, 10), CoverMode::Insert).is_err());
    }

    /// Replacing twice is stable — the second cover supersedes the first and the
    /// page count still doesn't move.
    #[test]
    fn replace_is_idempotent_in_page_count() {
        let pdf = labelled_pdf(3);
        let once = set_cover_page(&pdf, &png(100, 100), CoverMode::Replace).expect("first");
        let twice = set_cover_page(&once, &png(200, 100), CoverMode::Replace).expect("second");
        assert_eq!(probe_pdf(twice).unwrap().pages.len(), 3);
    }
}
