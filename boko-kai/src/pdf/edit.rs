//! In-place PDF editing — the shared surgical-write harness.
//!
//! The PDF analog of [`crate::kfx::container_edit`] and [`crate::epub::edit`].
//! Each format's harness expresses the same idea in that format's own terms:
//! KFX re-serializes a container passing untouched entities through byte for
//! byte; EPUB repackages a zip passing untouched members through; and PDF —
//! uniquely — doesn't have to rewrite anything at all.
//!
//! PDF has a native surgical-edit mechanism: the **incremental update** (PDF
//! 32000-1 §7.5.6). The original file is left byte-for-byte intact and a new
//! generation of *only* the changed objects is appended, followed by a
//! cross-reference section chaining back to the original via `/Prev`. A reader
//! resolves each object to its newest generation, so appending a fresh `/Info`
//! or `/Outlines` supersedes the old one while every page, font, and image
//! stream keeps its original bytes and offsets.
//!
//! That makes this the highest-fidelity of the three harnesses: fidelity risk is
//! bounded by construction, because the original bytes are a literal prefix of
//! the output (asserted in this module's tests). Nothing else can be perturbed.
//!
//! Usage: [`PdfPackage::parse`], read the existing structure through
//! [`original`](PdfPackage::original), mutate via
//! [`edit_dict`](PdfPackage::edit_dict) (copy-on-write into the increment) /
//! [`add_object`](PdfPackage::add_object) / [`info_dict`](PdfPackage::info_dict),
//! then [`into_bytes`](PdfPackage::into_bytes).
//!
//! Scope (v1): **encrypted PDFs are rejected** — see [`PdfPackage::parse`].

use std::io;

use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId};

use super::doc::load_pdf;

/// A PDF opened for surgical editing.
///
/// Holds the original bytes plus a pending increment. Objects you never touch
/// are never re-encoded — they stay in the original bytes the output is built
/// on. An untouched package repackages to the input, byte for byte.
pub struct PdfPackage {
    inc: IncrementalDocument,
    /// Whether anything was actually staged; an untouched package short-circuits
    /// to the original bytes rather than appending an empty generation.
    dirty: bool,
}

