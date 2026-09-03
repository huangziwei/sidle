//! `validate` answers whether one book file is well-formed on its own; each
//! check lowers its result into [`Finding`]s.

pub mod epub;
pub mod kfx;
pub mod toc;

use super::{Finding, FixHint, Report, Severity};

/// Run every source check that applies to `bytes` and return one unified
/// [`Report`]. Sniffs the format and runs the matching structural checks plus
/// the cross-format TOC audit, lowering each check's result into [`Finding`]s.
pub fn validate(bytes: &[u8]) -> Report {
    let mut report = Report::default();

    if bytes.starts_with(b"PK") {
        if zip_bundles_kfx(bytes) {
            // A `.kfx-zip` bundle, not an EPUB: merge its containers into one
            // `.kfx` and run the KFX checks on that.
            match crate::formats::kfx::merge::merge_kfx_zip_bytes(bytes) {
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
            report.findings.extend(style_findings(bytes));
            report.findings.extend(package_findings(bytes));
        }
    } else {
        // A single KFX container (or an unknown blob → container-unreadable).
        report.findings.extend(validate_kfx(bytes));
    }

    report
}

/// The KFX side of [`validate`]: structural checks (rules 1–9) + the cross-
fn validate_kfx(bytes: &[u8]) -> Vec<Finding> {
    let mut findings = kfx::validate(bytes);
    let unreadable = findings.iter().any(|f| f.rule == "container-unreadable");
    if !unreadable && let Ok(audit) = toc::validate(bytes) {
        findings.extend(audit.into_findings());
    }
    findings
}

/// The error-level findings an edit introduced: everything [`validate`]
/// reports on `after` and not on `before`.
pub fn added_errors(before: &[u8], after: &[u8]) -> Vec<Finding> {
    use std::collections::HashMap;
    let key = |f: &Finding| {
        (
            f.check,
            f.rule.clone(),
            f.location.clone(),
            f.message.clone(),
        )
    };
    let mut budget: HashMap<_, usize> = HashMap::new();
    for finding in validate(before)
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
    {
        *budget.entry(key(finding)).or_default() += 1;
    }
    validate(after)
        .findings
        .into_iter()
        .filter(|f| f.severity == Severity::Error)
        .filter(|f| match budget.get_mut(&key(f)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false // already present before the edit
            }
            _ => true,
        })
        .collect()
}

/// True when this `PK` zip is an Amazon `.kfx-zip`: its entry names include
/// a `.kfx` member and no EPUB `mimetype`.
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

/// Run the cross-format TOC audit and lower it to findings. A read failure
/// becomes one `source/unreadable` finding; [`validate`] returns a report
/// either way.
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

fn package_findings(bytes: &[u8]) -> Vec<Finding> {
    let Ok(pkg) = crate::formats::epub::EpubPackage::parse(bytes) else {
        return Vec::new();
    };
    let (Ok(opf_path), Ok(opf_bytes)) = (pkg.opf_path(), pkg.opf_bytes()) else {
        return Vec::new();
    };
    let opf_text =
        crate::util::decode_text(opf_bytes, crate::util::extract_xml_encoding(opf_bytes));
    let Ok(opf) = crate::formats::epub::parse_opf(&opf_text) else {
        return Vec::new();
    };
    if !opf.version.starts_with('2') {
        return Vec::new();
    }
    vec![Finding {
        check: "package",
        rule: "epub2".to_string(),
        severity: Severity::Info,
        location: opf_path,
        message: format!(
            "EPUB {} package: refinement attributes, NCX-only navigation and XHTML 1.1 DOCTYPEs are what EPUB 3 readers and validators reject",
            opf.version
        ),
        fix: Some(FixHint::new(
            "upgrade-epub3",
            "Upgrade the package to EPUB 3: version, refining metadata, a navigation document from the NCX and guide, manifest properties, HTML DOCTYPEs",
        )),
    }]
}

fn style_findings(bytes: &[u8]) -> Vec<Finding> {
    let Ok(Some(flat)) = crate::formats::epub::flattened_styles(bytes) else {
        return Vec::new();
    };
    let producer = flat.producer.as_deref().unwrap_or("a converter");
    vec![Finding {
        check: "style",
        rule: "flattened".to_string(),
        severity: Severity::Warning,
        location: flat.sheets[0].clone(),
        message: format!(
            "stylesheet flattened by {producer}: {} generated classes; the publisher's selectors, page kinds and writing-mode classes are collapsed into one computed-style sheet",
            flat.generated_classes
        ),
        fix: Some(FixHint::new(
            "restore-styles",
            "Restore the stylesheets and class names from a sibling book that kept the publisher's originals",
        )),
    }]
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
