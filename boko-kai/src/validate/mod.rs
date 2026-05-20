//! Conversion validation — verify boko-kai's output preserves the semantics
//! of the source artifact. Each submodule covers one feature (ruby, text,
//! images, links, nav, metadata; plus EPUB-only health checks: tags, style)
//! and exposes:
//!
//! - an independent extractor for the EPUB side (using minimal XHTML
//!   tokenization, NOT going through boko's IR — so a parser-side bug
//!   surfaces here rather than being silently mirrored on both sides),
//! - an extractor for the KFX side (using boko's own KFX parser,
//!   since the format is shared with the Kindle reader anyway),
//! - a `validate(epub_bytes, kfx_bytes)` function producing a `Report`.
//!
//! ## Direction
//!
//! The validator works in **both directions** of conversion:
//!
//! - **EPUB → KFX**: source EPUB is ground truth (publisher's, or
//!   calibre-converted from azw3). Compare boko's KFX output against it.
//! - **KFX → EPUB**: source KFX is ground truth (e.g. the one we just
//!   produced from a `.kfx-zip`). Compare boko's EPUB output against it.
//!
//! The extractors and `validate()` signature are direction-agnostic — they
//! consume one EPUB and one KFX regardless. The diff itself is symmetric.
//! Only the [`Direction`] passed to print methods changes how the printed
//! report labels which side is source vs target, and which side's defects
//! reflect a boko bug.
//!
//! Calibre's output is NEVER ground truth — sidle exists to replace it.
//! See feedback memory: ground-truth-by-direction.

pub mod epub3;
pub mod images;
pub mod links;
pub mod metadata;
pub mod nav;
pub mod page_progression;
pub mod ruby;
pub mod style;
pub mod tags;
pub mod text;
pub mod writing_mode;

/// Which way the conversion under validation runs. Determines how printed
/// reports interpret each side: which is "source / ground truth" vs "target
/// (boko's output)", and therefore which defects are boko's fault.
///
/// All `Report` fields are stored direction-neutrally (`only_in_epub`,
/// `only_in_kfx`, `epub_*`, `kfx_*`). The `Direction` is consulted only at
/// presentation time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    /// EPUB is the source (ground truth); KFX is boko's output.
    #[default]
    EpubToKfx,
    /// KFX is the source (ground truth); EPUB is boko's output.
    KfxToEpub,
}

impl Direction {
    /// Short label for the ground-truth side of this direction.
    pub fn source_label(self) -> &'static str {
        match self {
            Self::EpubToKfx => "EPUB",
            Self::KfxToEpub => "KFX",
        }
    }

    /// Short label for boko's-output side of this direction.
    pub fn target_label(self) -> &'static str {
        match self {
            Self::EpubToKfx => "KFX",
            Self::KfxToEpub => "EPUB",
        }
    }

    /// Whether the EPUB side is the ground-truth source in this direction.
    pub fn epub_is_source(self) -> bool {
        matches!(self, Self::EpubToKfx)
    }
}
