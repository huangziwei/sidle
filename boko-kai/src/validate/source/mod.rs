//! Source validation — is one book file well-formed on its own? Each check
//! reads a single input in its native format and never consults a
//! derived/converted copy. These flag defects **in the source book**, so they
//! are what the book editor turns into a repair list.
//!
//! - [`epub`] — EPUB-3 structural conformance (a Rust `epubcheck` replacement):
//!   mimetype, container/OPF wiring, manifest ↔ zip ↔ spine integrity, nav
//!   presence, non-linear reachability, href resolution.
//! - [`toc`] — cross-format declared-TOC audit: is the reader's chapter sidebar
//!   chapterless while the book itself clearly has chapters? Sniffs EPUB vs KFX
//!   and reads only that source.
//! - [`kfx`] — KFX structural conformance (job 2, the `epubcheck` equivalent for
//!   KFX — no such tool exists elsewhere): container integrity, required
//!   entities, reference resolution (section→storyline, content/style refs), nav
//!   reachability, resource-byte and cover resolution, and position-map coverage.

pub mod epub;
pub mod kfx;
pub mod toc;

use super::{Finding, Report, Severity};

/// Run every source check that applies to `bytes` and return one unified
/// [`Report`]. Sniffs the format and runs the matching structural checks plus
/// the cross-format TOC audit, lowering each check's result into [`Finding`]s.
/// This is the single entry the book editor consumes to build its repair list.
///
/// Format sniff: a `PK` zip is an EPUB *unless* it bundles `.kfx` entries — an
/// Amazon `.kfx-zip`, which is merged to a single container and run through the
/// KFX checks (so `validate` on a bundle matches `validate` on the merged
/// `.kfx`). Anything else is treated as a single KFX container.
///
/// Infallible by design: a book so broken a check cannot run (e.g. a KFX that
/// won't load) becomes an `Error` finding rather than an `Err`, so the editor
/// always receives a `Report`.
pub fn validate(bytes: &[u8]) -> Report {
    let mut report = Report::default();

    if bytes.starts_with(b"PK") {
        if zip_bundles_kfx(bytes) {
            // A `.kfx-zip` bundle, not an EPUB: merge its containers into one
            // `.kfx` and run the KFX checks on that.
            match crate::kfx::merge::merge_kfx_zip_bytes(bytes) {
                Ok(merged) => report.findings.extend(validate_kfx(&merged)),
                Err(e) => report.findings.push(Finding {
                    check: "kfx",
                    rule: "container-unreadable".to_string(),
                    severity: Severity::Error,
                    location: "<kfx-zip>".to_string(),
                    message: format!("KFX bundle could not be merged into a container: {e}"),
                    fix: None,
                }),
            }
        } else {
            // EPUB: structural conformance (epubcheck replacement) + TOC audit.
            report
                .findings
                .extend(epub::validate(bytes).into_findings());
            report.findings.extend(toc_findings(bytes));
        }
    } else {
        // A single KFX container (or an unknown blob → container-unreadable).
        report.findings.extend(validate_kfx(bytes));
    }

    report
}

/// The KFX side of [`validate`]: structural checks (rules 1–9) + the cross-
/// format TOC audit (rule 10), over one already-merged container. A container
/// that won't load is already reported by `kfx::validate` as
/// `container-unreadable`, so the TOC audit's own load failure is swallowed
/// here, not double-reported.
///
/// Both `kfx::validate` and the TOC audit load the container once each; a single
/// shared load is a future optimization (the TOC audit's KFX evidence extractor
/// is currently private to `toc`).
fn validate_kfx(bytes: &[u8]) -> Vec<Finding> {
    let mut findings = kfx::validate(bytes);
    let unreadable = findings.iter().any(|f| f.rule == "container-unreadable");
    if !unreadable && let Ok(audit) = toc::validate(bytes) {
        findings.extend(audit.into_findings());
    }
    findings
}

/// Does this `PK` zip bundle `.kfx` containers (an Amazon `.kfx-zip`) rather
/// than being an EPUB? Peeks the entry names — a `.kfx-zip` carries `.kfx`
/// entries and no EPUB `mimetype`, so one `.kfx` entry is a reliable tell.
fn zip_bundles_kfx(zip_bytes: &[u8]) -> bool {
    let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)) else {
        return false;
    };
    (0..archive.len()).any(|i| {
        archive
            .by_index(i)
            .map(|f| f.name().to_ascii_lowercase().ends_with(".kfx"))
            .unwrap_or(false)
    })
}

/// Run the cross-format TOC audit and lower it to findings. A read failure — a
/// malformed EPUB the structural check has already flagged — becomes one
/// `source/unreadable` finding rather than an `Err`, keeping [`validate`]
/// infallible.
fn toc_findings(bytes: &[u8]) -> Vec<Finding> {
    match toc::validate(bytes) {
        Ok(audit) => audit.into_findings(),
        Err(e) => vec![Finding {
            check: "source",
            rule: "unreadable".to_string(),
            severity: Severity::Error,
            location: "<book>".to_string(),
            message: format!("could not read the book for the TOC audit: {e}"),
            fix: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use crate::validate::{Finding, Report, Severity};

    fn finding(severity: Severity, rule: &'static str) -> Finding {
        Finding {
            check: "test",
            rule: rule.to_string(),
            severity,
            location: "<x>".to_string(),
            message: "m".to_string(),
            fix: None,
        }
    }

    #[test]
    fn is_clean_ignores_info_only() {
        let mut report = Report::default();
        assert!(report.is_clean());

        report.findings.push(finding(Severity::Info, "note"));
        assert!(report.is_clean(), "info-only report is still clean");

        report.findings.push(finding(Severity::Warning, "warn"));
        assert!(!report.is_clean());
    }

    #[test]
    fn by_severity_and_count() {
        let report = Report {
            findings: vec![
                finding(Severity::Error, "e1"),
                finding(Severity::Warning, "w1"),
                finding(Severity::Warning, "w2"),
                finding(Severity::Info, "i1"),
            ],
        };
        assert_eq!(report.count(Severity::Error), 1);
        assert_eq!(report.count(Severity::Warning), 2);
        assert_eq!(report.count(Severity::Info), 1);

        let warns: Vec<&str> = report
            .by_severity(Severity::Warning)
            .map(|f| f.rule.as_str())
            .collect();
        assert_eq!(warns, vec!["w1", "w2"]);
    }
}
