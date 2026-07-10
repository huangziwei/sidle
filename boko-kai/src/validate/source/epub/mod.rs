//! EPUB-3 standalone validator. Catches format-level defects that would
//! either corrupt downstream KFX conversion or get rejected by strict
//! readers (Apple Books). Distinct from the pair-conversion validator at
//! [`crate::validate`], which compares semantic preservation across
//! formats; this one checks one EPUB against the EPUB-3 spec.
//!
//! Rules covered (focused on the failure modes we've actually hit):
//!
//! - `mimetype` is the first zip entry, `STORED`, exact 20-byte content
//!   `application/epub+zip` (EPUB 3.3 §3.4).
//! - `META-INF/container.xml` exists and points to an OPF that's in the zip.
//! - Every manifest `<item href>` resolves to a file inside the zip.
//! - Every zip entry under the OPF directory is declared in the manifest
//!   (besides `mimetype` and `META-INF/*`).
//! - Every spine `<itemref idref>` matches a manifest id.
//! - Exactly one manifest item carries `properties="nav"` (EPUB 3 nav doc).
//! - Every spine item with `linear="no"` is the target of an `<a href>`
//!   from another doc in the publication (EPUB 3.3 §5.8.2 reachability —
//!   the rule that fails downstream KFX conversion silently).
//! - Every `<a href>` in spine XHTML resolves to a file in the zip
//!   (fragments aren't checked yet — file-level only).
//! - No href in OPF or XHTML resolves to a path outside the OPF root
//!   directory. `..` parent segments inside the OPF tree are fine (e.g.
//!   `../style.css` from a chapter is legal); escapes above the OPF root
//!   are not — Apple Books rejects them silently.
//!
//! Out of scope (deferred): full XSD/RNG/Schematron validation, fragment
//! resolution within XHTML, CSS validation, content document
//! well-formedness. Shell out to W3C's `epubcheck` Java tool for those.

pub mod opf;

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

#[derive(Debug, Default)]
pub struct Report {
    pub violations: Vec<Violation>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn push(&mut self, v: Violation) {
        self.violations.push(v);
    }

