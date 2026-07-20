//! End-to-end check of the standalone KFX structural validator — job 2 — via
//! the public `source::validate` entry on a real container.
//!
//! The fixture is a well-formed published book, so it must validate with no KFX
//! structural errors. This guards against the checker false-flagging legitimate
//! KFX (the failure mode that makes a validator useless).

use std::path::Path;

#[test]
fn wellformed_kfx_fixture_has_no_structural_defects() {
    let path = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
    assert!(Path::new(path).exists(), "fixture missing: {path}");
    let bytes = std::fs::read(path).expect("read fixture");

    let report = bokai::validate::source::validate(&bytes);

    // A well-formed book has zero error-level defects: its container loads, the
    // required entities are present, every resource resolves and the cover is
    // wired. (TOC audit findings, if any, are warnings and don't count here.)
    assert_eq!(
        report.count(bokai::validate::Severity::Error),
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
    let report = bokai::validate::source::validate(b"CONT not a real container");
    assert_eq!(report.count(bokai::validate::Severity::Error), 1);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.check == "kfx" && f.rule == "container-unreadable"),
        "expected kfx/container-unreadable; got:\n{report}"
    );
}

#[test]
fn kfx_zip_bundle_routes_through_kfx_checks_not_epub() {
    use std::io::Write;

    let kfx_path = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
    let kfx_bytes = std::fs::read(kfx_path).expect("read fixture");

    // Pack the single .kfx into a one-entry `.kfx-zip` (starts with `PK`, like
    // an EPUB) in memory.
    let mut zip_buf: Vec<u8> = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("book.kfx", opts).expect("start_file");
        writer.write_all(&kfx_bytes).expect("write kfx bytes");
        writer.finish().expect("finish zip");
    }

    let report = bokai::validate::source::validate(&zip_buf);

    // Had the bundle been mis-sniffed as an EPUB, `epub::validate` (no mimetype
    // / container.xml) plus the TOC audit would emit errors. Zero errors + no
    // `epub` findings proves it was unwrapped and validated as KFX instead.
    assert_eq!(
        report.count(bokai::validate::Severity::Error),
        0,
        "well-formed kfx-zip bundle should validate clean; got:\n{report}"
    );
    assert!(
        !report.findings.iter().any(|f| f.check == "epub"),
        "a kfx-zip bundle must not be checked as an EPUB; got:\n{report}"
    );
}