impl PdfPackage {
    /// Open a PDF for editing.
    ///
    /// Loads through [`load_pdf`], so the catalog and page tree resolve even on
    /// files that defeat a bare `Document::load_mem`.
    ///
    /// **Encrypted PDFs are rejected.** lopdf decrypts strings and streams into
    /// memory on load, but the incremental writer emits them back as plaintext —
    /// so an appended `/Info` title would land unencrypted in a file whose reader
    /// will try to decrypt it, silently producing mojibake. Refusing up front is
    /// the honest v1 behaviour; supporting it means re-encrypting appended
    /// objects with the document key.
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let doc = load_pdf(bytes)?;
        if doc.trailer.get(b"Encrypt").is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encrypted PDF: editing would write unencrypted objects into an \
                 encrypted file",
            ));
        }
        let mut inc = IncrementalDocument::create_from(bytes.to_vec(), doc);
        // `Document::new_from_prev` hardcodes 1.4; the increment's `%PDF-x.y`
        // line is only a comment, but there's no reason for it to disagree with
        // the file it's appended to.
        inc.new_document.version = inc.get_prev_documents().version.clone();
        Ok(Self { inc, dirty: false })
    }

    /// The document as originally loaded — the read side. Every object resolves
    /// here; the increment only holds what has been staged for write.
    pub fn original(&self) -> &Document {
        self.inc.get_prev_documents()
    }

    /// True once any mutator has staged a change.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The document catalog's object id (trailer `/Root`).
    pub fn catalog_id(&self) -> io::Result<ObjectId> {
        self.original()
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("PDF trailer has no /Root catalog reference: {e}"),
                )
            })
    }

    /// Stage `id` for modification and return its dictionary mutably.
    ///
    /// Copy-on-write: the object is cloned out of the original into the pending
    /// increment on first call, so edits accumulate across calls and only the
    /// touched objects are ever appended. Errors if `id` is absent or isn't a
    /// dictionary.
    pub fn edit_dict(&mut self, id: ObjectId) -> io::Result<&mut Dictionary> {
        self.inc
            .opt_clone_object_to_new_document(id)
            .map_err(|e| io::Error::other(format!("staging object {id:?} failed: {e}")))?;
        self.dirty = true;
        // Deliberately a direct map lookup rather than `get_object_mut`, which
        // would chase a reference elsewhere: we staged exactly `id` and must
        // hand back exactly `id`.
        match self.inc.new_document.objects.get_mut(&id) {
            Some(Object::Dictionary(d)) => Ok(d),
            Some(other) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "object {id:?} is {}, not a dictionary",
                    other.type_name().unwrap_or(b"unknown").escape_ascii()
                ),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("object {id:?} not found in the PDF"),
            )),
        }
    }

    /// Append a brand-new object, returning its freshly allocated id. Ids
    /// continue after the original's highest, so a new object can never collide
    /// with an existing one.
    pub fn add_object(&mut self, object: impl Into<Object>) -> ObjectId {
        self.dirty = true;
        self.inc.new_document.add_object(object)
    }

    /// Set a trailer key on the pending increment (e.g. `/Info`). The increment's
    /// trailer starts as a clone of the original's, so unset keys carry over.
    pub fn set_trailer(&mut self, key: &str, value: Object) {
        self.dirty = true;
        self.inc.new_document.trailer.set(key, value);
    }

    /// The document `/Info` dictionary, staged for modification — created and
    /// wired into the trailer if the PDF has none (or points at a missing
    /// object).
    pub fn info_dict(&mut self) -> io::Result<&mut Dictionary> {
        let existing = self
            .original()
            .trailer
            .get(b"Info")
            .and_then(Object::as_reference)
            .ok()
            .filter(|id| self.original().objects.contains_key(id));

        match existing {
            Some(id) => self.edit_dict(id),
            None => {
                let id = self.add_object(Dictionary::new());
                self.set_trailer("Info", Object::Reference(id));
                self.edit_dict(id)
            }
        }
    }

    /// Serialize.
    ///
    /// With changes staged: the original bytes verbatim, then an appended
    /// generation of the touched objects plus a cross-reference section chaining
    /// back via `/Prev` (matching the original's table-vs-stream flavour).
    /// Untouched: the original bytes, unchanged — no empty generation appended.
    pub fn into_bytes(mut self) -> io::Result<Vec<u8>> {
        if !self.dirty {
            return Ok(self.inc.get_prev_documents_bytes().to_vec());
        }
        let mut out = Vec::with_capacity(self.inc.get_prev_documents_bytes().len() + 1024);
        self.inc
            .save_to(&mut out)
            .map_err(|e| io::Error::other(format!("PDF incremental save failed: {e}")))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::doc::{decode_pdf_string, encode_pdf_string};

    /// The minimal fixture: a classic cross-reference *table* PDF with an /Info.
    const MINIMAL: &str = "../sidle/core/tests/fixtures/minimal.pdf";

    fn minimal() -> Vec<u8> {
        std::fs::read(MINIMAL).expect("read minimal.pdf fixture")
    }

    /// A PDF using a cross-reference **stream** + an `/Info`, the layout every
    /// modern PDF (both real books this was validated against) actually uses.
    /// The harness must chain `/Prev` to it and append a matching xref *stream*,
    /// which is a different writer path from `minimal.pdf`'s xref table — so it
    /// gets its own coverage rather than relying on an out-of-repo book.
    /// `Document::new` defaults to `CrossReferenceStream`, asserted below.
    fn synthetic_xref_stream_pdf() -> Vec<u8> {
        use lopdf::dictionary;

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Synthetic Original"),
            "Author" => Object::string_literal("Nobody"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);

        let mut out = Vec::new();
        doc.save_to(&mut out).expect("save synthetic pdf");
        out
    }

    fn info_of(bytes: &[u8], key: &[u8]) -> Option<String> {
        let doc = load_pdf(bytes).ok()?;
        let info = doc.trailer.get(b"Info").ok()?;
        let dict = crate::pdf::doc::deref(&doc, info)?.as_dict().ok()?;
        Some(decode_pdf_string(dict.get(key).ok()?.as_str().ok()?))
    }

    /// The harness's core guarantee, and the one that makes it the safest of the
    /// three: the input is a literal prefix of the output. Nothing outside the
    /// appended generation can have been perturbed.
    #[test]
    fn edit_is_append_only() {
        let pdf = minimal();
        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("Edited Title"));
        let out = pkg.into_bytes().expect("save");

        assert!(
            out.starts_with(&pdf),
            "the original bytes must survive verbatim as a prefix"
        );
        assert!(out.len() > pdf.len(), "an increment was appended");
        assert!(out.ends_with(b"%%EOF"), "output ends with the EOF marker");
    }

    /// The appended generation supersedes the original object, and untouched
    /// keys of that same object survive the copy-on-write.
    #[test]
    fn edit_supersedes_and_preserves_siblings() {
        let pdf = minimal();
        assert_eq!(info_of(&pdf, b"Title").as_deref(), Some("Tiny Test PDF"));
        assert_eq!(info_of(&pdf, b"Author").as_deref(), Some("A. Tester"));

        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("Edited Title"));
        let out = pkg.into_bytes().expect("save");

        assert_eq!(
            info_of(&out, b"Title").as_deref(),
            Some("Edited Title"),
            "the new generation wins"
        );
        assert_eq!(
            info_of(&out, b"Author").as_deref(),
            Some("A. Tester"),
            "an untouched key of the edited dict is preserved"
        );
    }

    /// The page tree — the bulk of any real PDF — is untouched by an edit.
    #[test]
    fn edit_preserves_the_page_tree() {
        let pdf = minimal();
        let before = load_pdf(&pdf).expect("load").get_pages().len();

        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("Edited"));
        let out = pkg.into_bytes().expect("save");

        assert_eq!(
            load_pdf(&out).expect("reload").get_pages().len(),
            before,
            "page count unchanged"
        );
    }

    /// An untouched package returns the input byte-for-byte — no stray
    /// generation appended for a no-op edit.
    #[test]
    fn untouched_package_is_byte_identical() {
        let pdf = minimal();
        let pkg = PdfPackage::parse(&pdf).expect("parse");
        assert!(!pkg.is_dirty());
        assert_eq!(pkg.into_bytes().expect("save"), pdf);
    }

    /// New objects get ids after the original's max, so they can't collide.
    #[test]
    fn added_objects_get_fresh_ids() {
        let pdf = minimal();
        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        let max_before = pkg.original().max_id;

        let a = pkg.add_object(Dictionary::new());
        let b = pkg.add_object(Dictionary::new());
        assert!(a.0 > max_before, "first new id is past the original max");
        assert_ne!(a, b, "ids are unique");
        assert!(b.0 > a.0, "ids advance");

        // And they survive the round-trip as real, resolvable objects.
        let out = pkg.into_bytes().expect("save");
        let doc = load_pdf(&out).expect("reload");
        assert!(doc.get_object(a).is_ok(), "added object {a:?} resolves");
        assert!(doc.get_object(b).is_ok(), "added object {b:?} resolves");
    }

    /// Successive edits accumulate rather than clobbering each other (the
    /// copy-on-write is idempotent per object).
    #[test]
    fn successive_edits_accumulate() {
        let pdf = minimal();
        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("First"));
        pkg.info_dict()
            .expect("info")
            .set("Subject", encode_pdf_string("Second"));
        let out = pkg.into_bytes().expect("save");

        assert_eq!(info_of(&out, b"Title").as_deref(), Some("First"));
        assert_eq!(info_of(&out, b"Subject").as_deref(), Some("Second"));
        assert_eq!(info_of(&out, b"Author").as_deref(), Some("A. Tester"));
    }

    /// A PDF with no `/Info` gets one synthesized and wired into the trailer.
    #[test]
    fn info_dict_is_created_when_absent() {
        // Strip the fixture's /Info reference from its trailer. Safe byte
        // surgery: the trailer sits *after* the xref table, so dropping bytes
        // from it moves no object and leaves `startxref` pointing at the xref.
        let orig = minimal();
        let needle = b"/Info 4 0 R ";
        let at = orig
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("fixture trailer declares /Info");
        let mut pdf = orig[..at].to_vec();
        pdf.extend_from_slice(&orig[at + needle.len()..]);
        assert_eq!(info_of(&pdf, b"Title"), None, "precondition: no /Info");

        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("Synthesized"));
        let out = pkg.into_bytes().expect("save");

        assert_eq!(info_of(&out, b"Title").as_deref(), Some("Synthesized"));
    }

    /// Editing a non-dictionary object is a clean error, not a panic.
    #[test]
    fn edit_dict_rejects_non_dict_and_missing() {
        let pdf = minimal();
        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        // (2,0) is the /Pages dict; (9999,0) doesn't exist.
        assert!(pkg.edit_dict((9999, 0)).is_err(), "missing object errors");

        let id = pkg.add_object(Object::Integer(42));
        assert!(pkg.edit_dict(id).is_err(), "non-dictionary errors");
    }

    #[test]
    fn parse_rejects_non_pdf() {
        assert!(PdfPackage::parse(b"not a pdf").is_err());
    }

    /// The xref-*stream* layout: the increment must chain to it and append a
    /// matching stream section, preserving the original bytes and page tree just
    /// as for the xref-table fixture.
    #[test]
    fn edit_works_on_xref_stream_pdfs() {
        use lopdf::xref::XrefType;

        let pdf = synthetic_xref_stream_pdf();
        let before = load_pdf(&pdf).expect("load synthetic");
        assert!(
            matches!(
                before.reference_table.cross_reference_type,
                XrefType::CrossReferenceStream
            ),
            "precondition: this fixture really is xref-stream flavoured"
        );
        assert_eq!(before.get_pages().len(), 1);

        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("Edited 人間"));
        let out = pkg.into_bytes().expect("save");

        assert!(out.starts_with(&pdf), "append-only holds for xref streams");
        assert_eq!(info_of(&out, b"Title").as_deref(), Some("Edited 人間"));
        assert_eq!(
            info_of(&out, b"Author").as_deref(),
            Some("Nobody"),
            "sibling key preserved"
        );
        assert_eq!(
            load_pdf(&out).expect("reload").get_pages().len(),
            1,
            "page tree intact"
        );
    }

    /// The increment's `%PDF-x.y` marker must not contradict the file it is
    /// appended to (lopdf's `new_from_prev` would otherwise hardcode 1.4).
    #[test]
    fn increment_does_not_downgrade_the_version() {
        let pdf = synthetic_xref_stream_pdf(); // built as 1.5
        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        assert_eq!(pkg.original().version, "1.5");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("v"));
        let out = pkg.into_bytes().expect("save");
        assert_eq!(load_pdf(&out).expect("reload").version, "1.5");
    }

    /// Encrypted PDFs are refused rather than silently written as plaintext into
    /// a file whose reader will try to decrypt them.
    #[test]
    fn parse_rejects_encrypted_pdfs() {
        use lopdf::dictionary;

        // Stand in for a real encrypted file: a trailer declaring /Encrypt is
        // exactly what `parse` gates on.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        let encrypt_id = doc.add_object(dictionary! {
            "Filter" => "Standard", "V" => 1, "R" => 2,
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Encrypt", encrypt_id);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).expect("save");

        match PdfPackage::parse(&pdf) {
            Ok(_) => panic!("must refuse encrypted PDFs"),
            Err(e) => assert!(
                e.to_string().contains("encrypted"),
                "error names the reason: {e}"
            ),
        }
    }

    /// Non-ASCII text survives the write→read round-trip through the file.
    #[test]
    fn unicode_title_roundtrips_through_the_file() {
        let pdf = minimal();
        let mut pkg = PdfPackage::parse(&pdf).expect("parse");
        pkg.info_dict()
            .expect("info")
            .set("Title", encode_pdf_string("人間失格 — 太宰治"));
        let out = pkg.into_bytes().expect("save");
        assert_eq!(
            info_of(&out, b"Title").as_deref(),
            Some("人間失格 — 太宰治")
        );
    }
}