    pub fn has_rule(&self, rule: Rule) -> bool {
        self.violations.iter().any(|v| v.rule == rule)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.violations.is_empty() {
            return write!(f, "epub3 validate: clean (0 violations)");
        }
        writeln!(f, "epub3 validate: {} violation(s)", self.violations.len())?;
        for v in &self.violations {
            writeln!(f, "  [{}] {}: {}", v.rule.as_str(), v.location, v.message)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Violation {
    pub rule: Rule,
    pub location: String,
    pub message: String,
}

impl Violation {
    fn new(rule: Rule, location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            rule,
            location: location.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    ZipMalformed,
    MimetypeNotFirst,
    MimetypeNotStored,
    MimetypeBadContent,
    MissingContainerXml,
    OpfMissing,
    OpfParseError,
    ManifestFileMissing,
    FileNotInManifest,
    SpineIdrefUnknown,
    NavMissing,
    NavDuplicated,
    NonLinearUnreachable,
    BrokenHref,
    HrefEscapesOpfRoot,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::ZipMalformed => "zip-malformed",
            Rule::MimetypeNotFirst => "mimetype-not-first",
            Rule::MimetypeNotStored => "mimetype-not-stored",
            Rule::MimetypeBadContent => "mimetype-bad-content",
            Rule::MissingContainerXml => "missing-container-xml",
            Rule::OpfMissing => "opf-missing",
            Rule::OpfParseError => "opf-parse-error",
            Rule::ManifestFileMissing => "manifest-file-missing",
            Rule::FileNotInManifest => "file-not-in-manifest",
            Rule::SpineIdrefUnknown => "spine-idref-unknown",
            Rule::NavMissing => "nav-missing",
            Rule::NavDuplicated => "nav-duplicated",
            Rule::NonLinearUnreachable => "non-linear-unreachable",
            Rule::BrokenHref => "broken-href",
            Rule::HrefEscapesOpfRoot => "href-escapes-opf-root",
        }
    }
}

/// Validate `epub_bytes` against the rules listed in the module docs.
/// Returns a [`Report`]; `report.is_clean()` is true iff no rule fired.
pub fn validate(epub_bytes: &[u8]) -> Report {
    let mut report = Report::default();

    check_mimetype_header(epub_bytes, &mut report);

    let cursor = Cursor::new(epub_bytes);
    let mut zip = match ZipArchive::new(cursor) {
        Ok(z) => z,
        Err(e) => {
            report.push(Violation::new(
                Rule::ZipMalformed,
                "<archive>",
                format!("{e}"),
            ));
            return report;
        }
    };

    let opf_path = match read_container_opf_path(&mut zip) {
        Ok(p) => p,
        Err(v) => {
            report.push(v);
            return report;
        }
    };

    let opf_text = match read_text(&mut zip, &opf_path) {
        Ok(t) => t,
        Err(_) => {
            report.push(Violation::new(
                Rule::OpfMissing,
                opf_path,
                "OPF path referenced by container.xml is not present in the zip",
            ));
            return report;
        }
    };

    let pkg = match opf::parse(&opf_text) {
        Ok(p) => p,
        Err(e) => {
            report.push(Violation::new(
                Rule::OpfParseError,
                opf_path,
                format!("{e}"),
            ));
            return report;
        }
    };

    let zip_paths: HashSet<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    let opf_dir = opf_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();

    check_manifest_files(&pkg, &opf_dir, &zip_paths, &opf_path, &mut report);
    check_files_in_manifest(&pkg, &opf_dir, &zip_paths, &opf_path, &mut report);
    check_spine_idrefs(&pkg, &opf_path, &mut report);
    check_nav_present(&pkg, &opf_path, &mut report);
    check_parent_paths_in_opf(&pkg, &opf_dir, &opf_path, &mut report);
    check_xhtml_hrefs_and_reachability(
        &pkg,
        &opf_dir,
        &mut zip,
        &zip_paths,
        &opf_path,
        &mut report,
    );

    report
}

// =========================================================================
// Rule: mimetype is first entry, STORED, exact "application/epub+zip"
// =========================================================================

fn check_mimetype_header(bytes: &[u8], report: &mut Report) {
    // Local file header layout: PK\x03\x04 (4) + version (2) + flags (2) +
    // compression (2) + time/date (4) + crc (4) + comp size (4) + uncomp
    // size (4) + name len (2) + extra len (2) = 30 bytes, then name, extra,
    // data. EPUB 3.3 §3.4 requires the first entry to be `mimetype` with
    // compression method 0 (STORED), no extra field, content equal to the
    // 20 bytes `application/epub+zip`.
    const REQUIRED: &[u8] = b"application/epub+zip";

    if bytes.len() < 30 + 8 + REQUIRED.len() {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            "file too small to contain a valid mimetype entry",
        ));
        return;
    }
    if &bytes[0..4] != b"PK\x03\x04" {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            "missing zip local-file-header signature at offset 0",
        ));
        return;
    }
    let compression = u16::from_le_bytes([bytes[8], bytes[9]]);
    let name_len = u16::from_le_bytes([bytes[26], bytes[27]]) as usize;
    let extra_len = u16::from_le_bytes([bytes[28], bytes[29]]) as usize;

    if bytes.len() < 30 + name_len + extra_len + REQUIRED.len() {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            "first zip entry header is truncated",
        ));
        return;
    }
    let name = &bytes[30..30 + name_len];
    if name != b"mimetype" {
        report.push(Violation::new(
            Rule::MimetypeNotFirst,
            "<archive>",
            format!(
                "first zip entry must be `mimetype`, found {:?}",
                String::from_utf8_lossy(name)
            ),
        ));
        return;
    }
    if compression != 0 {
        report.push(Violation::new(
            Rule::MimetypeNotStored,
            "mimetype",
            format!("compression method must be 0 (STORED), found {compression}"),
        ));
    }
    let content_start = 30 + name_len + extra_len;
    let content = &bytes[content_start..content_start + REQUIRED.len()];
    if content != REQUIRED {
        report.push(Violation::new(
            Rule::MimetypeBadContent,
            "mimetype",
            format!(
                "expected {:?}, found {:?}",
                String::from_utf8_lossy(REQUIRED),
                String::from_utf8_lossy(content),
            ),
        ));
    }
}

// =========================================================================
// Container.xml → OPF path
// =========================================================================

