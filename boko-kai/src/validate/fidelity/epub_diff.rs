//! Strict A/B tree diff between two EPUBs — the 1:1 gate for converging the
//! two KFX→EPUB routes.
//!
//! Unlike the sibling fidelity checks, both inputs are EPUBs: `a` is the
//! oracle (today: the mechanical `kfx_to_epub` output) and `b` the candidate
//! (the IR route). The comparison is deliberately byte-exact per zip entry —
//! no canonicalization, no "semantically equivalent" tolerance — because the
//! two routes are being converged onto shared emitters where equality is
//! structural. Zip-level details (entry order, compression, timestamps) are
//! out of scope: the tree is compared by entry name.
//!
//! Differences are classified by artifact kind so a corpus sweep can rank
//! convergence work by diff-class frequency.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use zip::ZipArchive;

/// What kind of EPUB artifact a zip entry is — the diff-class key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtifactKind {
    Mimetype,
    Container,
    Opf,
    Nav,
    Ncx,
    Css,
    Xhtml,
    Image,
    Font,
    Other,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Mimetype => "mimetype",
            ArtifactKind::Container => "container",
            ArtifactKind::Opf => "opf",
            ArtifactKind::Nav => "nav",
            ArtifactKind::Ncx => "ncx",
            ArtifactKind::Css => "css",
            ArtifactKind::Xhtml => "xhtml",
            ArtifactKind::Image => "image",
            ArtifactKind::Font => "font",
            ArtifactKind::Other => "other",
        }
    }

    /// Classify a zip entry path. `nav.xhtml` outranks the generic xhtml
    /// bucket because the navigation document has its own emitter.
    pub fn classify(path: &str) -> Self {
        let lower = path.to_ascii_lowercase();
        if lower == "mimetype" {
            return ArtifactKind::Mimetype;
        }
        if lower.starts_with("meta-inf/") {
            return ArtifactKind::Container;
        }
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        if name == "nav.xhtml" {
            return ArtifactKind::Nav;
        }
        match name.rsplit('.').next().unwrap_or("") {
            "opf" => ArtifactKind::Opf,
            "ncx" => ArtifactKind::Ncx,
            "css" => ArtifactKind::Css,
            "xhtml" | "html" | "htm" => ArtifactKind::Xhtml,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "jxr" => ArtifactKind::Image,
            "otf" | "ttf" | "woff" | "woff2" => ArtifactKind::Font,
            _ => ArtifactKind::Other,
        }
    }
}

/// One entry present in both trees whose bytes differ.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub kind: ArtifactKind,
    pub a_len: usize,
    pub b_len: usize,
    /// Byte offset of the first difference (== min(a_len, b_len) when one
    /// side is a strict prefix of the other).
    pub first_diff: usize,
    /// Printable context around the first difference, one line per side.
    pub a_context: String,
    pub b_context: String,
}

/// Result of a strict tree comparison of two EPUBs.
#[derive(Debug, Default)]
pub struct Report {
    /// Entry names only present in A (the oracle).
    pub only_in_a: Vec<String>,
    /// Entry names only present in B (the candidate).
    pub only_in_b: Vec<String>,
    /// Entries present in both but byte-different.
    pub differing: Vec<FileDiff>,
    /// Entries present in both and byte-identical.
    pub identical: usize,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.only_in_a.is_empty() && self.only_in_b.is_empty() && self.differing.is_empty()
    }

    /// Diff-class tallies over every non-identical entry (missing/extra
    /// entries count toward their kind too, since a missing file is emitter
    /// work of that class).
    pub fn class_tally(&self) -> BTreeMap<ArtifactKind, usize> {
        let mut tally: BTreeMap<ArtifactKind, usize> = BTreeMap::new();
        for p in self.only_in_a.iter().chain(self.only_in_b.iter()) {
            *tally.entry(ArtifactKind::classify(p)).or_insert(0) += 1;
        }
        for d in &self.differing {
            *tally.entry(d.kind).or_insert(0) += 1;
        }
        tally
    }

    pub fn print_summary(&self) {
        if self.is_clean() {
            println!(
                "epub-diff: IDENTICAL — {} entries byte-equal",
                self.identical
            );
            return;
        }
        println!(
            "epub-diff: DIFFERENT — {} identical, {} differing, {} only in A, {} only in B",
            self.identical,
            self.differing.len(),
            self.only_in_a.len(),
            self.only_in_b.len()
        );
        for (kind, n) in self.class_tally() {
            println!("  class {:<9} {}", kind.as_str(), n);
        }
    }

    pub fn print_details(&self, limit: usize) {
        for p in self.only_in_a.iter().take(limit) {
            println!("  only in A: {p}");
        }
        for p in self.only_in_b.iter().take(limit) {
            println!("  only in B: {p}");
        }
        for d in self.differing.iter().take(limit) {
            println!(
                "  differs [{}] {} (A {} B, first diff @{})",
                d.kind.as_str(),
                d.path,
                if d.a_len == d.b_len {
                    format!("{} bytes ==", d.a_len)
                } else {
                    format!("{} vs {} bytes", d.a_len, d.b_len)
                },
                d.first_diff
            );
            println!("    A: {}", d.a_context);
            println!("    B: {}", d.b_context);
        }
    }
}

