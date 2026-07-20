//! Page-progression-direction validation — verify the OPF spine attribute
//! `page-progression-direction` matches the source KFX `document_data`.
//!
//! KFX side (mirrors calibre's `yj_to_epub_metadata.py`):
//!
//! 1. Start from `document_data.direction` (symbol field, defaults to `ltr`).
//! 2. If `document_data.writing_mode` ends in `-rl` (`vertical_rl`), override
//!    to `rtl`. This is calibre's hard rule and the only way a vertical-RTL
//!    Japanese book gets the correct `rtl` flag — most CJK KFX files store
//!    `direction: ltr` explicitly and rely on the writing-mode override.
//!
//! EPUB side: read the `<spine page-progression-direction="...">` attribute
//! from the OPF. When omitted, the EPUB3 default is `default` — which most
//! readers interpret as `ltr`, so for comparison purposes we treat absent ==
//! `ltr`.
//!
//! This validator is independent from `writing_mode.rs` and from `metadata.rs`'s
//! informational PPD print — `metadata.rs` only flags PPD when the EPUB side
//! declares one, which silently passes when bokai forgets to emit the attribute.
//! This module catches the omission.

use std::io::Cursor;

use zip::ZipArchive;

use crate::formats::epub::{parse_container_xml, parse_opf};
use crate::formats::kfx::container::{
    SymbolTable, parse_container_header, parse_container_info, parse_index_table, skip_enty_header,
};
use crate::formats::kfx::ion::{IonParser, IonValue};
use crate::formats::kfx::symbols::KfxSymbol;