fn read_container_opf_path(zip: &mut ZipArchive<Cursor<&[u8]>>) -> Result<String, Violation> {
    let text = read_text(zip, "META-INF/container.xml").map_err(|_| {
        Violation::new(
            Rule::MissingContainerXml,
            "META-INF/container.xml",
            "container.xml is missing from the zip",
        )
    })?;
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref() == b"rootfile" => {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"full-path" {
                        return Ok(String::from_utf8_lossy(&attr.value).to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(Violation::new(
                    Rule::MissingContainerXml,
                    "META-INF/container.xml",
                    format!("parse error: {e}"),
                ));
            }
            _ => {}
        }
    }
    Err(Violation::new(
        Rule::MissingContainerXml,
        "META-INF/container.xml",
        "no <rootfile full-path=...> entry",
    ))
}

// =========================================================================
// Manifest ↔ files
// =========================================================================

fn check_manifest_files(
    pkg: &opf::Package,
    opf_dir: &str,
    zip_paths: &HashSet<String>,
    opf_path: &str,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        let resolved = join_opf(opf_dir, &item.href);
        if !zip_paths.contains(&resolved) {
            report.push(Violation::new(
                Rule::ManifestFileMissing,
                opf_path,
                format!(
                    "manifest item id={:?} href={:?} -> {:?} is not present in the zip",
                    item.id, item.href, resolved
                ),
            ));
        }
    }
}

fn check_files_in_manifest(
    pkg: &opf::Package,
    opf_dir: &str,
    zip_paths: &HashSet<String>,
    opf_path: &str,
    report: &mut Report,
) {
    let manifest_paths: HashSet<String> = pkg
        .manifest
        .iter()
        .map(|m| join_opf(opf_dir, &m.href))
        .collect();
    for path in zip_paths {
        if path == "mimetype" || path.starts_with("META-INF/") || path == opf_path {
            continue;
        }
        if path.ends_with('/') {
            continue;
        }
        if manifest_paths.contains(path) {
            continue;
        }
        report.push(Violation::new(
            Rule::FileNotInManifest,
            opf_path,
            format!("zip entry {:?} is not declared in the manifest", path),
        ));
    }
}

fn check_spine_idrefs(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let ids: HashSet<&str> = pkg.manifest.iter().map(|m| m.id.as_str()).collect();
    for s in &pkg.spine {
        if !ids.contains(s.idref.as_str()) {
            report.push(Violation::new(
                Rule::SpineIdrefUnknown,
                opf_path,
                format!("spine idref={:?} does not match any manifest id", s.idref),
            ));
        }
    }
}

// =========================================================================
// Nav doc
// =========================================================================

fn check_nav_present(pkg: &opf::Package, opf_path: &str, report: &mut Report) {
    let nav_items: Vec<&opf::ManifestItem> = pkg
        .manifest
        .iter()
        .filter(|m| m.properties.iter().any(|p| p == "nav"))
        .collect();
    match nav_items.len() {
        0 => report.push(Violation::new(
            Rule::NavMissing,
            opf_path,
            "no manifest item has properties=\"nav\" (required by EPUB 3)",
        )),
        1 => {}
        n => report.push(Violation::new(
            Rule::NavDuplicated,
            opf_path,
            format!("{n} manifest items have properties=\"nav\"; exactly one is allowed"),
        )),
    }
}

// =========================================================================
// XHTML href scan: reachability for non-linear spine + broken href + parent paths
// =========================================================================

