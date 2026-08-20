//! Book validation, grouped by the question each check answers and who
//! consumes the answer:
//!
//! - [`source`] — **is one book file well-formed on its own?** Single-input
//!   structural checks (`source::epub` = a Rust epubcheck replacement,
//!   `source::toc` = a cross-format declared-TOC audit). These flag defects
//!   **in the source book** and feed the book editor's repair list.
//! - [`fidelity`] — **did EPUB ⇄ KFX conversion lose anything?** Pair-input
//!   diffs (`validate(epub_bytes, kfx_bytes)`) comparing semantic preservation
//!   across a conversion. A loss here is a bokai converter bug, so these are the
//!   CI checks. Direction-aware (see [`Direction`]).
//! - [`coverage`] — **what does bokai not handle yet?** Aggregate reports on
//!   bokai's own parser coverage (unmapped HTML tags, dropped CSS properties).
//!   These are roadmap tools, **not** book validators — they never judge a book
//!   right or wrong.
//!
//! The `source` extractors read one format natively and never consult a
//! derived/converted copy. The `fidelity` extractors deliberately parse the
//! EPUB side with independent, minimal tokenization (NOT bokai's IR) so a
//! parser-side bug surfaces here instead of being mirrored on both sides; the
//! KFX side reuses bokai's own KFX parser (the format is shared with the reader
//! anyway).
//!
//! Calibre's output is NEVER ground truth. Ground truth is per-direction: the
//! SOURCE book for an import check, the target format's own specification and
//! the devices that render it for an export check — never another converter's
//! rendering of the same book.

pub mod coverage;
pub mod fidelity;
pub mod source;

use std::fmt;

// ============================================================================
// Unified source-finding model
// ============================================================================
//
// One shape for every [`source`] check's result. Each check (`source::epub`,
// `source::toc`, `source::kfx`) keeps its own rich internal report
// and *lowers* it into `Finding`s (see each module's `into_findings`); the
// aggregator [`source::validate`] concatenates them into one [`Report`]. That
// Report is the single type the book editor consumes to build a repair list —
// no caller special-cases each check's bespoke report anymore.
//
// Deliberately serde-free: the library never depends on `serde` (it sits behind
// the `cli` feature, which library consumers switch off with
// `default-features = false`). The CLI serialises by reading these public
// fields — see `main.rs`.

/// How bad a source [`Finding`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A spec violation or integrity break that corrupts conversion or gets
    /// the book rejected by strict readers. Must be fixed.
    Error,
    /// A real defect readers usually tolerate but the editor should surface —
    /// e.g. an undeclared resource, or a chapterless TOC a human should confirm
    /// before rebuilding.
    Warning,
    /// Informational context, not a defect on its own.
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// A machine-actionable repair proposal attached to a [`Finding`]. The book
/// editor reads [`action`](FixHint::action) to drive a repair panel; `detail`
/// is the human-facing description. This is the shape the editor consumes;
/// checks fill in the simple hints they can derive today, and richer payloads
/// (e.g. a proposed nav tree for a deficient TOC) are added as the editor grows
/// a UI for them.
#[derive(Debug, Clone)]
pub struct FixHint {
    /// Machine-readable repair action slug, e.g. `"add-nav-doc"`,
    /// `"rebuild-toc"`, `"make-linear-or-link"`.
    pub action: String,
    /// Human-readable description of the proposed repair.
    pub detail: String,
}

impl FixHint {
    pub fn new(action: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            detail: detail.into(),
        }
    }
}

/// One defect in a source book, in a shape uniform across every source check.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Which source check produced this — `"epub"`, `"toc"`, `"kfx"`.
    pub check: &'static str,
    /// Stable machine-readable rule id, e.g. `"broken-href"`, `"nav-missing"`,
    /// `"toc-deficient"`. Unique within a `check`.
    pub rule: String,
    pub severity: Severity,
    /// Where in the book the defect sits: a spine item / OPF path / container
    /// offset, or a book-level marker like `"<toc>"`.
    pub location: String,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured repair proposal the editor can act on.
    pub fix: Option<FixHint>,
}

/// The unified result of running the source checks over one book: a flat list
/// of [`Finding`]s. Returned by [`source::validate`] and rendered by the book
/// editor as its fix-list.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    /// No error- or warning-level findings. Info-only reports are still clean.
    pub fn is_clean(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|f| matches!(f.severity, Severity::Error | Severity::Warning))
    }

    /// Any error-level finding. This — not [`is_clean`](Self::is_clean) — is the
    /// gate for "would epubcheck reject this?": epubcheck exits 0 on warnings,
    /// so an EPUB with only warning findings is still epubcheck-valid. Every
    /// conversion/repair gate that stands in for "epubcheck-clean" tests this;
    /// warnings surface in the report and UI but never block.
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    /// Findings at exactly `severity`, in report order.
    pub fn by_severity(&self, severity: Severity) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(move |f| f.severity == severity)
    }

    /// How many findings sit at `severity`.
    pub fn count(&self, severity: Severity) -> usize {
        self.by_severity(severity).count()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.findings.is_empty() {
            return write!(f, "source validate: clean (0 findings)");
        }
        writeln!(
            f,
            "source validate: {} finding(s) — {} error, {} warning, {} info",
            self.findings.len(),
            self.count(Severity::Error),
            self.count(Severity::Warning),
            self.count(Severity::Info),
        )?;
        for finding in &self.findings {
            writeln!(
                f,
                "  [{}] {}/{} @ {}: {}",
                finding.severity.as_str(),
                finding.check,
                finding.rule,
                finding.location,
                finding.message,
            )?;
        }
        Ok(())
    }
}

/// Which way the conversion under validation runs. Determines how printed
/// [`fidelity`] reports interpret each side: which is "source / ground truth"
/// vs "target (bokai's output)", and therefore which defects are bokai's fault.
///
/// All fidelity `Report` fields are stored direction-neutrally (`only_in_epub`,
/// `only_in_kfx`, `epub_*`, `kfx_*`). The `Direction` is consulted only at
/// presentation time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    /// EPUB is the source (ground truth); KFX is bokai's output.
    #[default]
    EpubToKfx,
    /// KFX is the source (ground truth); EPUB is bokai's output.
    KfxToEpub,
    /// AZW3 (KF8) is the source (ground truth); EPUB is bokai's output.
    Azw3ToEpub,
}

impl Direction {
    /// Short label for the ground-truth side of this direction.
    pub fn source_label(self) -> &'static str {
        match self {
            Self::EpubToKfx => "EPUB",
            Self::KfxToEpub => "KFX",
            Self::Azw3ToEpub => "AZW3",
        }
    }

    /// Short label for bokai's-output side of this direction.
    pub fn target_label(self) -> &'static str {
        match self {
            Self::EpubToKfx => "KFX",
            Self::KfxToEpub => "EPUB",
            Self::Azw3ToEpub => "EPUB",
        }
    }

    /// Whether the EPUB side is the ground-truth source in this direction.
    pub fn epub_is_source(self) -> bool {
        matches!(self, Self::EpubToKfx)
    }
}
