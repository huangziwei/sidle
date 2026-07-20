//! TOC validation — is a book's declared table of contents properly formed?
//!
//! Source-native and single-file: given one book (KFX or EPUB), compare its
//! **declared** TOC (the KFX `nav_container` toc / the EPUB nav doc or NCX —
//! what the reader's chapter sidebar shows) against the book's **own in-book
//! chapter list** (a Contents page's links, styled headings, or numbered section
//! starts). A book whose declared TOC is chapterless while the content clearly
//! has chapters is *not properly formatted* — flag it.
//!
//! This is a validator, so it reads only the one source format it's handed; it
//! never consults a converted/derived copy (an EPUB derived from a KFX, or vice
//! versa, gets no say). The KFX extractor lives in `kfx`, the EPUB extractor in
//! `epub`; both feed the one shared [`classify`] rule.

mod epub;
mod kfx;

/// Format-neutral evidence extracted from a book — the shared input to
/// [`classify`], so the KFX and EPUB extractors share one verdict rule. Each
/// format fills these from its own structures; the meaning is identical: the
/// *declared* TOC vs the *in-book* chapter list.
#[derive(Debug, Clone, Default)]
pub struct TocEvidence {
    /// The declared TOC entry labels (KFX `nav_container` toc / EPUB nav or NCX).
    pub nav_labels: Vec<String>,
    /// Distinct internal chapter links on the in-book Contents page (densest
    /// cluster; the marked toc page when present).
    pub contents_links: usize,
    /// A few chapter-link labels from that page, for the review report.
    pub contents_sample: Vec<String>,
    /// Heading elements in the content (KFX styled headings / EPUB `<hN>`).
    pub headings: usize,
    /// Content divisions whose first text is a bare chapter marker (number /
    /// 第N章 / Chapter N).
    pub section_heads: usize,
    /// The book marks a Contents page as the TOC (KFX `toc` landmark / EPUB
    /// guide-or-nav `toc` landmark).
    pub has_toc_landmark: bool,
}

/// The validation report for one book's TOC.
#[derive(Debug, Clone)]
pub struct TocAudit {
    /// Total declared-TOC entries (recursive, incl. nested).
    pub nav_count: usize,
    /// Flattened declared-TOC entry labels.
    pub nav_labels: Vec<String>,
    /// Declared entries whose label is not front-matter/boilerplate — the real
    /// chapter entries the reader can navigate by.
    pub nav_chapters: usize,
    /// TOC is present but every entry is front-matter (表紙/目次/奥付/Cover/…).
    pub fm_only: bool,
    /// Distinct internal chapter links on the in-book Contents page.
    pub contents_links: usize,
    /// A few chapter-link labels from that page, for the review report.
    pub contents_sample: Vec<String>,
    /// Heading elements in the content (styled headings / `<hN>`).
    pub headings: usize,
    /// Content divisions whose first text is a bare chapter marker.
    pub section_heads: usize,
    /// The book marks a Contents page as the TOC.
    pub has_toc_landmark: bool,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Declared TOC already lists chapters (or no stronger in-book evidence).
    Ok,
    /// Declared TOC is chapterless but the book's own content lists many chapters
    /// — the TOC is deficient / malformed. Candidate for confirm-then-fix.
    Suspect,
    /// Declared TOC is chapterless and there's no machine-readable in-book chapter
    /// list either. May be a genuinely flat book, or chapters that no signal
    /// caught. Left alone until a human looks.
    Sparse,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "OK",
            Verdict::Suspect => "SUSPECT",
            Verdict::Sparse => "SPARSE",
        }
    }
}

/// Minimum in-book chapter signals for the evidence to count as a real chapter
/// list (below this, a stray forward link or two is just noise).
const MIN_EVIDENCE: usize = 5;

/// A declared TOC with more real chapter entries than this is considered healthy
/// and is never flagged, however many internal links the body carries (footnote /
/// index / cross-reference links inflate the raw link count but say nothing about
/// the TOC).
const MAX_NAV_CHAPTERS_TO_FLAG: usize = 2;

/// Validate a book's TOC from its bytes. Sniffs the format (EPUB = zip `PK`, KFX
/// = `CONT`) and runs the matching source-native extractor. Never reads a
/// derived copy.
pub fn validate(bytes: &[u8]) -> Result<TocAudit, String> {
    if bytes.starts_with(b"PK") {
        Ok(classify(epub::evidence(bytes)?))
    } else {
        let book = crate::formats::kfx::loader::load(bytes).map_err(|e| e.to_string())?;
        Ok(classify(kfx::evidence(&book)))
    }
}

