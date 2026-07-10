//! End-to-end check of the standalone KFX structural validator — job 2 — via
//! the public `source::validate` entry on a real container.
//!
//! The fixture is a well-formed published book, so it must validate with no KFX
//! structural errors. This guards against the checker false-flagging legitimate
//! KFX (the failure mode that makes a validator useless).

use std::path::Path;

#[test]
fn wellformed_kfx_fixture_has_no_structural_defects() {
    let path = "tests/fixtures/[太宰 治] 人間失格.kfx";
    assert!(Path::new(path).exists(), "fixture missing: {path}");
    let bytes = std::fs::read(path).expect("read fixture");

    let report = boko::validate::source::validate(&bytes);

    // A well-formed book has zero error-level defects: its container loads, the
    // required entities are present, every resource resolves and the cover is
    // wired. (TOC audit findings, if any, are warnings and don't count here.)
    assert_eq!(
        report.count(boko::validate::Severity::Error),
        0,
        "well-formed KFX should have no errors; got:\n{report}"
    );
    // And no KFX structural finding fires at all on a clean book.
    assert!(
        !report.findings.iter().any(|f| f.check == "kfx"),
        "no KFX structural finding expected on a well-formed book; got:\n{report}"
    );
}

#[test]
fn garbage_container_reports_unreadable() {
    // A non-EPUB, non-KFX blob routes to the KFX branch and must surface a
    // single `container-unreadable` error rather than panicking.
    let report = boko::validate::source::validate(b"CONT not a real container");
    assert_eq!(report.count(boko::validate::Severity::Error), 1);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.check == "kfx" && f.rule == "container-unreadable"),
        "expected kfx/container-unreadable; got:\n{report}"
    );
}
