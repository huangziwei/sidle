//! KFX anchor table — the one rule set for turning `$266 anchor` entities and
//! `book_navigation` target positions into HTML ids.
//!
//! Both KFX→EPUB engines (the mechanical `kfx_to_epub` port and the IR route)
//! stamp `id="…"` attributes at anchored `(eid, offset)` positions and emit
//! nav/NCX/guide fragments from the same names, so the table and every
//! registration rule live here and are shared — byte-equal output by
//! construction, not by parallel maintenance.
//!
//! Registration order is part of the contract: real `$266` anchors first (in
//! sorted-name order — the source container gives no meaningful order and a
//! hash-map walk would make the co-located first-anchor choice differ run to
//! run), then synthetic `toc-…` anchors at TOC target positions, then
//! `page-…` at page-list positions. A position that already has an anchor is
//! never re-registered, so real Amazon anchors and TOC-claimed positions keep
//! their first name.

use std::collections::HashMap;

use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;

/// Per-anchor data extracted from `$266 anchor` entities plus synthetic nav
/// anchors. Mirrors calibre's `process_anchors` (yj_to_epub_navigation.py:40).
#[derive(Debug, Default, Clone)]
pub struct AnchorTable {
    /// External-URI anchors: anchor_name → uri. Set when `$186 uri` is
    /// present. These are dereferenced by `<a href>` link_to → uri
    /// resolution.
    pub anchor_uri: HashMap<String, String>,

    /// Internal-position anchors, keyed by `(location_id, offset)`. Each
    /// position can have multiple anchor names (calibre stores a list).
    /// Content emission looks up `(eid, offset)` here and, if found, sets the
    /// HTML element's `id="..."` attribute to a unique id derived from the
    /// first anchor name.
    pub position_anchors: HashMap<i64, HashMap<i64, Vec<String>>>,

    /// Reverse index `anchor_name → (location_id, offset)`, built alongside
    /// `position_anchors`. `resolve_uri` used to scan the whole
    /// `position_anchors` map for every `<a href="anchor:…">` — an
    /// O(hrefs × anchors) quadratic that cost ~300 ms on a link-dense book.
    /// This makes resolution O(1). The OFFSET is retained (not just the eid)
    /// so `resolve_uri` can resolve a link to ANY anchor at a position to the
    /// SAME element id the *first* anchor there stamped (calibre's
    /// `get_anchor_uri` semantics — `id_at` returns the first anchor's id, so a
    /// link naming a non-first co-located anchor must still point at that id or
    /// it dangles). First registration wins, matching the prior "first match".
    pub name_to_position: HashMap<String, (i64, i64)>,

    /// Heading level (1..=6) registered at a `(location_id, offset)` by the
    /// `$798 headings` nav container. bokai's eager equivalent of calibre's
    /// `anchor_heading_level` (name-keyed) + `position_anchors` lookup: since
    /// these synthesized heading anchors are never link targets (only the
    /// element's TAG matters), we key the level directly by position.
    /// Content emission reads this and stamps `-kfx-heading-level` (here: the
    /// element's `layout_hints` level), which `consolidate_html` promotes to
    /// `<hN>` when the element also carries the `"heading"` layout hint.
    pub heading_level: HashMap<(i64, i64), u8>,
}

impl AnchorTable {
    /// Resolve an html id for the given anchor name. The id is unique
    /// across the table; calibre's `make_unique_name` over a set —
    /// here we just sanitize the anchor name with a `anchor-` prefix
    /// fallback for purely numeric names (HTML ids can't start with a
    /// digit in some validators).
    pub fn anchor_id(&self, anchor_name: &str) -> String {
        fix_html_id(anchor_name)
    }

    /// Look up the html id for `(eid, offset)`. Returns the id derived
    /// from the FIRST anchor at that position (matches calibre's
    /// `process_position` behavior).
    pub fn id_at(&self, eid: i64, offset: i64) -> Option<String> {
        let names = self.position_anchors.get(&eid)?.get(&offset)?;
        let name = names.first()?;
        Some(self.anchor_id(name))
    }