/// Read every entry of an EPUB zip into (name → bytes). Directory entries
/// are skipped; duplicate names keep the last occurrence (mirrors how
/// readers resolve them).
fn read_tree(bytes: &[u8], label: &str) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("{label}: not a readable zip: {e}"))?;
    let mut tree = BTreeMap::new();
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("{label}: zip entry {i}: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut data)
            .map_err(|e| format!("{label}: reading {name}: {e}"))?;
        tree.insert(name, data);
    }
    Ok(tree)
}

/// Printable snippet of `bytes` around `offset` (±40 bytes), with control
/// bytes escaped so binary diffs stay one-line readable.
fn context_snippet(bytes: &[u8], offset: usize) -> String {
    let start = offset.saturating_sub(40);
    let end = (offset + 40).min(bytes.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    for &b in &bytes[start..end] {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            // Multi-byte UTF-8 passes through byte-wise; lossy is fine for
            // a debugging snippet.
            _ if b >= 0x80 => out.push_str(&String::from_utf8_lossy(&[b])),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    if end < bytes.len() {
        out.push('…');
    }
    out
}

fn first_diff_offset(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    a[..n]
        .iter()
        .zip(&b[..n])
        .position(|(x, y)| x != y)
        .unwrap_or(n)
}

/// Compare two EPUBs entry-by-entry. `a` is the oracle, `b` the candidate.
pub fn validate(a_bytes: &[u8], b_bytes: &[u8]) -> Result<Report, String> {
    let a = read_tree(a_bytes, "A")?;
    let b = read_tree(b_bytes, "B")?;

    let mut report = Report::default();

    for (path, a_data) in &a {
        match b.get(path) {
            None => report.only_in_a.push(path.clone()),
            Some(b_data) if a_data == b_data => report.identical += 1,
            Some(b_data) => {
                let off = first_diff_offset(a_data, b_data);
                report.differing.push(FileDiff {
                    path: path.clone(),
                    kind: ArtifactKind::classify(path),
                    a_len: a_data.len(),
                    b_len: b_data.len(),
                    first_diff: off,
                    a_context: context_snippet(a_data, off),
                    b_context: context_snippet(b_data, off),
                });
            }
        }
    }
    for path in b.keys() {
        if !a.contains_key(path) {
            report.only_in_b.push(path.clone());
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut cursor);
        for (name, data) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        cursor.into_inner()
    }

    #[test]
    fn identical_trees_are_clean() {
        let z = make_zip(&[("mimetype", b"application/epub+zip"), ("a.xhtml", b"<x/>")]);
        let report = validate(&z, &z).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.identical, 2);
    }

    #[test]
    fn entry_order_does_not_matter() {
        let a = make_zip(&[("a.xhtml", b"one"), ("b.xhtml", b"two")]);
        let b = make_zip(&[("b.xhtml", b"two"), ("a.xhtml", b"one")]);
        assert!(validate(&a, &b).unwrap().is_clean());
    }

    #[test]
    fn classifies_and_reports_differences() {
        let a = make_zip(&[
            ("OEBPS/content.opf", b"<opf>a</opf>"),
            ("OEBPS/c0.xhtml", b"<p>hi</p>"),
            ("OEBPS/style.css", b"p{}"),
        ]);
        let b = make_zip(&[
            ("OEBPS/content.opf", b"<opf>b</opf>"),
            ("OEBPS/chapter_0.xhtml", b"<p>hi</p>"),
            ("OEBPS/style.css", b"p{}"),
        ]);
        let report = validate(&a, &b).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.identical, 1); // style.css
        assert_eq!(report.only_in_a, vec!["OEBPS/c0.xhtml".to_string()]);
        assert_eq!(report.only_in_b, vec!["OEBPS/chapter_0.xhtml".to_string()]);
        assert_eq!(report.differing.len(), 1);
        let d = &report.differing[0];
        assert_eq!(d.kind, ArtifactKind::Opf);
        assert_eq!(d.first_diff, 5);
        let tally = report.class_tally();
        assert_eq!(tally[&ArtifactKind::Xhtml], 2);
        assert_eq!(tally[&ArtifactKind::Opf], 1);
    }

    #[test]
    fn prefix_diff_offset_is_shorter_len() {
        let a = make_zip(&[("x.css", b"abc")]);
        let b = make_zip(&[("x.css", b"abcdef")]);
        let report = validate(&a, &b).unwrap();
        assert_eq!(report.differing[0].first_diff, 3);
    }

    #[test]
    fn nav_outranks_generic_xhtml() {
        assert_eq!(ArtifactKind::classify("OEBPS/nav.xhtml"), ArtifactKind::Nav);
        assert_eq!(
            ArtifactKind::classify("OEBPS/c1.xhtml"),
            ArtifactKind::Xhtml
        );
        assert_eq!(ArtifactKind::classify("mimetype"), ArtifactKind::Mimetype);
        assert_eq!(
            ArtifactKind::classify("META-INF/container.xml"),
            ArtifactKind::Container
        );
        assert_eq!(
            ArtifactKind::classify("OEBPS/images/img0.jxr"),
            ArtifactKind::Image
        );
    }
}