fn check_xhtml_hrefs_and_reachability(
    pkg: &opf::Package,
    opf_dir: &str,
    zip: &mut ZipArchive<Cursor<&[u8]>>,
    zip_paths: &HashSet<String>,
    opf_path: &str,
    report: &mut Report,
) {
    // Collect every internal target referenced by an `<a href>` (or
    // `<link href>`) inside any XHTML in the manifest. Paths are resolved
    // relative to the XHTML they appear in and stripped of fragment.
    let mut hyperlink_targets: HashSet<String> = HashSet::new();

    for item in &pkg.manifest {
        if !is_xhtml(&item.media_type) {
            continue;
        }
        let path = join_opf(opf_dir, &item.href);
        let Ok(text) = read_text(zip, &path) else {
            continue;
        };
        let hrefs = collect_xhtml_hrefs(&text);
        for href in hrefs {
            if let Some(resolved) = resolve_href(&path, &href) {
                hyperlink_targets.insert(resolved.clone());
                if !zip_paths.contains(&resolved) {
                    report.push(Violation::new(
                        Rule::BrokenHref,
                        path.clone(),
                        format!(
                            "<a href={:?}> -> {:?} not present in the zip",
                            href, resolved
                        ),
                    ));
                }
                if escapes_opf_root(opf_dir, &resolved) {
                    report.push(Violation::new(
                        Rule::HrefEscapesOpfRoot,
                        path.clone(),
                        format!(
                            "href={:?} resolves to {:?}, which is outside the OPF root {:?}",
                            href, resolved, opf_dir
                        ),
                    ));
                }
            }
        }
    }

    // Reachability: every spine item with `linear="no"` must be the target of
    // some hyperlink elsewhere in the publication.
    for s in &pkg.spine {
        if s.linear != Some(false) {
            continue;
        }
        let Some(item) = pkg.manifest_by_id(&s.idref) else {
            continue; // already reported by check_spine_idrefs
        };
        let resolved = join_opf(opf_dir, &item.href);
        if !hyperlink_targets.contains(&resolved) {
            report.push(Violation::new(
                Rule::NonLinearUnreachable,
                opf_path,
                format!(
                    "spine item idref={:?} (href={:?}) has linear=\"no\" but no <a href> in the publication points to it (EPUB 3.3 §5.8.2)",
                    s.idref, resolved
                ),
            ));
        }
    }
}

fn check_parent_paths_in_opf(
    pkg: &opf::Package,
    opf_dir: &str,
    opf_path: &str,
    report: &mut Report,
) {
    for item in &pkg.manifest {
        let resolved = join_opf(opf_dir, &item.href);
        if escapes_opf_root(opf_dir, &resolved) {
            report.push(Violation::new(
                Rule::HrefEscapesOpfRoot,
                opf_path,
                format!(
                    "manifest item href={:?} resolves to {:?}, outside the OPF root {:?}",
                    item.href, resolved, opf_dir
                ),
            ));
        }
    }
}

fn collect_xhtml_hrefs(content: &str) -> Vec<String> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                if local == b"a" || local == b"link" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"href" {
                            out.push(String::from_utf8_lossy(&attr.value).to_string());
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // XHTML well-formedness is out of scope here.
            _ => {}
        }
    }
    out
}

// =========================================================================
// Path helpers
// =========================================================================

fn join_opf(opf_dir: &str, href: &str) -> String {
    if opf_dir.is_empty() {
        href.to_string()
    } else {
        format!("{}/{}", opf_dir, href)
    }
}