    /// Forget every anchor registered on `eid`, so [`Self::id_at`] reports
    /// none there.
    ///
    /// For elements whose chapter is dropped from the package rather than
    /// written: the ids went with the chapter, so a nav or guide entry still
    /// carrying `#…` would point at a fragment nothing defines (epubcheck
    /// RSC-012). Call only after content emission — during emission the table
    /// is what tells the DOM which ids to stamp.
    pub fn forget_element(&mut self, eid: i64) {
        self.position_anchors.remove(&eid);
    }

    /// Heading level (1..=6) registered at `(eid, offset)` by the `$798`
    /// headings nav, or `None`.
    pub fn heading_level_at(&self, eid: i64, offset: i64) -> Option<u8> {
        self.heading_level.get(&(eid, offset)).copied()
    }

    /// The registered offsets > 0 at `eid`, ascending. These are the
    /// positions content emission must locate inside the element's text and
    /// stamp with a zero-length `<span id>` (offset 0 stamps the element
    /// itself).
    pub fn offsets_beyond_zero(&self, eid: i64) -> Vec<i64> {
        let mut offsets: Vec<i64> = self
            .position_anchors
            .get(&eid)
            .map(|m| m.keys().copied().filter(|&o| o > 0).collect())
            .unwrap_or_default();
        offsets.sort_unstable();
        offsets
    }

    /// Resolve an anchor to its final `<a href>` using the AUTHORITATIVE
    /// `html-id → file` map built from the emitted DOM, guaranteeing
    /// referential integrity. Unlike a structural `eid → file` guess (wrong
    /// for content that emits in a different section than the one that
    /// structurally claims its eid — e.g. footnotes — and blind to positions
    /// that never got stamped), this resolves an internal anchor ONLY when its
    /// target id was actually stamped somewhere, and to the file it really
    /// landed in. Returns `None` when the anchor is unresolvable (never
    /// stamped, or a blank external URI); the caller drops the dangling link
    /// so no `<a href="…#missing">` is emitted (epubcheck RSC-012). Matches
    /// calibre's behavior of dropping anchors it can't place.
    pub fn resolve_uri_stamped(
        &self,
        anchor_name: &str,
        stamped_id_to_file: &HashMap<String, String>,
    ) -> Option<String> {
        // External URI (real http link); a blank placeholder is unresolvable.
        if let Some(uri) = self.anchor_uri.get(anchor_name) {
            return if uri.is_empty() {
                None
            } else {
                Some(uri.clone())
            };
        }
        // Internal position anchor — resolve to the file the stamped id is
        // actually in (not the structural guess).
        let &(eid, offset) = self.name_to_position.get(anchor_name)?;
        let frag = self.id_at(eid, offset)?;
        let file = stamped_id_to_file.get(&frag)?;
        Some(format!("{file}#{frag}"))
    }

