//! The epubcheck message catalog — every check W3C epubcheck defines, ported so
//! bokai keys its rules to epubcheck message IDs and can measure coverage
//! against the full spec.
//!
//! Generated from epubcheck 5.3.1's `DefaultSeverities.java` (the authoritative
//! id→severity map). IDs are printed as epubcheck prints them (`RSC-007`).
//! Severity maps FATAL/ERROR → [`Severity::Error`], WARNING → [`Severity::Warning`],
//! USAGE/INFO → [`Severity::Info`]; `None` == epubcheck `SUPPRESSED` (not emitted
//! by default, so bokai doesn't emit it either — kept for a complete catalog).
//!
//! This table is *the spec*, not bokai's coverage. A [`super::Rule`] declares
//! which catalog id it implements via [`super::Rule::message_id`]; the tests in
//! that module assert every mapped id exists here and that bokai never rates a
//! rule *below* epubcheck's severity for the same id.

use crate::validate::Severity;

/// One epubcheck message id and the severity epubcheck assigns it by default.
#[derive(Debug, Clone, Copy)]
pub struct KnownMessage {
    /// Hyphenated id as epubcheck prints it, e.g. `"RSC-007"`.
    pub id: &'static str,
    /// Default severity, or `None` when epubcheck `SUPPRESSED`s the message.
    pub severity: Option<Severity>,
}

