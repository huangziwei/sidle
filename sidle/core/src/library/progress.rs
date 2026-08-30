//! How a phase report becomes a progress bar.

use std::cell::Cell;

/// Map a phase report to a monotonic 0.0–1.0 progress-bar fraction.
/// `pipeline` is a conversion direction (`epub_to_kfx`, …) or an import's
/// `<ext>_import`; a band spans one phase and `cur`/`total` interpolates it.
pub fn fraction(pipeline: &str, phase: &str, cur: usize, total: usize) -> f32 {
    let (lo, hi): (f32, f32) = match (pipeline, phase) {
        ("epub_to_kfx", "survey") => (0.00, 0.08),
        ("epub_to_kfx", "chapters") => (0.08, 0.40),
        ("epub_to_kfx", "images") => (0.40, 0.92),
        ("epub_to_kfx", "finalize") => (0.92, 1.00),

        // Emission order: `load`, `content`, `resources`, `nav`, `finalize`.
        // `resources` transcodes every image and takes the widest band.
        ("kfx_to_epub", "load") => (0.00, 0.08),
        ("kfx_to_epub", "content") => (0.08, 0.24),
        ("kfx_to_epub", "resources") => (0.24, 0.92),
        ("kfx_to_epub", "nav") => (0.92, 0.96),
        ("kfx_to_epub", "finalize") => (0.96, 1.00),

        ("pdf_to_kfx", "probe") => (0.00, 0.05),
        ("pdf_to_kfx", "cover") => (0.05, 0.15),
        ("pdf_to_kfx", "text") => (0.15, 0.55),
        ("pdf_to_kfx", "build") => (0.55, 0.90),
        ("pdf_to_kfx", "geom") => (0.90, 1.00),

        ("kfx_to_pdf", _) => (0.00, 1.00),

        // An `.azw3` import runs two conversions back to back, each leg's
        // phases under its own `epub/` or `kfx/` prefix. The `epub/` legs
        // deflate what the azw3 holds; `kfx/images` re-encodes every image.
        ("azw3_import", "epub/parse") => (0.00, 0.01),
        ("azw3_import", "epub/content") => (0.01, 0.02),
        ("azw3_import", "epub/resources") => (0.02, 0.05),
        ("azw3_import", "epub/nav") => (0.05, 0.06),
        ("azw3_import", "epub/finalize") => (0.06, 0.07),
        ("azw3_import", "kfx/parse") => (0.07, 0.12),
        ("azw3_import", "kfx/survey") => (0.12, 0.16),
        ("azw3_import", "kfx/chapters") => (0.16, 0.26),
        ("azw3_import", "kfx/images") => (0.26, 0.93),
        ("azw3_import", "kfx/finalize") => (0.93, 0.95),

        // A `.mobi` import runs the `epub/` leg alone; the background queue
        // takes its KFX side. `epub/resources` writes the container.
        ("mobi_import", "epub/parse") => (0.00, 0.10),
        ("mobi_import", "epub/content") => (0.10, 0.20),
        ("mobi_import", "epub/resources") => (0.20, 0.90),
        ("mobi_import", "epub/nav") => (0.90, 0.93),
        ("mobi_import", "epub/finalize") => (0.93, 0.95),

        // An Aozora import parses text, renders a cover and builds an EPUB.
        // No phase carries per-item counts; each is one tick.
        ("aozora_import", "epub/parse") => (0.00, 0.35),
        ("aozora_import", "epub/cover") => (0.35, 0.70),
        ("aozora_import", "epub/finalize") => (0.70, 0.95),

        ("kfx_zip_import", "merge") => (0.00, 0.95),

        // Every import ends at `store`: metadata, cover, library slot.
        (_, "store") => (0.95, 1.00),

        // A phase with no band of its own spans the whole bar, leaving
        // `cur`/`total` to read as a plain fraction.
        _ => (0.00, 1.00),
    };
    let within = if total == 0 {
        1.0
    } else {
        (cur as f32 / total as f32).clamp(0.0, 1.0)
    };
    (lo + (hi - lo) * within).clamp(0.0, 1.0)
}

