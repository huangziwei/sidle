//! Keep on-device books in sync with the canonical desktop library.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::api::{self, Book};
use crate::config::ServerConfig;
use crate::device_state;

/// The picker's record of the KFX revision it last wrote for each on-device file,
/// keyed by the frozen filename. Lives under `extensions/sidle/`, not the book dir.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revs(pub HashMap<String, i64>);

impl Revs {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec());
        std::fs::write(path, bytes)
    }
}

/// Record the rev a fresh download landed, so the update pass has a baseline and
/// won't re-pull it. Called from the normal download flow. Best-effort.
pub fn record_download(state_path: &Path, device_filename: &str, kfx_rev: i64) {
    let mut revs = Revs::load(state_path);
    revs.0.insert(device_filename.to_string(), kfx_rev);
    let _ = revs.save(state_path);
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpdateReport {
    /// Files whose bytes we re-pulled in place.
    pub updated: usize,
    /// On-device files considered (matched to a library book).
    pub matched: usize,
    /// Downloads that failed (left the original file untouched).
    pub failed: usize,
}

impl UpdateReport {
    /// A toast line, or `None` when there was nothing to update (the common case
    /// — don't clutter the sync banner with "0 updated").
    pub fn summary(&self) -> Option<String> {
        if self.updated == 0 && self.failed == 0 {
            return None;
        }
        let mut s = format!(
            "{} book{} updated",
            self.updated,
            if self.updated == 1 { "" } else { "s" }
        );
        if self.failed > 0 {
            s.push_str(&format!(", {} failed", self.failed));
        }
        Some(s)
    }
}

/// What the update pass should do with one on-device file.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Server has a newer revision → re-download in place, then record `rev`.
    Update { rev: i64 },
    /// Up to date (or first-seen and not drifted) → record `rev`, no transfer.
    Record { rev: i64 },
    /// No usable server rev → leave the file alone.
    Skip,
}

/// The `<base>` of a `<base>.<sha8>.kfx` name — the library basename, which a
/// reconvert never changes (only the sha8 / bytes do). The stable key that links
/// a drifted on-device file back to its library book.
fn kfx_stem(name: &str) -> &str {
    name.strip_suffix(".kfx")
        .and_then(|s| s.rsplit_once('.'))
        .map_or(name, |(stem, _sha)| stem)
}

/// Resolve an on-device file to a library book: by the sha8 identity first
/// (steady state), then by the basename stem (a book reconverted before we
/// tracked revs — its device sha8 drifted but the basename didn't).
pub fn match_book<'a>(device_file: &str, books: &'a [Book]) -> Option<&'a Book> {
    let sha8 = device_state::extract_sha8(device_file)?;
    if let Some(b) = books
        .iter()
        .find(|b| b.kfx_sha256.as_deref().is_some_and(|s| s.starts_with(sha8)))
    {
        return Some(b);
    }
    let stem = kfx_stem(device_file);
    books
        .iter()
        .find(|b| b.device_filename.as_deref().map(kfx_stem) == Some(stem))
}

/// Decide what to do with one matched file given the rev we last recorded for it.
fn plan_for(device_file: &str, book: &Book, recorded: Option<i64>) -> Plan {
    let rev = book.kfx_rev;
    if rev == 0 {
        // Older server, or a row with no KFX — nothing to compare against.
        return Plan::Skip;
    }
    match recorded {
        Some(r) if r == rev => Plan::Record { rev }, // known & unchanged
        Some(_) => Plan::Update { rev },             // known & the desktop moved it
        None => {
            // First time we've seen this file. If its frozen sha8 still equals
            let drifted = device_state::extract_sha8(device_file)
                .zip(book.kfx_sha256.as_deref())
                .map(|(dev, srv)| !srv.starts_with(dev))
                .unwrap_or(false);
            if drifted {
                Plan::Update { rev }
            } else {
                Plan::Record { rev }
            }
        }
    }
}

