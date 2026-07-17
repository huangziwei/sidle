//! KDF/KPF SQLite container read.
//!
//! Reads the `fragments` table (id → Ion-binary blob), applying
//! `local_delta_fragments` overrides on top (a `deleted` row tombstones its id).
//! Mirrors the merge in kfxlib's `kpf_container.deserialize`: deltas win by id,
//! deletions remove. rusqlite needs a file path, so the de-fingerprinted bytes
//! are written to a temp file (same as kfxlib's `temp_filename`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OpenFlags};

use super::NbkError;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Read and delta-merge the `fragments` table into `id -> raw Ion blob`.
pub fn read_fragments(nbk_path: &Path) -> Result<HashMap<String, Vec<u8>>, NbkError> {
    let raw = std::fs::read(nbk_path)?;
    let clean = super::fingerprint::strip_fingerprints(raw);

    let mut tmp = std::env::temp_dir();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    tmp.push(format!("sidle-nbk-{}-{}.sqlite", std::process::id(), seq));
    std::fs::write(&tmp, &clean)?;

    let result = read_from_file(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, rusqlite::Error> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn read_from_file(path: &Path) -> Result<HashMap<String, Vec<u8>>, NbkError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let mut map: HashMap<String, Vec<u8>> = HashMap::new();

    {
        let mut stmt = conn.prepare("SELECT id, payload_value FROM fragments")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
        })?;
        for row in rows {
            let (id, blob) = row?;
            if let Some(b) = blob {
                map.insert(id, b);
            }
        }
    }

    // Delta overrides win by id; a `deleted` delta removes the base fragment.
    if table_exists(&conn, "local_delta_fragments")? {
        let mut stmt =
            conn.prepare("SELECT id, payload_value, deleted FROM local_delta_fragments")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<Vec<u8>>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, blob, deleted) = row?;
            if deleted != 0 {
                map.remove(&id);
            } else if let Some(b) = blob {
                map.insert(id, b);
            }
        }
    }

    Ok(map)
}