/// One of `ltr` / `rtl`. `default` (EPUB3 spec) and any omission are normalised
/// to `Ltr` for comparison — reading systems default to LTR when no PPD is set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Ltr,
    Rtl,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ltr => "ltr",
            Self::Rtl => "rtl",
        }
    }

    fn from_epub_attr(value: Option<&str>) -> Self {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "rtl" => Self::Rtl,
            // `ltr`, `default`, empty/missing — all → Ltr.
            _ => Self::Ltr,
        }
    }

    fn from_kfx_direction(value: &str) -> Self {
        match value.trim_start_matches('$').to_ascii_lowercase().as_str() {
            "rtl" => Self::Rtl,
            _ => Self::Ltr,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub epub_ppd: Direction,
    /// Whether the OPF actually carried the `page-progression-direction`
    /// attribute (independent of the normalised value). When an EPUB omits the
    /// attribute the validator still reports `epub_ppd = Ltr` since that's the
    /// reading-system default — but `epub_attr_present` lets the report flag
    /// "you should have emitted `rtl` but emitted nothing" specifically.
    pub epub_attr_present: bool,

    pub kfx_ppd: Direction,
    /// Raw KFX `document_data.direction` value (e.g. `ltr`, `rtl`, or empty if
    /// no `direction` field was found). Reported for forensics.
    pub kfx_raw_direction: String,
    /// Raw KFX `document_data.writing_mode` value. The `-rl` writing modes
    /// override `kfx_raw_direction` to `rtl`.
    pub kfx_raw_writing_mode: String,
}

impl Report {
    /// Clean when the EPUB matches the KFX. A missing OPF attribute is OK iff
    /// the KFX also resolves to `ltr` (the default).
    pub fn is_clean(&self) -> bool {
        self.epub_ppd == self.kfx_ppd
    }

    pub fn print_summary(&self, dir: super::Direction) {
        println!("Page progression direction:");
        println!(
            "  EPUB: {} ({} OPF attribute)",
            self.epub_ppd.as_str(),
            if self.epub_attr_present { "with" } else { "no" }
        );
        println!(
            "  KFX:  {} (direction={:?}, writing_mode={:?})",
            self.kfx_ppd.as_str(),
            self.kfx_raw_direction,
            self.kfx_raw_writing_mode
        );
        if self.is_clean() {
            println!(
                "  {} preserved on {} side",
                self.kfx_ppd.as_str(),
                dir.target_label()
            );
        } else {
            println!(
                "  MISMATCH: {} ({}) vs {} ({})",
                if dir.epub_is_source() {
                    self.epub_ppd.as_str()
                } else {
                    self.kfx_ppd.as_str()
                },
                dir.source_label(),
                if dir.epub_is_source() {
                    self.kfx_ppd.as_str()
                } else {
                    self.epub_ppd.as_str()
                },
                dir.target_label(),
            );
            if dir.target_label() == "EPUB"
                && self.kfx_ppd == Direction::Rtl
                && !self.epub_attr_present
            {
                println!(
                    "  → EPUB OPF should include `<spine page-progression-direction=\"rtl\">`"
                );
            }
        }
    }

    pub fn print_details(&self, _limit: usize, _dir: super::Direction) {
        // Single value, nothing per-instance.
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let (epub_ppd, epub_attr_present) = extract_epub_ppd(epub_bytes)?;
    let (kfx_ppd, raw_dir, raw_wm) = extract_kfx_ppd(kfx_bytes)?;
    Ok(Report {
        epub_ppd,
        epub_attr_present,
        kfx_ppd,
        kfx_raw_direction: raw_dir,
        kfx_raw_writing_mode: raw_wm,
    })
}

// ============================================================================
// EPUB side
// ============================================================================

fn extract_epub_ppd(epub_bytes: &[u8]) -> Result<(Direction, bool), String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_bytes =
        read_zip_entry(&mut archive, &opf_path).map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let enc = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, enc);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    let raw = opf.metadata.page_progression_direction.clone();
    let present = raw.is_some();
    Ok((Direction::from_epub_attr(raw.as_deref()), present))
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

// ============================================================================
// KFX side
// ============================================================================

/// Returns `(resolved_ppd, raw_direction, raw_writing_mode)`.
fn extract_kfx_ppd(kfx_bytes: &[u8]) -> Result<(Direction, String, String), String> {
    let header = parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    if header.container_info_offset + header.container_info_length > kfx_bytes.len() {
        return Err("container info out of bounds".into());
    }
    let info_data = &kfx_bytes
        [header.container_info_offset..header.container_info_offset + header.container_info_length];
    let info =
        parse_container_info(info_data).map_err(|e| format!("kfx container info: {:?}", e))?;

    // Declared-base symbol table: doc-local ids start at the container's
    // declared import max_id, not at our static table's length (see
    // kfx::container::SymbolTable).
    let symbols = match info.doc_symbols {
        Some((off, len)) if off + len <= kfx_bytes.len() => {
            SymbolTable::from_fragment(Some(&kfx_bytes[off..off + len]))
        }
        _ => SymbolTable::from_fragment(None),
    };

    let resolve_sym = |id: u64| -> String { symbols.resolve(id).to_string() };

    let Some((idx_off, idx_len)) = info.index else {
        return Err("kfx: no index table".into());
    };
    let entities = parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    let doc_data_type = KfxSymbol::DocumentData as u32;
    let metadata_type = KfxSymbol::Metadata as u32;
    let mut raw_direction = String::new();
    let mut raw_writing_mode = String::new();
    let mut explicit_ppd: Option<Direction> = None;

    for ent in &entities {
        if ent.type_id != doc_data_type && ent.type_id != metadata_type {
            continue;
        }
        if ent.offset + ent.length > kfx_bytes.len() {
            continue;
        }
        let entity = &kfx_bytes[ent.offset..ent.offset + ent.length];
        let ion = skip_enty_header(entity);
        let Ok(value) = IonParser::new(ion).parse() else {
            continue;
        };
        if ent.type_id == doc_data_type {
            walk_doc_data(
                &value,
                &resolve_sym,
                &mut raw_direction,
                &mut raw_writing_mode,
            );
        }
        walk_reading_order_ppd(&value, &resolve_sym, &mut explicit_ppd);
    }

    // Calibre rule: start with `direction` (default ltr), then override to rtl
    // when writing-mode ends in `-rl`.
    let mut ppd = if raw_direction.is_empty() {
        Direction::Ltr
    } else {
        Direction::from_kfx_direction(&raw_direction)
    };
    if raw_writing_mode.trim_start_matches('$').ends_with("_rl")
        || raw_writing_mode.trim_start_matches('$').ends_with("-rl")
    {
        ppd = Direction::Rtl;
    }
    // The explicit `reading_orders[*].page_progression_direction` ($425) is the
    // authoritative book-level PPD. Calibre never reads it — it relies solely on
    // the direction + writing-mode heuristic above, which misses rtl books whose
    // *document-level* writing mode is `horizontal_tb` (so the `-rl` override
    // never fires) even though the spine reads right-to-left. Trust it when set.
    if let Some(explicit) = explicit_ppd {
        ppd = explicit;
    }

    Ok((ppd, raw_direction, raw_writing_mode))
}

/// Walk a `document_data` Ion struct and pull out the raw `direction` and
/// `writing_mode` strings. Both are stored as symbols (`$ltr`, `$vertical_rl`)
/// in calibre's `process_content_properties`.
fn walk_doc_data<F>(
    value: &IonValue,
    resolve_sym: &F,
    raw_direction: &mut String,
    raw_writing_mode: &mut String,
) where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    for (k, v) in fields {
        let name = resolve_sym(*k);
        let resolve_v = |v: &IonValue| -> Option<String> {
            match v {
                IonValue::Symbol(s) => Some(resolve_sym(*s)),
                IonValue::String(s) => Some(s.clone()),
                _ => None,
            }
        };
        if name == "direction" && raw_direction.is_empty() {
            if let Some(s) = resolve_v(v) {
                *raw_direction = s;
            }
        } else if name == "writing_mode"
            && raw_writing_mode.is_empty()
            && let Some(s) = resolve_v(v)
        {
            *raw_writing_mode = s;
        }
    }
}