    /// Register one `$266 anchor` entity's fields under `name`.
    ///
    /// Each anchor has either:
    ///   - `$186 uri` — external link target (kept as-is; calibre normalises
    ///     bare `http://` / `https://` placeholders to empty string)
    ///   - `$183 position` struct with `$155 id` (location_id) and
    ///     `$143 offset` — registers the anchor against `(id, offset)`.
    pub fn register_anchor_fields(&mut self, name: &str, fields: &[(u64, IonValue)]) {
        // External URI anchor.
        if let Some(uri_val) = get_field(fields, KfxSymbol::Uri as u64)
            && let IonValue::String(uri) = uri_val.unwrap_annotated()
        {
            let clean = if uri == "http://" || uri == "https://" {
                String::new()
            } else {
                uri.clone()
            };
            self.anchor_uri.insert(name.to_string(), clean);
            return;
        }
        // Internal position anchor.
        if let Some(pos_val) = get_field(fields, KfxSymbol::Position as u64) {
            let pos = pos_val.unwrap_annotated();
            let Some(pos_fields) = pos.as_struct() else {
                return;
            };
            let Some(eid) = get_field(pos_fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
            else {
                return;
            };
            // `$143 offset` defaults to 0 when missing.
            let offset = get_field(pos_fields, KfxSymbol::Offset as u64)
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            self.position_anchors
                .entry(eid)
                .or_default()
                .entry(offset)
                .or_default()
                .push(name.to_string());
            // Reverse index for O(1) resolve_uri; first registration wins.
            self.name_to_position
                .entry(name.to_string())
                .or_insert((eid, offset));
        }
    }

    /// Register one nav entry's target (if the position is not already
    /// anchored) under the given name `prefix` (`toc` / `page`), then recurse
    /// into its nested entries. The prefix only names the synthetic id;
    /// positions already anchored (a real `$266` anchor or an earlier pass)
    /// are left untouched and shared. Runs on EVERY entry — including ones the
    /// displayed TOC drops (empty label / placeholder) — so the stamped-id set
    /// in content does not depend on display filtering.
    pub fn register_nav_entry(&mut self, entry: &IonValue, prefix: &str) {
        if let Some((eid, offset)) = nav_target_position(entry) {
            let occupied = self
                .position_anchors
                .get(&eid)
                .and_then(|m| m.get(&offset))
                .is_some_and(|names| !names.is_empty());
            if !occupied {
                let name = format!("{prefix}-{eid}-{offset}");
                self.position_anchors
                    .entry(eid)
                    .or_default()
                    .entry(offset)
                    .or_default()
                    .push(name.clone());
                self.name_to_position.entry(name).or_insert((eid, offset));
            }
        }
        if let Some(children) = entry
            .unwrap_annotated()
            .as_struct()
            .and_then(|f| get_field(f, KfxSymbol::Entries as u64))
            .and_then(|v| v.as_list())
        {
            for child in children {
                self.register_nav_entry(child, prefix);
            }
        }
    }

    /// Register heading levels from one `$798 headings` level unit: the
    /// unit's `landmark_type` (`$h2`..`$h6`) sets the level for its nested
    /// entries, and every nested unit's `target_position` `(eid, offset)` is
    /// recorded at that level. Mirrors the subset of calibre's
    /// `process_nav_unit` (`yj_to_epub_navigation.py:199-258`) that derives
    /// `anchor_heading_level`.
    pub fn register_heading_level_unit(&mut self, value: &IonValue, symbols: &SymbolTable) {
        let inner = value.unwrap_annotated();
        let Some(fields) = inner.as_struct() else {
            return;
        };
        let Some(level) = get_field(fields, KfxSymbol::LandmarkType as u64)
            .and_then(|v| symbols.text_of(v))
            .and_then(level_of_landmark)
        else {
            return;
        };
        let Some(nested) = get_field(fields, KfxSymbol::Entries as u64).and_then(|v| v.as_list())
        else {
            return;
        };
        for unit in nested {
            if let Some((eid, offset)) = nav_target_position(unit) {
                self.heading_level.entry((eid, offset)).or_insert(level);
            }
        }
    }
}

/// Walk `book_navigation` values and hand every `nav_container` of
/// `wanted_type` to `f`, in source order. `nav_values` must iterate the
/// `book_navigation` entities deterministically (sort by name when they come
/// from a map); `resolve_container` handles the inline-struct vs referenced
/// `$391` entity forms, which differ between the two engines' data models.
pub fn for_each_nav_container<'a, I, R, F>(
    nav_values: I,
    resolve_container: R,
    symbols: &SymbolTable,
    wanted_type: &str,
    mut f: F,
) where
    I: Iterator<Item = &'a IonValue>,
    R: Fn(&IonValue) -> Option<IonValue>,
    F: FnMut(&[(u64, IonValue)]),
{
    for value in nav_values {
        let unwrapped = value.unwrap_annotated();
        let candidates: Vec<IonValue> = match unwrapped {
            IonValue::List(items) => items.clone(),
            IonValue::Struct(_) => vec![unwrapped.clone()],
            _ => Vec::new(),
        };
        for reading_order in candidates {
            let Some(ro_fields) = reading_order.as_struct() else {
                continue;
            };
            let Some(containers) =
                get_field(ro_fields, KfxSymbol::NavContainers as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for container in containers {
                let Some(resolved) = resolve_container(container) else {
                    continue;
                };
                let Some(cfields) = resolved.as_struct() else {
                    continue;
                };
                let nav_type = get_field(cfields, KfxSymbol::NavType as u64)
                    .and_then(|v| symbols.text_of(v))
                    .unwrap_or("");
                if nav_type != wanted_type {
                    continue;
                }
                f(cfields);
            }
        }
    }
}

/// Register synthetic `{prefix}-{eid}-{offset}` anchors at every entry of the
/// `wanted_type` nav containers (recursively). The shared driver behind the
/// TOC and page-list passes.
pub fn register_nav_synthetics<'a, I, R>(
    table: &mut AnchorTable,
    nav_values: I,
    resolve_container: R,
    symbols: &SymbolTable,
    wanted_type: &str,
    prefix: &str,
) where
    I: Iterator<Item = &'a IonValue>,
    R: Fn(&IonValue) -> Option<IonValue>,
{
    for_each_nav_container(nav_values, resolve_container, symbols, wanted_type, |c| {
        if let Some(entries) = get_field(c, KfxSymbol::Entries as u64).and_then(|v| v.as_list()) {
            for entry in entries {
                table.register_nav_entry(entry, prefix);
            }
        }
    });
}