/// Resolve `href` against the directory of `source_path`. Returns `None`
/// for external URLs (`http:`, `mailto:`, …), pure fragments, and empty
/// hrefs. The result is the zip-relative path of the link target with the
/// fragment stripped.
fn resolve_href(source_path: &str, href: &str) -> Option<String> {
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    if href.contains("://")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
        || href.starts_with("data:")
    {
        return None;
    }
    let no_frag = href.split('#').next().unwrap_or(href);
    if no_frag.is_empty() {
        return None;
    }
    let source_dir = source_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut parts: Vec<&str> = if source_dir.is_empty() {
        Vec::new()
    } else {
        source_dir.split('/').collect()
    };
    for seg in no_frag.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// True when `resolved` (a zip-relative path produced by [`resolve_href`] or
/// [`join_opf`]) is outside the OPF root directory. `OEBPS/text/foo.xhtml`
/// with `<a href="../style.css">` resolves to `OEBPS/style.css` (inside
/// `OEBPS/` — fine); the same chapter with `<a href="../../escape.xhtml">`
/// resolves to `escape.xhtml` at zip root (outside `OEBPS/` — flagged).
fn escapes_opf_root(opf_dir: &str, resolved: &str) -> bool {
    if opf_dir.is_empty() {
        return false;
    }
    let prefix = format!("{}/", opf_dir);
    !resolved.starts_with(&prefix) && resolved != opf_dir
}

fn is_xhtml(media_type: &str) -> bool {
    media_type.eq_ignore_ascii_case("application/xhtml+xml")
}

fn read_text(zip: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> io::Result<String> {
    let mut entry = zip.by_name(name)?;
    let mut s = String::new();
    entry.read_to_string(&mut s)?;
    Ok(s)
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aozora::{Document, EpubInput, TocEntry, build_epub};

    fn tiny_jpeg() -> Vec<u8> {
        // Same 1x1 JPEG used by the epub_builder tests — small, valid header.
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    fn sample_aozora_epub() -> Vec<u8> {
        let doc = Document {
            title: "テスト".to_string(),
            author: "著者".to_string(),
            body_xhtml: r#"<p>序文</p><h2 id="h1">第一章</h2><p>本文</p>"#.to_string(),
            toc: vec![TocEntry {
                id: "h1".to_string(),
                level: 2,
                text: "第一章".to_string(),
            }],
            colophon: String::new(),
            referenced_images: vec![],
        };
        build_epub(EpubInput {
            document: &doc,
            images: &[],
            cover_jpeg: &tiny_jpeg(),
        })
        .unwrap()
    }

    #[test]
    fn aozora_output_passes_after_fix() {
        let bytes = sample_aozora_epub();
        let report = validate(&bytes);
        assert!(
            report.is_clean(),
            "aozora epub should validate clean; got:\n{}",
            report
        );
    }

    #[test]
    fn detects_non_linear_cover_without_hyperlink() {
        // Re-introduce the bug locally: take a valid aozora epub and rewrite
        // the OPF to set `linear="no"` on the cover. Validator must catch it.
        let bytes = sample_aozora_epub();
        let mutated = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
            opf.replace(
                r#"<itemref idref="cover"/>"#,
                r#"<itemref idref="cover" linear="no"/>"#,
            )
        });
        let report = validate(&mutated);
        assert!(
            report.has_rule(Rule::NonLinearUnreachable),
            "expected NonLinearUnreachable, got:\n{}",
            report
        );
    }

    #[test]
    fn detects_missing_manifest_file() {
        // Rewrite OPF to reference a nonexistent file.
        let bytes = sample_aozora_epub();
        let mutated = rewrite_zip_entry(&bytes, "OEBPS/content.opf", |opf| {
            opf.replace(
                r#"<item id="style" href="style.css" media-type="text/css"/>"#,
                r#"<item id="style" href="nope.css" media-type="text/css"/>"#,
            )
        });
        let report = validate(&mutated);
        assert!(
            report.has_rule(Rule::ManifestFileMissing),
            "expected ManifestFileMissing, got:\n{}",
            report
        );
    }

    #[test]
    fn detects_href_escaping_opf_root() {
        // OPF root is `OEBPS/`. Insert a `../../escape.xhtml` from a chapter:
        // resolves to `escape.xhtml` (zip root, outside OEBPS) — must flag.
        // (A plain `../style.css` would resolve to `OEBPS/style.css` and is
        // therefore fine — verified by `aozora_output_passes_after_fix`.)
        let bytes = sample_aozora_epub();
        let mutated = rewrite_zip_entry(&bytes, "OEBPS/text/title.xhtml", |xhtml| {
            xhtml.replace("</body>", r#"<a href="../../escape.xhtml">link</a></body>"#)
        });
        let report = validate(&mutated);
        assert!(
            report.has_rule(Rule::HrefEscapesOpfRoot),
            "expected HrefEscapesOpfRoot, got:\n{}",
            report
        );
    }

    /// Rebuild `epub_bytes` with `entry`'s content rewritten by `f`. Uses the
    /// `zip` crate to iterate entries and emit a new archive with the same
    /// per-entry compression methods. Preserves mimetype-first ordering.
    fn rewrite_zip_entry(epub_bytes: &[u8], target: &str, f: impl Fn(String) -> String) -> Vec<u8> {
        use zip::write::{SimpleFileOptions, ZipWriter};
        let cursor = Cursor::new(epub_bytes);
        let mut zin = ZipArchive::new(cursor).unwrap();
        let mut out = Vec::with_capacity(epub_bytes.len());
        let mut zout = ZipWriter::new(Cursor::new(&mut out));
        for i in 0..zin.len() {
            let mut e = zin.by_index(i).unwrap();
            let name = e.name().to_string();
            let comp = e.compression();
            let opts: SimpleFileOptions = SimpleFileOptions::default().compression_method(comp);
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).unwrap();
            zout.start_file(&name, opts).unwrap();
            if name == target {
                let s = String::from_utf8(buf).unwrap();
                let rewritten = f(s);
                std::io::Write::write_all(&mut zout, rewritten.as_bytes()).unwrap();
            } else {
                std::io::Write::write_all(&mut zout, &buf).unwrap();
            }
        }
        zout.finish().unwrap();
        out
    }
}