/// The pipeline key for a book being converted at import time, or `None` for
/// the formats stored as they arrive: a `SourceKind` mapping to `None` lands
/// in the library in one step.
pub fn import_pipeline(kind: crate::library::import::SourceKind) -> Option<&'static str> {
    use crate::library::import::SourceKind as K;
    match kind {
        K::Azw3 => Some("azw3_import"),
        K::Mobi => Some("mobi_import"),
        K::AozoraZip => Some("aozora_import"),
        K::KfxZip => Some("kfx_zip_import"),
        K::Epub | K::Kfx | K::Pdf | K::Unknown => None,
    }
}

/// Suppresses progress ticks too small to see: an image phase fires once per
/// image. Movement under a percent is dropped, and `1.0` always passes.
pub struct Throttle(Cell<f32>);

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

impl Throttle {
    pub fn new() -> Self {
        Self(Cell::new(-1.0))
    }

    /// Whether a bar at `fraction` is worth telling anyone about.
    pub fn worth_emitting(&self, fraction: f32) -> bool {
        if fraction >= 1.0 || fraction >= self.0.get() + 0.01 {
            self.0.set(fraction);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phases each pipeline emits, in emission order. `fraction`'s bands
    /// are hand-written against this order; one out of sequence sends a bar
    /// backward mid-job.
    const SEQUENCES: &[(&str, &[&str])] = &[
        ("epub_to_kfx", &["survey", "chapters", "images", "finalize"]),
        (
            "kfx_to_epub",
            &["load", "content", "resources", "nav", "finalize"],
        ),
        ("pdf_to_kfx", &["probe", "cover", "text", "build", "geom"]),
        (
            "azw3_import",
            &[
                "epub/parse",
                "epub/content",
                "epub/resources",
                "epub/nav",
                "epub/finalize",
                "kfx/parse",
                "kfx/survey",
                "kfx/chapters",
                "kfx/images",
                "kfx/finalize",
                "store",
            ],
        ),
        (
            "mobi_import",
            &[
                "epub/parse",
                "epub/content",
                "epub/resources",
                "epub/nav",
                "epub/finalize",
                "store",
            ],
        ),
        (
            "aozora_import",
            &["epub/parse", "epub/cover", "epub/finalize", "store"],
        ),
        ("kfx_zip_import", &["merge", "store"]),
    ];

    #[test]
    fn a_bar_only_ever_moves_forward() {
        for (pipeline, phases) in SEQUENCES {
            let mut previous = 0.0_f32;
            for phase in *phases {
                // Both ends of each band: the start of one phase sits at or
                // past the end of the phase before it.
                for (cur, total) in [(0, 4), (1, 4), (3, 4), (4, 4)] {
                    let f = fraction(pipeline, phase, cur, total);
                    assert!(
                        f >= previous,
                        "{pipeline}/{phase} at {cur}/{total} went backward: {f} after {previous}"
                    );
                    previous = f;
                }
            }
            assert_eq!(
                previous, 1.0,
                "{pipeline} must finish full — it has no later phase to get there"
            );
        }
    }

    #[test]
    fn an_unreported_phase_does_not_jump_the_bar_to_full() {
        // A phase with no band spans the whole bar: a mid-job report reads as
        // a plain fraction.
        assert_eq!(fraction("azw3_import", "kfx/something-new", 1, 4), 0.25);
    }

    #[test]
    fn ticks_too_small_to_see_are_dropped() {
        let throttle = Throttle::new();
        assert!(throttle.worth_emitting(0.0));
        assert!(
            !throttle.worth_emitting(0.005),
            "half a percent is invisible"
        );
        assert!(throttle.worth_emitting(0.02));
        // Movement is measured from the last tick through the gate, not from
        // the last one offered.
        assert!(!throttle.worth_emitting(0.025));
        assert!(!throttle.worth_emitting(0.029));
        assert!(throttle.worth_emitting(0.03));

        // `1.0` passes however small the step into it.
        let throttle = Throttle::new();
        assert!(throttle.worth_emitting(0.995));
        assert!(!throttle.worth_emitting(0.999));
        assert!(throttle.worth_emitting(1.0));
    }
}