/// Every epubcheck message id with its default severity — the port's spec.
pub const CATALOG: &[KnownMessage] = &[
    KnownMessage {
        id: "INF-001",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "ACC-001",
        severity: None,
    },
    KnownMessage {
        id: "ACC-002",
        severity: None,
    },
    KnownMessage {
        id: "ACC-003",
        severity: None,
    },
    KnownMessage {
        id: "ACC-004",
        severity: None,
    },
    KnownMessage {
        id: "ACC-005",
        severity: None,
    },
    KnownMessage {
        id: "ACC-006",
        severity: None,
    },
    KnownMessage {
        id: "ACC-007",
        severity: None,
    },
    KnownMessage {
        id: "ACC-008",
        severity: None,
    },
    KnownMessage {
        id: "ACC-009",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "ACC-010",
        severity: None,
    },
    KnownMessage {
        id: "ACC-011",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "ACC-012",
        severity: None,
    },
    KnownMessage {
        id: "ACC-013",
        severity: None,
    },
    KnownMessage {
        id: "ACC-014",
        severity: None,
    },
    KnownMessage {
        id: "ACC-015",
        severity: None,
    },
    KnownMessage {
        id: "ACC-016",
        severity: None,
    },
    KnownMessage {
        id: "ACC-017",
        severity: None,
    },
    KnownMessage {
        id: "CHK-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-002",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-003",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-004",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-005",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-006",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-007",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CHK-008",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CSS-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CSS-002",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CSS-003",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "CSS-004",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CSS-005",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "CSS-006",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "CSS-007",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "CSS-008",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CSS-009",
        severity: None,
    },
    KnownMessage {
        id: "CSS-010",
        severity: None,
    },
    KnownMessage {
        id: "CSS-011",
        severity: None,
    },
    KnownMessage {
        id: "CSS-012",
        severity: None,
    },
    KnownMessage {
        id: "CSS-013",
        severity: None,
    },
    KnownMessage {
        id: "CSS-015",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "CSS-016",
        severity: None,
    },
    KnownMessage {
        id: "CSS-017",
        severity: None,
    },
    KnownMessage {
        id: "CSS-019",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "CSS-020",
        severity: None,
    },
    KnownMessage {
        id: "CSS-021",
        severity: None,
    },
    KnownMessage {
        id: "CSS-022",
        severity: None,
    },
    KnownMessage {
        id: "CSS-023",
        severity: None,
    },
    KnownMessage {
        id: "CSS-024",
        severity: None,
    },
    KnownMessage {
        id: "CSS-025",
        severity: None,
    },
    KnownMessage {
        id: "CSS-028",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "CSS-029",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "CSS-030",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-002",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "HTM-003",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-004",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-005",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "HTM-006",
        severity: None,
    },
    KnownMessage {
        id: "HTM-007",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "HTM-008",
        severity: None,
    },
    KnownMessage {
        id: "HTM-009",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-010",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "HTM-011",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-012",
        severity: None,
    },
    KnownMessage {
        id: "HTM-013",
        severity: None,
    },
    KnownMessage {
        id: "HTM-014",
        severity: None,
    },
    KnownMessage {
        id: "HTM-015",
        severity: None,
    },
    KnownMessage {
        id: "HTM-016",
        severity: None,
    },
    KnownMessage {
        id: "HTM-017",
        severity: None,
    },
    KnownMessage {
        id: "HTM-018",
        severity: None,
    },
    KnownMessage {
        id: "HTM-019",
        severity: None,
    },
    KnownMessage {
        id: "HTM-020",
        severity: None,
    },
    KnownMessage {
        id: "HTM-021",
        severity: None,
    },
    KnownMessage {
        id: "HTM-022",
        severity: None,
    },
    KnownMessage {
        id: "HTM-023",
        severity: None,
    },
    KnownMessage {
        id: "HTM-024",
        severity: None,
    },
    KnownMessage {
        id: "HTM-025",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "HTM-027",
        severity: None,
    },
    KnownMessage {
        id: "HTM-028",
        severity: None,
    },
    KnownMessage {
        id: "HTM-029",
        severity: None,
    },
    KnownMessage {
        id: "HTM-033",
        severity: None,
    },
    KnownMessage {
        id: "HTM-036",
        severity: None,
    },
    KnownMessage {
        id: "HTM-038",
        severity: None,
    },
    KnownMessage {
        id: "HTM-044",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "HTM-045",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "HTM-046",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-047",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-048",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-049",
        severity: None,
    },
    KnownMessage {
        id: "HTM-050",
        severity: None,
    },
    KnownMessage {
        id: "HTM-051",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "HTM-052",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-053",
        severity: None,
    },
    KnownMessage {
        id: "HTM-054",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-055",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "HTM-056",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-057",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-058",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-059",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "HTM-061",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-001",
        severity: None,
    },
    KnownMessage {
        id: "MED-002",
        severity: None,
    },
    KnownMessage {
        id: "MED-003",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-004",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-005",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-006",
        severity: None,
    },
    KnownMessage {
        id: "MED-007",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-008",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-009",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-010",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-011",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-012",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-013",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-014",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "MED-015",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "MED-016",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "MED-017",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "MED-018",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "NAV-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "NAV-002",
        severity: None,
    },
    KnownMessage {
        id: "NAV-003",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "NAV-004",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "NAV-005",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "NAV-006",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "NAV-007",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "NAV-008",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "NAV-009",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "NAV-010",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "NAV-011",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "NCX-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "NCX-002",
        severity: None,
    },
    KnownMessage {
        id: "NCX-003",
        severity: None,
    },
    KnownMessage {
        id: "NCX-004",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "NCX-005",
        severity: None,
    },
    KnownMessage {
        id: "NCX-006",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-002",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-003",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-004",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-005",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-006",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-007",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-008",
        severity: None,
    },
    KnownMessage {
        id: "OPF-009",
        severity: None,
    },
    KnownMessage {
        id: "OPF-010",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-011",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-012",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-013",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-014",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-015",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-016",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-017",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-018",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-019",
        severity: None,
    },
    KnownMessage {
        id: "OPF-020",
        severity: None,
    },
    KnownMessage {
        id: "OPF-021",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-025",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-026",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-027",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-028",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-029",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-030",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-031",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-032",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-033",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-034",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-035",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-036",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-037",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-038",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-039",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-040",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-041",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-042",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-043",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-044",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-045",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-046",
        severity: None,
    },
    KnownMessage {
        id: "OPF-047",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-048",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-049",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-050",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-051",
        severity: None,
    },
    KnownMessage {
        id: "OPF-052",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-053",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-054",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-055",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-056",
        severity: None,
    },
    KnownMessage {
        id: "OPF-057",
        severity: None,
    },
    KnownMessage {
        id: "OPF-058",
        severity: None,
    },
    KnownMessage {
        id: "OPF-059",
        severity: None,
    },
    KnownMessage {
        id: "OPF-060",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-062",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-063",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-064",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-065",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-066",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-067",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-068",
        severity: None,
    },
    KnownMessage {
        id: "OPF-069",
        severity: None,
    },
    KnownMessage {
        id: "OPF-070",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-071",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-072",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-073",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-074",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-075",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-076",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-077",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-078",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-079",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-080",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-081",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-082",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-083",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-084",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-085",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-086",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "OPF-087",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-088",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-089",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-090",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-091",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-092",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-093",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-094",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-095",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-096",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-097",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "OPF-098",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "OPF-099",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-001",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "PKG-003",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-004",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-005",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-006",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-007",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-008",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-009",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-010",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "PKG-011",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-012",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "PKG-013",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-014",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "PKG-015",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-016",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "PKG-017",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "PKG-018",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-020",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-021",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-022",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "PKG-023",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "PKG-024",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "PKG-025",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-026",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "PKG-027",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-001",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-002",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-003",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-004",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "RSC-005",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-006",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-007",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-008",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-009",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "RSC-010",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-011",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-012",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-013",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-014",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-015",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-016",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-017",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "RSC-018",
        severity: None,
    },
    KnownMessage {
        id: "RSC-019",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "RSC-020",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-021",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-022",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "RSC-023",
        severity: None,
    },
    KnownMessage {
        id: "RSC-024",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "RSC-025",
        severity: Some(Severity::Info),
    },
    KnownMessage {
        id: "RSC-026",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-027",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "RSC-028",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-029",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-030",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-031",
        severity: Some(Severity::Warning),
    },
    KnownMessage {
        id: "RSC-032",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "RSC-033",
        severity: Some(Severity::Error),
    },
    KnownMessage {
        id: "SCP-001",
        severity: None,
    },
    KnownMessage {
        id: "SCP-002",
        severity: None,
    },
    KnownMessage {
        id: "SCP-003",
        severity: None,
    },
    KnownMessage {
        id: "SCP-004",
        severity: None,
    },
    KnownMessage {
        id: "SCP-005",
        severity: None,
    },
    KnownMessage {
        id: "SCP-006",
        severity: None,
    },
    KnownMessage {
        id: "SCP-007",
        severity: None,
    },
    KnownMessage {
        id: "SCP-008",
        severity: None,
    },
    KnownMessage {
        id: "SCP-009",
        severity: None,
    },
    KnownMessage {
        id: "SCP-010",
        severity: None,
    },
];