/// Register `$798 headings` levels from every headings nav container.
pub fn register_heading_levels<'a, I, R>(
    table: &mut AnchorTable,
    nav_values: I,
    resolve_container: R,
    symbols: &SymbolTable,
) where
    I: Iterator<Item = &'a IonValue>,
    R: Fn(&IonValue) -> Option<IonValue>,
{
    for_each_nav_container(nav_values, resolve_container, symbols, "headings", |c| {
        if let Some(units) = get_field(c, KfxSymbol::Entries as u64).and_then(|v| v.as_list()) {
            for unit in units {
                table.register_heading_level_unit(unit, symbols);
            }
        }
    });
}

/// Kindle headings `landmark_type` symbol → heading level. h1 is included for
/// completeness even though Kindle omits it from the headings nav.
fn level_of_landmark(name: &str) -> Option<u8> {
    match name {
        "h1" | "$h1" => Some(1),
        "h2" | "$h2" => Some(2),
        "h3" | "$h3" => Some(3),
        "h4" | "$h4" => Some(4),
        "h5" | "$h5" => Some(5),
        "h6" | "$h6" => Some(6),
        _ => None,
    }
}

/// The `(location_id, offset)` a nav_unit's `$246 target_position` points at.
/// Shared by heading-level and TOC-anchor registration (both key off the same
/// target).
pub fn nav_target_position(unit: &IonValue) -> Option<(i64, i64)> {
    let fields = unit.unwrap_annotated().as_struct()?;
    get_field(fields, KfxSymbol::TargetPosition as u64)
        .and_then(|v| v.as_struct())
        .and_then(|pos| {
            let id = get_field(pos, KfxSymbol::Id as u64)?.as_int()?;
            let offset = get_field(pos, KfxSymbol::Offset as u64)
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            Some((id, offset))
        })
}

