//! Book validation, grouped by the question each check answers and who
//! consumes the answer:
//!
//! - [`source`] — **is one book file well-formed on its own?** Single-input
//!   structural checks (`source::epub` = a Rust epubcheck replacement;
//!   `source::toc` = a cross-format declared-TOC audit; a KFX structural
//!   checker is planned). These flag defects **in the source book** and feed
//!   the book editor's repair list.
//! - [`fidelity`] — **did EPUB ⇄ KFX conversion lose anything?** Pair-input
//!   diffs (`validate(epub_bytes, kfx_bytes)`) comparing semantic preservation
//!   across a conversion. A loss here is a boko converter bug, so these are the
//!   CI checks. Direction-aware (see [`Direction`]).
//! - [`coverage`] — **what does boko not handle yet?** Aggregate reports on
//!   boko's own parser coverage (unmapped HTML tags, dropped CSS properties).
//!   These are roadmap tools, **not** book validators — they never judge a book
//!   right or wrong.
//!
//! The `source` extractors read one format natively and never consult a
//! derived/converted copy. The `fidelity` extractors deliberately parse the
//! EPUB side with independent, minimal tokenization (NOT boko's IR) so a
//! parser-side bug surfaces here instead of being mirrored on both sides; the
//! KFX side reuses boko's own KFX parser (the format is shared with the reader
//! anyway).
//!
//! Calibre's output is NEVER ground truth — sidle exists to replace it. See
//! feedback memory: ground-truth-by-direction.

pub mod coverage;
pub mod fidelity;
pub mod source;

/// Which way the conversion under validation runs. Determines how printed
/// [`fidelity`] reports interpret each side: which is "source / ground truth"
/// vs "target (boko's output)", and therefore which defects are boko's fault.
///
/// All fidelity `Report` fields are stored direction-neutrally (`only_in_epub`,
/// `only_in_kfx`, `epub_*`, `kfx_*`). The `Direction` is consulted only at
/// presentation time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    /// EPUB is the source (ground truth); KFX is boko's output.
    #[default]
    EpubToKfx,
    /// KFX is the source (ground truth); EPUB is boko's output.
    KfxToEpub,
    /// AZW3 (KF8) is the source (ground truth); EPUB is boko's output.
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

    /// Short label for boko's-output side of this direction.
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