/// The catalog entry for `id` (e.g. `"RSC-007"`), if epubcheck defines it.
pub fn lookup(id: &str) -> Option<&'static KnownMessage> {
    CATALOG.iter().find(|m| m.id == id)
}

/// How many catalog messages epubcheck emits at Error severity (its ERROR +
/// FATAL). This is the parity-gate denominator: bokai must eventually flag every
/// one of these on the file epubcheck would.
pub fn error_level_count() -> usize {
    CATALOG
        .iter()
        .filter(|m| m.severity == Some(Severity::Error))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_populated_and_ids_unique() {
        assert!(CATALOG.len() >= 290, "catalog too small: {}", CATALOG.len());
        let mut ids: Vec<&str> = CATALOG.iter().map(|m| m.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate ids in CATALOG");
    }

    #[test]
    fn known_ids_resolve_with_expected_severity() {
        assert_eq!(lookup("RSC-007").unwrap().severity, Some(Severity::Error));
        assert_eq!(lookup("HTM-004").unwrap().severity, Some(Severity::Error));
        assert_eq!(lookup("OPF-003").unwrap().severity, Some(Severity::Info)); // USAGE
        assert!(lookup("ACC-001").unwrap().severity.is_none()); // SUPPRESSED
        assert!(lookup("NOPE-999").is_none());
    }

    #[test]
    fn error_level_count_matches_epubcheck_5_3_1() {
        // epubcheck 5.3.1 DefaultSeverities put() lines: 130 ERROR + 8 FATAL.
        assert_eq!(error_level_count(), 138, "error-level count drifted");
    }
}