/// Sanitise an anchor name into a valid HTML id (alphanumerics + `_` /
/// `-` / `.`; prefix with `anchor-` if it would start with a digit). Matches
/// the safe-name policy `fix_html_id` in calibre's misc helpers.
pub fn fix_html_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let starts_with_alpha = out
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !starts_with_alpha {
        out = format!("anchor-{}", out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position_anchor(eid: i64, offset: i64) -> IonValue {
        IonValue::Struct(vec![(
            KfxSymbol::Position as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(eid)),
                (KfxSymbol::Offset as u64, IonValue::Int(offset)),
            ]),
        )])
    }

    #[test]
    fn register_anchor_fields_uri_and_position() {
        let mut t = AnchorTable::default();
        let uri = IonValue::Struct(vec![(
            KfxSymbol::Uri as u64,
            IonValue::String("https://example.com".to_string()),
        )]);
        t.register_anchor_fields("ext", uri.as_struct().unwrap());
        assert_eq!(t.anchor_uri["ext"], "https://example.com");

        // Bare scheme placeholder normalises to empty.
        let blank = IonValue::Struct(vec![(
            KfxSymbol::Uri as u64,
            IonValue::String("http://".to_string()),
        )]);
        t.register_anchor_fields("blank", blank.as_struct().unwrap());
        assert_eq!(t.anchor_uri["blank"], "");

        let pos = position_anchor(42, 7);
        t.register_anchor_fields("a1", pos.as_struct().unwrap());
        assert_eq!(t.id_at(42, 7).as_deref(), Some("a1"));
        assert_eq!(t.name_to_position["a1"], (42, 7));
    }

    #[test]
    fn first_anchor_at_position_wins() {
        let mut t = AnchorTable::default();
        let pos = position_anchor(5, 0);
        t.register_anchor_fields("first", pos.as_struct().unwrap());
        t.register_anchor_fields("second", pos.as_struct().unwrap());
        assert_eq!(t.id_at(5, 0).as_deref(), Some("first"));
        // Both names resolve to the same position.
        assert_eq!(t.name_to_position["second"], (5, 0));
    }

    #[test]
    fn register_nav_entry_skips_occupied_and_recurses() {
        let mut t = AnchorTable::default();
        let pos = position_anchor(5, 0);
        t.register_anchor_fields("real", pos.as_struct().unwrap());

        // Entry targeting (5,0) — occupied, keeps "real"; nested child at
        // (6,2) gets a synthetic name.
        let child = IonValue::Struct(vec![(
            KfxSymbol::TargetPosition as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Id as u64, IonValue::Int(6)),
                (KfxSymbol::Offset as u64, IonValue::Int(2)),
            ]),
        )]);
        let entry = IonValue::Struct(vec![
            (
                KfxSymbol::TargetPosition as u64,
                IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(5)),
                    (KfxSymbol::Offset as u64, IonValue::Int(0)),
                ]),
            ),
            (KfxSymbol::Entries as u64, IonValue::List(vec![child])),
        ]);
        t.register_nav_entry(&entry, "toc");
        assert_eq!(t.id_at(5, 0).as_deref(), Some("real"));
        assert_eq!(t.id_at(6, 2).as_deref(), Some("toc-6-2"));
    }

    #[test]
    fn offsets_beyond_zero_sorted() {
        let mut t = AnchorTable::default();
        for off in [4, 0, 2] {
            let pos = position_anchor(9, off);
            t.register_anchor_fields(&format!("a{off}"), pos.as_struct().unwrap());
        }
        assert_eq!(t.offsets_beyond_zero(9), vec![2, 4]);
    }

    #[test]
    fn fix_html_id_sanitizes() {
        assert_eq!(fix_html_id("a85J"), "a85J");
        assert_eq!(fix_html_id("page-9-0"), "page-9-0");
        assert_eq!(fix_html_id("123"), "anchor-123");
        // Non-ASCII chars sanitize to `_`, which is a valid id start.
        assert_eq!(fix_html_id("日本"), "__");
        assert_eq!(fix_html_id("_x"), "_x");
    }

    #[test]
    fn resolve_uri_stamped_rules() {
        let mut t = AnchorTable::default();
        let pos = position_anchor(7, 0);
        t.register_anchor_fields("n1", pos.as_struct().unwrap());
        let mut stamped: HashMap<String, String> = HashMap::new();
        // Not stamped → None.
        assert_eq!(t.resolve_uri_stamped("n1", &stamped), None);
        stamped.insert("n1".to_string(), "ch1.xhtml".to_string());
        assert_eq!(
            t.resolve_uri_stamped("n1", &stamped).as_deref(),
            Some("ch1.xhtml#n1")
        );
        // Blank external URI → None.
        t.anchor_uri.insert("blank".to_string(), String::new());
        assert_eq!(t.resolve_uri_stamped("blank", &stamped), None);
    }
}