/// Re-download, in place, every on-device book the desktop has a newer revision
pub fn pull_updates(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    books: &[Book],
    dir: &Path,
    state_path: &Path,
    on_book: &mut dyn FnMut(usize, usize, &str),
    log: &dyn Fn(&str),
) -> UpdateReport {
    let mut revs = Revs::load(state_path);
    let mut report = UpdateReport::default();

    // First pass: classify. Records (no transfer) apply immediately; updates are
    // collected so the toast can count "N of M".
    let mut to_update: Vec<(String, &Book, i64)> = Vec::new();
    for file in device_state::scan_downloaded_files(dir) {
        let Some(book) = match_book(&file, books) else {
            continue;
        };
        report.matched += 1;
        match plan_for(&file, book, revs.0.get(&file).copied()) {
            Plan::Record { rev } => {
                revs.0.insert(file, rev);
            }
            Plan::Update { rev } => to_update.push((file, book, rev)),
            Plan::Skip => {}
        }
    }

    let total = to_update.len();
    for (i, (file, book, rev)) in to_update.into_iter().enumerate() {
        on_book(i + 1, total, &book.title);
        let target = dir.join(&file);
        match api::download_book(agent, cfg, book).and_then(|dl| api::stream_download(dl, &target))
        {
            Ok(bytes) => {
                log(&format!("updated {file} ({bytes} bytes, rev {rev})"));
                revs.0.insert(file, rev);
                report.updated += 1;
            }
            Err(e) => {
                log(&format!("update {file} failed: {e:#}"));
                report.failed += 1;
            }
        }
    }

    if let Err(e) = revs.save(state_path) {
        log(&format!("save synced revs failed: {e}"));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(sha: &str, dev: &str, rev: i64) -> Book {
        Book {
            id: 1,
            title: "T".into(),
            kfx_sha256: Some(sha.into()),
            device_filename: Some(dev.into()),
            kfx_rev: rev,
            ..Default::default()
        }
    }

    #[test]
    fn kfx_stem_strips_sha_and_ext() {
        assert_eq!(
            kfx_stem("[Homer] The Iliad (2023).faf30ffb.kfx"),
            "[Homer] The Iliad (2023)"
        );
        assert_eq!(kfx_stem("foo.bar.12345678.kfx"), "foo.bar");
        assert_eq!(kfx_stem("no-ext"), "no-ext");
    }

    #[test]
    fn match_prefers_sha_then_stem() {
        let books = vec![
            book("632717c800aa", "[Homer] The Iliad (2023).632717c8.kfx", 100),
            book(
                "8e247f2500bb",
                "[Raymond Chandler] The Big Sleep (2002).8e247f25.kfx",
                100,
            ),
        ];
        // Exact identity.
        let hit = match_book("[Homer] The Iliad (2023).632717c8.kfx", &books).unwrap();
        assert_eq!(hit.kfx_sha256.as_deref(), Some("632717c800aa"));
        // Drifted device sha8 (faf30ffb) — resolves by stem to the Iliad.
        let hit = match_book("[Homer] The Iliad (2023).faf30ffb.kfx", &books).unwrap();
        assert_eq!(hit.kfx_sha256.as_deref(), Some("632717c800aa"));
        // A stem we don't have.
        assert!(match_book("[Nobody] Unknown (1999).deadbeef.kfx", &books).is_none());
    }

    #[test]
    fn plan_records_baseline_then_detects_change() {
        let b = book("632717c800aa", "[Homer] The Iliad (2023).632717c8.kfx", 100);
        let file = "[Homer] The Iliad (2023).632717c8.kfx";
        // First sight, identity matches → just record, don't pull.
        assert_eq!(plan_for(file, &b, None), Plan::Record { rev: 100 });
        // Recorded and unchanged → nothing.
        assert_eq!(plan_for(file, &b, Some(100)), Plan::Record { rev: 100 });
        // Desktop reconverted (rev moved) → pull.
        assert_eq!(plan_for(file, &b, Some(80)), Plan::Update { rev: 100 });
    }

    #[test]
    fn plan_pulls_drifted_book_on_first_sight() {
        // Device file frozen at the old sha8; the library identity has drifted.
        let b = book("632717c800aa", "[Homer] The Iliad (2023).632717c8.kfx", 100);
        let drifted_file = "[Homer] The Iliad (2023).faf30ffb.kfx";
        assert_eq!(plan_for(drifted_file, &b, None), Plan::Update { rev: 100 });
        // But once recorded at the current rev, it settles (no re-pull loop).
        assert_eq!(
            plan_for(drifted_file, &b, Some(100)),
            Plan::Record { rev: 100 }
        );
    }

    #[test]
    fn plan_skips_when_server_has_no_rev() {
        let b = book("632717c800aa", "x.632717c8.kfx", 0);
        assert_eq!(plan_for("x.632717c8.kfx", &b, None), Plan::Skip);
    }

    #[test]
    fn revs_round_trip_as_plain_map() {
        let mut r = Revs::default();
        r.0.insert("a.deadbeef.kfx".into(), 42);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(
            json, r#"{"a.deadbeef.kfx":42}"#,
            "transparent → plain object"
        );
        let back: Revs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.0.get("a.deadbeef.kfx"), Some(&42));
    }
}