/// Turn format-neutral evidence into a verdict. The one rule both formats share:
/// a declared TOC with >2 real chapter entries is always OK (footnote / index /
/// cross-reference links never flag it); SUSPECT only when the TOC is chapterless
/// AND the book itself carries ≥`MIN_EVIDENCE` chapters the TOC omits; else
/// SPARSE.
pub fn classify(ev: TocEvidence) -> TocAudit {
    let nav_count = ev.nav_labels.len();
    let nav_chapters = ev.nav_labels.iter().filter(|l| !is_front_matter(l)).count();
    let fm_only = nav_count > 0 && nav_chapters == 0;

    // Ground-truth chapter count = the strongest of the three in-book signals.
    let evidence = ev.contents_links.max(ev.headings).max(ev.section_heads);
    let verdict = if nav_chapters > MAX_NAV_CHAPTERS_TO_FLAG {
        Verdict::Ok
    } else if evidence >= MIN_EVIDENCE && evidence > nav_count {
        Verdict::Suspect
    } else {
        Verdict::Sparse
    };

    TocAudit {
        nav_count,
        nav_labels: ev.nav_labels,
        nav_chapters,
        fm_only,
        contents_links: ev.contents_links,
        contents_sample: ev.contents_sample,
        headings: ev.headings,
        section_heads: ev.section_heads,
        has_toc_landmark: ev.has_toc_landmark,
        verdict,
    }
}

impl TocAudit {
    /// A validation pass = the declared TOC is not deficient. SPARSE (no chapter
    /// evidence found) is inconclusive, not a failure.
    pub fn is_clean(&self) -> bool {
        self.verdict != Verdict::Suspect
    }

    pub fn print_summary(&self) {
        println!(
            "{}  nav_toc={} (chapters={}, fm_only={})  evidence: {} links / {} headings / {} section-heads  toc_landmark={}",
            self.verdict.as_str(),
            self.nav_count,
            self.nav_chapters,
            self.fm_only,
            self.contents_links,
            self.headings,
            self.section_heads,
            self.has_toc_landmark,
        );
        if !self.nav_labels.is_empty() {
            let shown: Vec<&str> = self
                .nav_labels
                .iter()
                .take(12)
                .map(|s| s.as_str())
                .collect();
            println!("  nav labels: {}", shown.join(" · "));
        }
    }

    /// Lower this TOC audit into the unified
    /// [`Finding`](crate::validate::Finding) model. A `Suspect`
    /// verdict — declared TOC chapterless while the book itself lists chapters —
    /// is the one defect this check reports; `Ok` and `Sparse` are clean /
    /// inconclusive and yield nothing. Consumed by
    /// [`crate::validate::source::validate`].
    pub fn into_findings(self) -> Vec<crate::validate::Finding> {
        use crate::validate::{Finding, FixHint, Severity};
        if self.verdict != Verdict::Suspect {
            return Vec::new();
        }
        let in_book = self
            .contents_links
            .max(self.headings)
            .max(self.section_heads);
        vec![Finding {
            check: "toc",
            rule: "toc-deficient".to_string(),
            severity: Severity::Warning,
            location: "<toc>".to_string(),
            message: format!(
                "declared TOC lists {} chapter entr{} but the book has {in_book} in-book chapters",
                self.nav_chapters,
                if self.nav_chapters == 1 { "y" } else { "ies" },
            ),
            fix: Some(FixHint::new(
                "rebuild-toc",
                "rebuild the declared TOC from the book's in-book chapter list",
            )),
        }]
    }
}

/// Front-matter / boilerplate TOC labels (JP + EN). A declared TOC made only of
/// these has no chapters. Kept intentionally broad — the cost of a false "front
/// matter" is only that an entry doesn't count toward `nav_chapters`, and the
/// flag still requires strong positive in-book evidence. Shared with the EPUB
/// TOC repairer, which counts an existing TOC's real chapters before reusing it.
pub(crate) fn is_front_matter(label: &str) -> bool {
    let l = label.trim();
    const JP: &[&str] = &[
        "表紙",
        "奥付",
        "目次",
        "もくじ",
        "カバー",
        "扉",
        "中扉",
        "口絵",
        "表題",
        "本扉",
        "標題",
        "凡例",
        "序文",
        "序",
    ];
    for p in JP {
        if l.starts_with(p) {
            return true;
        }
    }
    let low = l.to_ascii_lowercase();
    const EN: &[&str] = &[
        "cover",
        "contents",
        "table of contents",
        "title",
        "copyright",
        "colophon",
        "dedication",
        "about the author",
        "about the publisher",
        "praise",
        "also by",
        "other books",
        "acknowledg",
        "index",
        "front matter",
        "back matter",
        "half title",
    ];
    EN.iter().any(|p| low.starts_with(p))
}

