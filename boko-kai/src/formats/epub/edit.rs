//! In-place EPUB (OCF zip) editing — the shared surgical-write harness.
//!
//! Every EPUB source edit has the same shape: open the zip, change a chosen
//! handful of member files (the OPF, a nav doc, one image), and pass every
//! *other* member through unchanged, then repackage with the OCF invariant —
//! `mimetype` first and uncompressed. This is the EPUB analog of
//! [`crate::formats::kfx::container_edit`]: metadata / cover / image / TOC edits all
//! build on this one audited core rather than each re-implementing the zip walk.
//!
//! Unlike KFX (binary Ion behind a doc-symbol table), EPUB members *are*
//! human-editable XHTML/CSS/OPF/NCX, so the harness exposes them by path:
//! [`EpubPackage::get`] / [`replace`](EpubPackage::replace) /
//! [`set`](EpubPackage::set) / [`remove`](EpubPackage::remove), then
//! [`into_bytes`](EpubPackage::into_bytes) to repackage. Members are held
//! decompressed in their original order; an untouched member re-serializes with
//! the same storage method it had on disk (so already-compressed images stay
//! `Stored` rather than being wastefully re-deflated).
//!
//! Parse is deliberately lenient (it does not require a `mimetype` or
//! `container.xml`); the EPUB-aware accessors [`opf_path`](EpubPackage::opf_path)
//! / [`opf_bytes`](EpubPackage::opf_bytes) validate when a consumer actually
//! needs the package document.

use std::io::{self, Cursor, Read, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::formats::epub::{neutralize_spurious_zip64, parse_container_xml};
use crate::util::percent_decode;

/// The OCF-mandated first member: the media-type marker.
const MIMETYPE_NAME: &str = "mimetype";
/// The canonical `mimetype` body, synthesized when a source EPUB lacks one.
const MIMETYPE_BODY: &[u8] = b"application/epub+zip";
/// The OCF container descriptor that names the OPF package document.
const CONTAINER_PATH: &str = "META-INF/container.xml";

/// One EPUB zip member, decompressed, carrying the storage method it had on disk
/// so an untouched member re-serializes with the same compression choice.
struct Entry {
    name: String,
    data: Vec<u8>,
    method: CompressionMethod,
}

/// An EPUB opened for surgical editing: every zip member decompressed in memory,
/// in original order. Mutate members by path, then
/// [`into_bytes`](Self::into_bytes) to repackage — `mimetype` first and
/// uncompressed, every other member after it with its original storage method.
pub struct EpubPackage {
    entries: Vec<Entry>,
}

impl EpubPackage {
    /// Parse an EPUB's zip directory, decompressing every member into memory.
    ///
    /// Retries once on the spurious-ZIP64 repair (a handful of producers emit
    /// extra fields the `zip` crate misreads — the same recovery the importer
    /// applies in [`crate::import::epub`]). Directory entries (names ending in
    /// `/`) are dropped: they carry no data and are implied by member paths, so
    /// readers reconstruct them (calibre and the boko exporter do the same).
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        match Self::parse_inner(bytes) {
            Ok(pkg) => Ok(pkg),
            Err(first) => match neutralize_spurious_zip64(bytes) {
                Some(repaired) => Self::parse_inner(&repaired),
                None => Err(first),
            },
        }
    }

    fn parse_inner(bytes: &[u8]) -> io::Result<Self> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
        let mut entries = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(io::Error::other)?;
            if file.name().ends_with('/') {
                continue; // directory entry — implied by member paths
            }
            let name = file.name().to_string();
            let method = file.compression();
            let mut data = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
            file.read_to_end(&mut data)?;
            entries.push(Entry { name, data, method });
        }
        Ok(Self { entries })
    }

    /// The decompressed bytes of the member at `name`, if present.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.data.as_slice())
    }

    /// True if a member with this exact path exists.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Every member path, in original zip order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Replace an existing member's bytes in place, keeping its position and
    /// storage method. Returns `false` (changing nothing) if no member has that
    /// name — use [`set`](Self::set) to upsert.
    pub fn replace(&mut self, name: &str, data: Vec<u8>) -> bool {
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(e) => {
                e.data = data;
                true
            }
            None => false,
        }
    }

    /// Replace an existing member, or append a new deflated one if absent.
    pub fn set(&mut self, name: &str, data: Vec<u8>) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.name == name) {
            e.data = data;
        } else {
            self.entries.push(Entry {
                name: name.to_string(),
                data,
                method: CompressionMethod::Deflated,
            });
        }
    }

    /// Remove the member at `name`. Returns `true` if one was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.entries.len() != before
    }

    /// The OPF package document's zip path, read from `META-INF/container.xml`
    /// (percent-decoded to the literal member name, matching the importer).
    pub fn opf_path(&self) -> io::Result<String> {
        let container = self.get(CONTAINER_PATH).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "EPUB has no META-INF/container.xml",
            )
        })?;
        Ok(percent_decode(&parse_container_xml(container)?))
    }

    /// The OPF package document's bytes, located via `container.xml`.
    pub fn opf_bytes(&self) -> io::Result<&[u8]> {
        let path = self.opf_path()?;
        self.get(&path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("OPF not found at {path}"))
        })
    }

    /// Repackage into EPUB (OCF) bytes. `mimetype` is written first and
    /// uncompressed (`Stored`) per OCF §3.3; every other member follows in
    /// original order with the storage method it was parsed with. A source that
    /// lacked a `mimetype` gets the canonical one synthesized.
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));

        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let mimetype = self
            .entries
            .iter()
            .find(|e| e.name == MIMETYPE_NAME)
            .map(|e| e.data.as_slice())
            .unwrap_or(MIMETYPE_BODY);
        zip.start_file(MIMETYPE_NAME, stored)
            .map_err(io::Error::other)?;
        zip.write_all(mimetype)?;

        for e in &self.entries {
            if e.name == MIMETYPE_NAME {
                continue; // already emitted first
            }
            let opts = SimpleFileOptions::default().compression_method(writable_method(e.method));
            zip.start_file(&e.name, opts).map_err(io::Error::other)?;
            zip.write_all(&e.data)?;
        }

        let cursor = zip.finish().map_err(io::Error::other)?;
        Ok(cursor.into_inner())
    }
}

