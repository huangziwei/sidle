//! Repairing a spine that contradicts the book's own navigation.
//!
//! The measurement, the proposal and the write are pinned together, because the
//! whole point of the repair is that the three agree: what the audit counts is
//! what the proposal moves, and what the proposal says is what the OPF ends up
//! declaring. The write is a permutation and is held to it.

mod misordered_epub;

use bokai::formats::epub::spine_repair::{
    Misordering, current_spine, declared_spine_misordering, propose_spine, repair_spine, set_spine,
};
use misordered_epub::{DECLARED_IDS, DECLARED_ORDER, SPINE_ORDER};

fn labels(docs: &[bokai::formats::epub::spine_repair::SpineDoc]) -> Vec<String> {
    docs.iter()
        .map(|d| d.label.clone().unwrap_or_else(|| d.href.clone()))
        .collect()
}

#[test]
fn the_contradiction_is_measured_not_guessed() {
    let (_dir, epub) = misordered_epub::build();
    let m = declared_spine_misordering(&epub).expect("measure");

    // Two places where the nav's next entry sits earlier in the spine: Three
    // (spine-last) before Four, and Colophon (spine-middle) after Four. The
    // same two epubcheck reports as NAV-011 on the book this is modelled on.
    assert_eq!(m.descents, 2, "got {m:?}");
    assert_eq!(m.moved, 2, "only the two displaced documents move: {m:?}");
    assert!(m.contradicts());
    assert!(
        m.machine_sorted,
        "this spine is its own manifest sorted lexicographically — the tell \
         that says the spine is the broken side, not the nav"
    );
    assert_eq!(
        m.first_out_of_order.as_deref(),
        Some("Three"),
        "the entry named is the one the spine reads late"
    );
}

#[test]
fn the_audit_reports_it_and_names_the_repair() {
    use bokai::validate::source::toc;

    let (_dir, epub) = misordered_epub::build();
    let audit = toc::validate(&epub).expect("audit");
    assert_eq!(audit.verdict.as_str(), "MISORDERED");
    assert!(!audit.is_clean(), "a self-contradicting book is not clean");

    let findings = audit.into_findings();
    let f = findings
        .iter()
        .find(|f| f.rule == "spine-misordered")
        .unwrap_or_else(|| panic!("no spine-misordered finding in {findings:#?}"));
    assert_eq!(
        f.fix.as_ref().map(|h| h.action.as_str()),
        Some("reorder-spine"),
        "the finding must name the repair that fixes it, not the TOC rebuild"
    );

    // And the repaired book audits clean — the audit and the repair are the
    // same rule read from two ends.
    let fixed = repair_spine(&epub).expect("repair");
    let after = toc::validate(&fixed).expect("re-audit");
    assert_ne!(after.verdict.as_str(), "MISORDERED", "still misordered");
    assert!(
        !after
            .into_findings()
            .iter()
            .any(|f| f.rule == "spine-misordered"),
        "the finding survived its own repair"
    );
}

#[test]
fn the_proposal_is_the_declared_order() {
    let (_dir, epub) = misordered_epub::build();
    assert_eq!(
        labels(&current_spine(&epub).expect("current")),
        SPINE_ORDER,
        "the spine as shipped"
    );
    assert_eq!(
        labels(&propose_spine(&epub).expect("propose")),
        DECLARED_ORDER,
        "the spine as the book's own navigation reads it"
    );
}

#[test]
fn the_write_converges_and_then_refuses() {
    let (_dir, epub) = misordered_epub::build();
    let fixed = repair_spine(&epub).expect("repair");

    assert_eq!(
        labels(&current_spine(&fixed).expect("current")),
        DECLARED_ORDER
    );
    assert_eq!(
        declared_spine_misordering(&fixed).expect("re-measure"),
        Misordering::default(),
        "a repaired book measures clean — the diagnosis and the fix are one rule"
    );
    // Re-writing would re-hash the file and renumber every reading position
    // downstream, for an order the book already has.
    assert!(
        repair_spine(&fixed).is_err(),
        "a second repair must be refused as a no-op"
    );
}

#[test]
fn a_write_is_a_permutation_or_it_is_refused() {
    let (_dir, epub) = misordered_epub::build();
    let drop_one: Vec<String> = DECLARED_IDS[..4].iter().map(|s| s.to_string()).collect();
    let add_one: Vec<String> = DECLARED_IDS
        .iter()
        .map(|s| s.to_string())
        .chain(["nav".to_string()])
        .collect();
    let duplicate: Vec<String> = ["v1-1", "v1-1", "v1-2", "v2", "v2-1"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    for (what, order) in [
        ("a dropped document", drop_one),
        ("an added document", add_one),
        ("a duplicated document", duplicate),
    ] {
        assert!(
            set_spine(&epub, &order).is_err(),
            "{what} is a different edit and must not pass as a reorder"
        );
    }
}

#[test]
fn only_the_spine_changes() {
    let (_dir, epub) = misordered_epub::build();
    let fixed = repair_spine(&epub).expect("repair");

    let before = entries(&epub);
    let after = entries(&fixed);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "a reorder adds and removes no files"
    );
    for (name, bytes) in &before {
        if name.ends_with(".opf") {
            continue;
        }
        assert_eq!(
            bytes,
            after.get(name).expect("same entry set"),
            "{name} was rewritten by a spine reorder"
        );
    }
    // Including the navigation: repairing the spine is what makes the book's own
    // nav correct, so rewriting the nav too would be fixing the wrong side.
    assert_eq!(
        before.get("OEBPS/nav.xhtml"),
        after.get("OEBPS/nav.xhtml"),
        "the nav document is the side that was already right"
    );
}

/// Every zip entry as `name → bytes`, in a form two packages can be compared by.
fn entries(epub: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(epub)).expect("read zip");
    let mut out = std::collections::BTreeMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).expect("entry");
        let name = f.name().to_string();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).expect("read entry");
        out.insert(name, bytes);
    }
    out
}
