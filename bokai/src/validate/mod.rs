//! Book validation in three groups: [`source`] checks one book file on its own
//! and feeds the editor's repair list, [`fidelity`] diffs an EPUB against a KFX
//! across a conversion ([`Direction`]), [`coverage`] reports unmapped input.

pub mod coverage;
pub mod fidelity;
pub mod source;

use std::fmt;

// One `Finding` shape for every `source` check: each lowers its own report
// through `into_findings`, and `source::validate` concatenates them into one
// `Report`. Serde-free — the CLI serialises by reading these public fields.

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

/// A machine-actionable repair proposal attached to a [`Finding`]:
/// [`action`](FixHint::action) drives a repair panel, `detail` describes it.
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
    /// No error- or warning-level findings. An info-only report is clean.
    pub fn is_clean(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|f| matches!(f.severity, Severity::Error | Severity::Warning))
    }

    /// True for any error-level finding. [`is_clean`](Self::is_clean) counts
    /// warnings too.
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

/// Which way the conversion under validation runs: which side a printed
/// [`fidelity`] report reads as ground truth. Report fields stay
/// direction-neutral (`only_in_epub`, `kfx_*`); presentation consults this.
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