/// Map a parsed member's compression to one the writer can always emit. The data
/// is already decompressed, so anything that isn't `Stored` re-deflates cleanly
/// (an exotic method that survived read would otherwise fail on write).
fn writable_method(m: CompressionMethod) -> CompressionMethod {
    if m == CompressionMethod::Stored {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    }
}

/// XML-escape text content (`&`, `<`, `>`) for emission into an OPF / nav / NCX.
/// Shared by the EPUB surgical-write primitives ([`super::toc_repair`],
/// [`super::metadata_edit`]).
pub(crate) fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML-escape an attribute value (text plus the quote char).
pub(crate) fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    fn read_fixture() -> Vec<u8> {
        std::fs::read(FIXTURE).expect("read EPUB fixture")
    }

    /// An untouched parse→repackage round-trip preserves every member's bytes
    /// and reopens as a valid `Book`. This is the harness's core guarantee:
    /// anything not explicitly changed survives verbatim.
    #[test]
    fn roundtrip_is_faithful_and_reopens() {
        let epub = read_fixture();
        let before = EpubPackage::parse(&epub).expect("parse fixture");
        let before_names: Vec<String> = before.names().map(str::to_string).collect();
        let before_cover = before
            .get("OEBPS/cover.jpeg")
            .expect("cover present")
            .to_vec();

        let out = EpubPackage::parse(&epub)
            .expect("parse fixture")
            .into_bytes()
            .expect("repackage");
        let after = EpubPackage::parse(&out).expect("re-parse repackaged");

        let after_names: Vec<String> = after.names().map(str::to_string).collect();
        assert_eq!(
            before_names, after_names,
            "every member preserved, in order"
        );
        assert_eq!(
            after.get("OEBPS/cover.jpeg"),
            Some(before_cover.as_slice()),
            "image bytes pass through unchanged"
        );

        // The repackaged bytes still open + parse as an EPUB.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.epub");
        std::fs::write(&path, &out).expect("write");
        let book = crate::Book::open(&path).expect("repackaged EPUB opens");
        assert_eq!(book.metadata().title, "人間失格");
        assert!(!book.spine().is_empty(), "spine survives repackage");
    }

    /// `mimetype` must be the first member and uncompressed (OCF §3.3).
    #[test]
    fn mimetype_is_first_and_stored() {
        let epub = read_fixture();
        let out = EpubPackage::parse(&epub)
            .expect("parse")
            .into_bytes()
            .expect("repackage");

        let mut archive = ZipArchive::new(Cursor::new(&out)).expect("open output");
        let first = archive.by_index(0).expect("first member");
        assert_eq!(first.name(), MIMETYPE_NAME, "mimetype is first");
        assert_eq!(
            first.compression(),
            CompressionMethod::Stored,
            "mimetype is uncompressed"
        );
        drop(first);
        let mut body = String::new();
        archive
            .by_name(MIMETYPE_NAME)
            .expect("mimetype member")
            .read_to_string(&mut body)
            .expect("read mimetype");
        assert_eq!(body.as_bytes(), MIMETYPE_BODY);
    }

    /// A single `replace` changes exactly one member; the rest are byte-identical.
    #[test]
    fn replace_is_surgical() {
        let epub = read_fixture();
        let original_css = {
            let p = EpubPackage::parse(&epub).expect("parse");
            p.get("OEBPS/style.css").expect("css present").to_vec()
        };
        let new_css = b"/* edited */ body { color: red; }".to_vec();

        let mut pkg = EpubPackage::parse(&epub).expect("parse");
        assert!(pkg.replace("OEBPS/style.css", new_css.clone()), "replaced");
        assert!(
            !pkg.replace("OEBPS/does-not-exist.css", vec![]),
            "replace of a missing member is a no-op returning false"
        );
        let cover = pkg.get("OEBPS/cover.jpeg").expect("cover").to_vec();

        let after = EpubPackage::parse(&pkg.into_bytes().expect("repackage")).expect("re-parse");
        assert_eq!(after.get("OEBPS/style.css"), Some(new_css.as_slice()));
        assert_ne!(
            after.get("OEBPS/style.css").unwrap(),
            original_css.as_slice()
        );
        assert_eq!(
            after.get("OEBPS/cover.jpeg"),
            Some(cover.as_slice()),
            "an unrelated member is untouched"
        );
    }

    /// `set` upserts (replace-or-append) and `remove` deletes.
    #[test]
    fn set_and_remove() {
        let epub = read_fixture();
        let mut pkg = EpubPackage::parse(&epub).expect("parse");

        pkg.set("OEBPS/new.txt", b"hello".to_vec());
        assert_eq!(pkg.get("OEBPS/new.txt"), Some(b"hello".as_slice()));
        pkg.set("OEBPS/new.txt", b"world".to_vec()); // upsert existing
        assert_eq!(pkg.get("OEBPS/new.txt"), Some(b"world".as_slice()));

        assert!(pkg.remove("OEBPS/titlepage.xhtml"));
        assert!(!pkg.remove("OEBPS/titlepage.xhtml"), "already gone");
        assert!(!pkg.contains("OEBPS/titlepage.xhtml"));

        let after = EpubPackage::parse(&pkg.into_bytes().expect("repackage")).expect("re-parse");
        assert_eq!(after.get("OEBPS/new.txt"), Some(b"world".as_slice()));
        assert!(!after.contains("OEBPS/titlepage.xhtml"));
    }

    /// The OPF is located via `container.xml`, and its bytes come back.
    #[test]
    fn opf_path_and_bytes() {
        let epub = read_fixture();
        let pkg = EpubPackage::parse(&epub).expect("parse");
        assert_eq!(pkg.opf_path().expect("opf path"), "OEBPS/content.opf");
        let opf = pkg.opf_bytes().expect("opf bytes");
        assert!(
            opf.windows(9).any(|w| w == b"<dc:title"),
            "opf_bytes returns the package document"
        );
    }

    #[test]
    fn parse_rejects_non_zip() {
        assert!(EpubPackage::parse(b"not a zip at all").is_err());
    }
}