/// Whether a short line reads as a standalone chapter marker: a bare number
/// (`1`, `12`), a Japanese `第N章/部/話/節`, or an English `Chapter N`. Kept tight
/// so prose first-lines and dropcaps (a single letter) don't match. Shared by the
/// KFX and EPUB section-start detectors.
fn is_chapter_marker(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    // Bare number (Western or full-width digits), up to 4 digits.
    let digits: String = t
        .chars()
        .map(|c| match c {
            '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
            other => other,
        })
        .collect();
    if digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let n = t.chars().count();
    // 第…章 / 第…部 / 第…話 / 第…節 (short).
    if t.starts_with('第')
        && n <= 12
        && (t.contains('章') || t.contains('部') || t.contains('話') || t.contains('節'))
    {
        return true;
    }
    // Chapter N.
    let low = t.to_ascii_lowercase();
    if n <= 20 && (low.starts_with("chapter ") || low.starts_with("chapter\u{a0}")) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_matches_expected() {
        for s in [
            "表紙",
            "目次",
            "奥付",
            "Cover",
            "Contents",
            "Copyright",
            "About the Author",
        ] {
            assert!(is_front_matter(s), "{s} should be front matter");
        }
        for s in [
            "第一章",
            "Chapter 1",
            "一　目撃者",
            "プロローグ",
            "The High Window",
        ] {
            assert!(!is_front_matter(s), "{s} should not be front matter");
        }
    }

    #[test]
    fn chapter_marker_matches_bare_numbers_and_headings() {
        for s in [
            "1",
            "12",
            "１０",
            "２０",
            "第一章",
            "第三部",
            "第2話",
            "Chapter 5",
        ] {
            assert!(is_chapter_marker(s), "{s} should be a chapter marker");
        }
        for s in [
            "T",
            "SCRIBNER",
            "He was an old man who fished alone",
            "12345",
            "登場人物",
            "はじめに",
        ] {
            assert!(!is_chapter_marker(s), "{s} should not be a chapter marker");
        }
    }

    #[test]
    fn classify_gate_and_flag() {
        // A rich declared TOC is never flagged, however many body links exist.
        let ok = classify(TocEvidence {
            nav_labels: (0..30).map(|i| format!("Chapter {i}")).collect(),
            contents_links: 5000,
            ..Default::default()
        });
        assert_eq!(ok.verdict, Verdict::Ok);

        // Chapterless declared TOC + a real in-book chapter list = deficient.
        let bad = classify(TocEvidence {
            nav_labels: vec!["表紙".into(), "奥付".into()],
            headings: 20,
            ..Default::default()
        });
        assert_eq!(bad.verdict, Verdict::Suspect);

        // Chapterless TOC, no evidence = inconclusive, not a failure.
        let flat = classify(TocEvidence {
            nav_labels: vec!["Cover".into(), "Copyright".into()],
            ..Default::default()
        });
        assert_eq!(flat.verdict, Verdict::Sparse);
    }

    #[test]
    fn into_findings_only_flags_suspect() {
        use crate::validate::Severity;

        // OK verdict -> no finding.
        let ok = classify(TocEvidence {
            nav_labels: (0..30).map(|i| format!("Chapter {i}")).collect(),
            contents_links: 5000,
            ..Default::default()
        });
        assert!(ok.into_findings().is_empty());

        // SPARSE verdict -> no finding (inconclusive, not a defect).
        let sparse = classify(TocEvidence {
            nav_labels: vec!["Cover".into(), "Copyright".into()],
            ..Default::default()
        });
        assert!(sparse.into_findings().is_empty());

        // SUSPECT verdict -> exactly one Warning carrying a rebuild-toc fix.
        let suspect = classify(TocEvidence {
            nav_labels: vec!["表紙".into(), "奥付".into()],
            headings: 20,
            ..Default::default()
        });
        let findings = suspect.into_findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "toc");
        assert_eq!(findings[0].rule, "toc-deficient");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert_eq!(findings[0].fix.as_ref().unwrap().action, "rebuild-toc");
    }
}