/// Pull the explicit `reading_orders[*].page_progression_direction` ($425) out
/// of a `document_data` ($538) or `metadata` ($258) Ion struct. PPD is a single
/// book-level value, so the first order that declares one wins. Mirrors
/// `validate::fidelity::metadata::extract_ppd`.
fn walk_reading_order_ppd<F>(value: &IonValue, resolve_sym: &F, out: &mut Option<Direction>)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    for (k, v) in fields {
        if resolve_sym(*k) == "reading_orders"
            && let IonValue::List(items) = v
        {
            for r in items {
                let IonValue::Struct(rfields) = r else {
                    continue;
                };
                for (rk, rv) in rfields {
                    if out.is_none()
                        && resolve_sym(*rk) == "page_progression_direction"
                        && let IonValue::Symbol(s) = rv
                    {
                        *out = Some(Direction::from_kfx_direction(&resolve_sym(*s)));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epub_attr_present_marks_explicit_rtl() {
        assert_eq!(Direction::from_epub_attr(Some("rtl")), Direction::Rtl);
        assert_eq!(Direction::from_epub_attr(Some("ltr")), Direction::Ltr);
        assert_eq!(Direction::from_epub_attr(Some("default")), Direction::Ltr);
        assert_eq!(Direction::from_epub_attr(None), Direction::Ltr);
    }

    #[test]
    fn kfx_direction_normalises_dollar_prefix() {
        assert_eq!(Direction::from_kfx_direction("$rtl"), Direction::Rtl);
        assert_eq!(Direction::from_kfx_direction("rtl"), Direction::Rtl);
        assert_eq!(Direction::from_kfx_direction("ltr"), Direction::Ltr);
        assert_eq!(Direction::from_kfx_direction(""), Direction::Ltr);
    }

    /// Mock symbol resolver for the `walk_reading_order_ppd` tests.
    fn resolve(id: u64) -> String {
        match id {
            100 => "reading_orders",
            101 => "page_progression_direction",
            102 => "rtl",
            103 => "ltr",
            104 => "sections",
            _ => "other",
        }
        .to_string()
    }

    /// The case these books hit: document_data says `horizontal_tb`/`ltr`, so
    /// the heuristic resolves `ltr`, but the explicit reading-orders field
    /// declares `rtl` — and that must win.
    #[test]
    fn reading_order_ppd_reads_explicit_rtl() {
        // {reading_orders: [{page_progression_direction: $rtl}]}
        let v = IonValue::Struct(vec![(
            100,
            IonValue::List(vec![IonValue::Struct(vec![(101, IonValue::Symbol(102))])]),
        )]);
        let mut out = None;
        walk_reading_order_ppd(&v, &resolve, &mut out);
        assert_eq!(out, Some(Direction::Rtl));
    }

    /// No explicit field → leave `None` so the caller falls back to the
    /// direction + writing-mode heuristic.
    #[test]
    fn reading_order_ppd_absent_leaves_none() {
        // {reading_orders: [{sections: []}]}
        let v = IonValue::Struct(vec![(
            100,
            IonValue::List(vec![IonValue::Struct(vec![(104, IonValue::List(vec![]))])]),
        )]);
        let mut out = None;
        walk_reading_order_ppd(&v, &resolve, &mut out);
        assert_eq!(out, None);
    }

    /// PPD is one value per book: the first declaring order wins, and an
    /// `Annotated` wrapper (how entities arrive) is transparent.
    #[test]
    fn reading_order_ppd_first_value_wins_through_annotation() {
        let v = IonValue::Annotated(
            vec![258],
            Box::new(IonValue::Struct(vec![(
                100,
                IonValue::List(vec![
                    IonValue::Struct(vec![(101, IonValue::Symbol(102))]), // rtl
                    IonValue::Struct(vec![(101, IonValue::Symbol(103))]), // ltr
                ]),
            )])),
        );
        let mut out = None;
        walk_reading_order_ppd(&v, &resolve, &mut out);
        assert_eq!(out, Some(Direction::Rtl));
    }
}
