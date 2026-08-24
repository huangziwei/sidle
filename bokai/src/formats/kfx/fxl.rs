//! Fixed-layout signal derivation shared across the KFX family.
//!
//! [`book_signals`] unions the two places a book states the image-based
//! fixed-layout capabilities (§12.1): the `content_features` ($585) keys
//! `yj_*fixed_layout` / `yj_double_page_spread`, and the
//! `kindle_capability_metadata` category of `book_metadata` ($490), which
//! states the same two as values. [`page_leaves`] walks a page template to a
//! section's pages, and [`read_px`] reads the `fixed_width`/`fixed_height`
//! each states.

use std::collections::HashMap;

use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::BookData;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::model::{PageSpread, PanelRect};

/// Book-level fixed-layout signals. `fixed_layout` splits each section's page
/// template into per-page spine documents; `double_page_spread` marks a spread
/// comic, driving `book-type: comic` and the `page-spread-left`/`-right` pairing.
#[derive(Debug, Clone, Copy, Default)]
pub struct FxlFeatures {
    pub fixed_layout: bool,
    pub double_page_spread: bool,
}

/// Scan one `content_features` ($585) entity, setting in `acc` the flags its
/// keys declare.
pub fn scan_content_features(entity: &IonValue, symbols: &SymbolTable, acc: &mut FxlFeatures) {
    let inner = entity.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return;
    };
    let Some(features) = get_field(fields, KfxSymbol::Features as u64).and_then(|x| x.as_list())
    else {
        return;
    };
    for feat in features {
        let Some(ff) = feat.unwrap_annotated().as_struct() else {
            continue;
        };
        // `key` is an IonString in Amazon KFX; a symbol resolves the same.
        let key = match get_field(ff, KfxSymbol::Key as u64) {
            Some(v) => match v.unwrap_annotated() {
                IonValue::String(s) => s.clone(),
                other => symbols
                    .text_of(other)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            },
            None => continue,
        };
        if key.contains("fixed_layout") {
            acc.fixed_layout = true;
        }
        if key == "yj_double_page_spread" {
            acc.double_page_spread = true;
        }
    }
}

/// Scan one `book_metadata` ($490) entity, setting in `acc` the flags its
/// `kindle_capability_metadata` category enables. The category states
/// `yj_fixed_layout` and `yj_double_page_spread` as values.
pub fn scan_capability_metadata(entity: &IonValue, symbols: &SymbolTable, acc: &mut FxlFeatures) {
    let inner = entity.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return;
    };
    let Some(categories) = get_field(fields, KfxSymbol::CategorisedMetadata as u64)
        .and_then(|v| v.unwrap_annotated().as_list())
    else {
        return;
    };
    for category in categories {
        let Some(cf) = category.unwrap_annotated().as_struct() else {
            continue;
        };
        if get_field(cf, KfxSymbol::Category as u64).and_then(|v| symbols.text_of(v))
            != Some(CAPABILITY_CATEGORY)
        {
            continue;
        }
        let Some(entries) =
            get_field(cf, KfxSymbol::Metadata as u64).and_then(|v| v.unwrap_annotated().as_list())
        else {
            continue;
        };
        for entry in entries {
            let Some(ef) = entry.unwrap_annotated().as_struct() else {
                continue;
            };
            let Some(key) = get_field(ef, KfxSymbol::Key as u64).and_then(|v| symbols.text_of(v))
            else {
                continue;
            };
            let enabled = get_field(ef, KfxSymbol::Value as u64)
                .is_some_and(|v| capability_enabled(v, symbols));
            if !enabled {
                continue;
            }
            if key.ends_with("fixed_layout") {
                acc.fixed_layout = true;
            }
            if key == "yj_double_page_spread" {
                acc.double_page_spread = true;
            }
        }
    }
}

/// The `book_metadata` category stating which capabilities a book turns on.
const CAPABILITY_CATEGORY: &str = "kindle_capability_metadata";

/// Whether a [`CAPABILITY_CATEGORY`] value turns its capability on. The
/// category carries a key it disables, valued 0 or false.
fn capability_enabled(value: &IonValue, symbols: &SymbolTable) -> bool {
    let inner = value.unwrap_annotated();
    match inner {
        IonValue::Bool(b) => *b,
        IonValue::Int(n) => *n != 0,
        _ => matches!(symbols.text_of(inner), Some("true" | "enabled")),
    }
}

/// The book's fixed-layout signals, unioned across both declaration sites.
pub fn book_signals(book: &BookData) -> FxlFeatures {
    let mut acc = FxlFeatures::default();
    if let Some(entities) = book.by_type.get(&(KfxSymbol::ContentFeatures as u64)) {
        for entity in entities.values() {
            scan_content_features(entity, &book.symbols, &mut acc);
        }
    }
    if let Some(entities) = book.by_type.get(&(KfxSymbol::BookMetadata as u64)) {
        for entity in entities.values() {
            scan_capability_metadata(entity, &book.symbols, &mut acc);
        }
    }
    acc
}

/// Read a pixel dimension field that may be a bare int (`fixed_width: 900`) or
/// a `{value, unit}` length struct. Returns `None` when absent or non-positive.
pub fn read_px(fields: &[(u64, IonValue)], sym: KfxSymbol) -> Option<u32> {
    let v = get_field(fields, sym as u64)?;
    let inner = v.unwrap_annotated();
    if let Some(n) = inner.as_int() {
        return if n > 0 { Some(n as u32) } else { None };
    }
    if let Some(fs) = inner.as_struct()
        && let Some(n) = get_field(fs, KfxSymbol::Value as u64).and_then(|x| x.as_int())
    {
        return if n > 0 { Some(n as u32) } else { None };
    }
    None
}

/// Read a `{value, unit: percent}` length as a fraction of the page box.
/// `Fixed` positioning states all four sides this way (§12.5).
fn read_percent(fields: &[(u64, IonValue)], sym: KfxSymbol) -> Option<f32> {
    let inner = get_field(fields, sym as u64)?.unwrap_annotated();
    let struct_fields = inner.as_struct()?;
    let value = get_field(struct_fields, KfxSymbol::Value as u64)?.unwrap_annotated();
    let number = match value {
        IonValue::Decimal(text) => text.parse::<f32>().ok()?,
        other => other.as_int()? as f32,
    };
    Some(number / 100.0)
}

/// The `left`/`top`/`width`/`height` of a positioned element, as page-box
/// fractions. `None` unless all four are stated.
fn read_rect(fields: &[(u64, IonValue)]) -> Option<PanelRect> {
    Some(PanelRect {
        left: read_percent(fields, KfxSymbol::Left)?,
        top: read_percent(fields, KfxSymbol::Top)?,
        width: read_percent(fields, KfxSymbol::Width)?,
        height: read_percent(fields, KfxSymbol::Height)?,
    })
}

/// The element id an `activate` ($442) entry zooms to: the `target` of its
/// `zoom_in` action.
fn zoom_target_id(fields: &[(u64, IonValue)]) -> Option<i64> {
    let entries = get_field(fields, KfxSymbol::Activate as u64)?
        .unwrap_annotated()
        .as_list()?;
    for entry in entries {
        let Some(entry_fields) = entry.unwrap_annotated().as_struct() else {
            continue;
        };
        let action = get_field(entry_fields, KfxSymbol::Action as u64)
            .and_then(|v| v.unwrap_annotated().as_symbol());
        if action == Some(KfxSymbol::ZoomIn as u64) {
            return get_field(entry_fields, KfxSymbol::Target as u64).and_then(|v| v.as_int());
        }
    }
    None
}

/// The author-drawn comic panels a page's `content_list` states, in `ordinal`
/// order (§12.6): a region carrying `ordinal` and an `activate` naming a
/// `zoom_target`, whose window container holds the magnified page image.
pub fn page_panels(items: &[IonValue]) -> Vec<crate::model::Panel> {
    let mut targets: HashMap<i64, &[(u64, IonValue)]> = HashMap::new();
    for item in items {
        let Some(fields) = item.unwrap_annotated().as_struct() else {
            continue;
        };
        let is_target = get_field(fields, KfxSymbol::Type as u64)
            .and_then(|v| v.unwrap_annotated().as_symbol())
            == Some(KfxSymbol::ZoomTarget as u64);
        if is_target
            && let Some(id) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
        {
            targets.insert(id, fields);
        }
    }

    let mut panels = Vec::new();
    for item in items {
        let Some(fields) = item.unwrap_annotated().as_struct() else {
            continue;
        };
        let (Some(ordinal), Some(target_id)) = (
            get_field(fields, KfxSymbol::Ordinal as u64).and_then(|v| v.as_int()),
            zoom_target_id(fields),
        ) else {
            continue;
        };
        let (Some(source), Some(target)) = (read_rect(fields), targets.get(&target_id)) else {
            continue;
        };
        // The target's one child is the window; the window's one child is the
        // magnified image.
        let Some(window_fields) = first_child(target) else {
            continue;
        };
        let (Some(window), Some(image_fields)) =
            (read_rect(window_fields), first_child(window_fields))
        else {
            continue;
        };
        let Some(image) = read_rect(image_fields) else {
            continue;
        };
        panels.push(crate::model::Panel {
            ordinal: ordinal.max(0) as u32,
            source,
            window,
            image,
        });
    }
    panels.sort_by_key(|p| p.ordinal);
    panels
}

/// Whether a page's content-list item is one half of a panel: the
/// `zoom_target` holding the magnified view, or the region whose `activate`
/// names it. [`page_panels`] reads both, so a content walk skips them.
pub fn is_panel_element(item: &IonValue) -> bool {
    let Some(fields) = item.unwrap_annotated().as_struct() else {
        return false;
    };
    let is_target = get_field(fields, KfxSymbol::Type as u64)
        .and_then(|v| v.unwrap_annotated().as_symbol())
        == Some(KfxSymbol::ZoomTarget as u64);
    is_target || zoom_target_id(fields).is_some()
}

/// The fields of the first struct in `fields`' `content_list`.
fn first_child(fields: &[(u64, IonValue)]) -> Option<&[(u64, IonValue)]> {
    get_field(fields, KfxSymbol::ContentList as u64)?
        .unwrap_annotated()
        .as_list()?
        .first()?
        .unwrap_annotated()
        .as_struct()
}

/// Whether a page-template `layout` value names a facing-page spread
/// container, whose story holds the per-page containers.
pub fn is_spread_layout(layout: &str) -> bool {
    layout == "page_spread" || layout == "facing_page"
}

/// What a page-template walk resolves through the container it came from: a
/// `structure` ($608) symbol a template stands in for, and the `content_list`
/// items of a storyline named by `story_name` ($176).
pub trait PageContext {
    fn structure(&self, name: &str) -> Option<IonValue>;
    fn storyline_pages(&self, story: &str) -> Vec<IonValue>;
}

/// A page template's leaf pages in reading order, each with the spread half it
/// occupies (`None` for a page alone). A `page_spread` storyline's first
/// container is an `ltr` book's left page (§12.3).
pub fn page_leaves(
    template: &IonValue,
    symbols: &SymbolTable,
    ppd: &str,
    ctx: &impl PageContext,
) -> Vec<(IonValue, Option<PageSpread>)> {
    let mut out = Vec::new();
    walk_pages(template, symbols, ppd, ctx, None, &mut out);
    out
}

fn walk_pages(
    template: &IonValue,
    symbols: &SymbolTable,
    ppd: &str,
    ctx: &impl PageContext,
    side: Option<PageSpread>,
    out: &mut Vec<(IonValue, Option<PageSpread>)>,
) {
    let resolved;
    let template = match template.unwrap_annotated() {
        IonValue::Symbol(id) => {
            let Some(elem) = ctx.structure(symbols.resolve(*id)) else {
                return;
            };
            resolved = elem;
            &resolved
        }
        _ => template,
    };
    let inner = template.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return;
    };
    let layout = get_field(fields, KfxSymbol::Layout as u64)
        .and_then(|v| symbols.text_of(v))
        .unwrap_or("");
    if !is_spread_layout(layout) {
        out.push((template.clone(), side));
        return;
    }
    let Some(story) =
        get_field(fields, KfxSymbol::StoryName as u64).and_then(|v| symbols.text_of(v))
    else {
        return;
    };
    let mut next = if ppd == "ltr" {
        PageSpread::Left
    } else {
        PageSpread::Right
    };
    for page in ctx.storyline_pages(story) {
        walk_pages(&page, symbols, ppd, ctx, Some(next), out);
        next = match next {
            PageSpread::Left => PageSpread::Right,
            _ => PageSpread::Left,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features_entity(keys: &[&str]) -> IonValue {
        let feats = keys
            .iter()
            .map(|k| {
                IonValue::Struct(vec![(
                    KfxSymbol::Key as u64,
                    IonValue::String(k.to_string()),
                )])
            })
            .collect();
        IonValue::Struct(vec![(KfxSymbol::Features as u64, IonValue::List(feats))])
    }

    #[test]
    fn detects_fixed_layout_and_spread_keys() {
        let symbols = SymbolTable::from_fragment(None);
        let mut acc = FxlFeatures::default();
        scan_content_features(
            &features_entity(&["yj_non_pdf_fixed_layout"]),
            &symbols,
            &mut acc,
        );
        assert!(acc.fixed_layout);
        assert!(!acc.double_page_spread);
        // A second scan sets the flags its own keys declare.
        scan_content_features(
            &features_entity(&["yj_double_page_spread", "yj_fixed_layout"]),
            &symbols,
            &mut acc,
        );
        assert!(acc.fixed_layout && acc.double_page_spread);
    }

    fn capability_entity(entries: &[(&str, IonValue)]) -> IonValue {
        let metadata = entries
            .iter()
            .map(|(key, value)| {
                IonValue::Struct(vec![
                    (KfxSymbol::Key as u64, IonValue::String(key.to_string())),
                    (KfxSymbol::Value as u64, value.clone()),
                ])
            })
            .collect();
        IonValue::Struct(vec![(
            KfxSymbol::CategorisedMetadata as u64,
            IonValue::List(vec![IonValue::Struct(vec![
                (
                    KfxSymbol::Category as u64,
                    IonValue::String(CAPABILITY_CATEGORY.to_string()),
                ),
                (KfxSymbol::Metadata as u64, IonValue::List(metadata)),
            ])]),
        )])
    }

    /// §12.1. The same two capabilities are declared in the metadata, where a
    /// capability the book does not want is stated as a disabled value.
    #[test]
    fn capability_metadata_declares_the_same_signals() {
        let symbols = SymbolTable::from_fragment(None);
        let mut acc = FxlFeatures::default();
        scan_capability_metadata(
            &capability_entity(&[
                ("yj_fixed_layout", IonValue::Int(1)),
                ("yj_double_page_spread", IonValue::Int(1)),
                ("continuous_popup_progression", IonValue::Int(0)),
            ]),
            &symbols,
            &mut acc,
        );
        assert!(acc.fixed_layout && acc.double_page_spread);

        let mut off = FxlFeatures::default();
        scan_capability_metadata(
            &capability_entity(&[("yj_fixed_layout", IonValue::Int(0))]),
            &symbols,
            &mut off,
        );
        assert!(!off.fixed_layout);

        // Another category's keys are not capabilities.
        let mut elsewhere = FxlFeatures::default();
        scan_capability_metadata(
            &IonValue::Struct(vec![(
                KfxSymbol::CategorisedMetadata as u64,
                IonValue::List(vec![IonValue::Struct(vec![
                    (
                        KfxSymbol::Category as u64,
                        IonValue::String("kindle_title_metadata".to_string()),
                    ),
                    (
                        KfxSymbol::Metadata as u64,
                        IonValue::List(vec![IonValue::Struct(vec![
                            (
                                KfxSymbol::Key as u64,
                                IonValue::String("yj_fixed_layout".to_string()),
                            ),
                            (KfxSymbol::Value as u64, IonValue::Int(1)),
                        ])]),
                    ),
                ])]),
            )]),
            &symbols,
            &mut elsewhere,
        );
        assert!(!elsewhere.fixed_layout);
    }

    /// One storyline of per-page containers, for a spread template to name.
    struct OnePage(Vec<IonValue>);

    impl PageContext for OnePage {
        fn structure(&self, _name: &str) -> Option<IonValue> {
            None
        }

        fn storyline_pages(&self, _story: &str) -> Vec<IonValue> {
            self.0.clone()
        }
    }

    /// §12.3. A spread container yields the pages of the storyline it names,
    /// facing alternate ways; anything else is a page in its own right.
    #[test]
    fn a_spread_template_yields_its_storylines_pages() {
        let symbols = SymbolTable::from_fragment(None);
        let leaf =
            |w: i64| IonValue::Struct(vec![(KfxSymbol::FixedWidth as u64, IonValue::Int(w))]);
        let spread = IonValue::Struct(vec![
            (
                KfxSymbol::Layout as u64,
                IonValue::Symbol(KfxSymbol::PageSpread as u64),
            ),
            (
                KfxSymbol::StoryName as u64,
                IonValue::String("story1".to_string()),
            ),
        ]);
        let ctx = OnePage(vec![leaf(1), leaf(2)]);

        let ltr = page_leaves(&spread, &symbols, "ltr", &ctx);
        assert_eq!(
            ltr.iter().map(|(_, side)| *side).collect::<Vec<_>>(),
            vec![Some(PageSpread::Left), Some(PageSpread::Right)]
        );
        let rtl = page_leaves(&spread, &symbols, "rtl", &ctx);
        assert_eq!(
            rtl.iter().map(|(_, side)| *side).collect::<Vec<_>>(),
            vec![Some(PageSpread::Right), Some(PageSpread::Left)]
        );

        // A template that is not a spread is the page itself, facing neither
        // way.
        let alone = page_leaves(&leaf(3), &symbols, "ltr", &ctx);
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].1, None);
    }

    /// A `{value, unit: percent}` length, the form a positioned side takes.
    fn pct(fraction: f32) -> IonValue {
        IonValue::Struct(vec![
            (
                KfxSymbol::Value as u64,
                IonValue::Decimal(format!("{:.4}", fraction * 100.0)),
            ),
            (
                KfxSymbol::Unit as u64,
                IonValue::Symbol(KfxSymbol::Percent as u64),
            ),
        ])
    }

    /// The four sides of `rect`, as an element's fields.
    fn sides(rect: PanelRect) -> Vec<(u64, IonValue)> {
        vec![
            (KfxSymbol::Left as u64, pct(rect.left)),
            (KfxSymbol::Top as u64, pct(rect.top)),
            (KfxSymbol::Width as u64, pct(rect.width)),
            (KfxSymbol::Height as u64, pct(rect.height)),
        ]
    }

    /// The region/`zoom_target` pair one panel adds to a page's content list.
    fn panel_pair(panel: crate::model::Panel, target_id: i64) -> Vec<IonValue> {
        let mut region = sides(panel.source);
        region.extend([
            (
                KfxSymbol::Ordinal as u64,
                IonValue::Int(panel.ordinal as i64),
            ),
            (
                KfxSymbol::Activate as u64,
                IonValue::List(vec![IonValue::Struct(vec![
                    (
                        KfxSymbol::Action as u64,
                        IonValue::Symbol(KfxSymbol::ZoomIn as u64),
                    ),
                    (KfxSymbol::Target as u64, IonValue::Int(target_id)),
                ])]),
            ),
        ]);

        let mut image = sides(panel.image);
        image.push((
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Image as u64),
        ));
        let mut window = sides(panel.window);
        window.push((
            KfxSymbol::ContentList as u64,
            IonValue::List(vec![IonValue::Struct(image)]),
        ));
        let target = IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(target_id)),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::ZoomTarget as u64),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List(vec![IonValue::Struct(window)]),
            ),
        ]);
        vec![IonValue::Struct(region), target]
    }

    fn rect(left: f32, top: f32, width: f32, height: f32) -> PanelRect {
        PanelRect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn a_panel_pair_reads_back_to_its_four_rectangles() {
        let panel = crate::model::Panel {
            ordinal: 1,
            source: rect(0.1, 0.2, 0.3, 0.4),
            window: rect(0.0, 0.0, 1.0, 0.5),
            image: rect(-0.2, -0.4, 2.0, 2.0),
        };
        let read = page_panels(&panel_pair(panel, 77));
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0].ordinal, 1);
        for (got, want) in [
            (read[0].source, panel.source),
            (read[0].window, panel.window),
            (read[0].image, panel.image),
        ] {
            for (a, b) in [
                (got.left, want.left),
                (got.top, want.top),
                (got.width, want.width),
                (got.height, want.height),
            ] {
                assert!((a - b).abs() < 1e-4, "{a} != {b} in {read:?}");
            }
        }
    }

    #[test]
    fn panels_come_back_in_ordinal_order() {
        let make = |ordinal: u32| crate::model::Panel {
            ordinal,
            source: rect(0.0, 0.0, 1.0, 1.0),
            window: rect(0.0, 0.0, 1.0, 1.0),
            image: rect(0.0, 0.0, 1.0, 1.0),
        };
        let mut items = panel_pair(make(3), 30);
        items.extend(panel_pair(make(1), 10));
        items.extend(panel_pair(make(2), 20));
        let read = page_panels(&items);
        assert_eq!(
            read.iter().map(|p| p.ordinal).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_region_whose_target_is_absent_yields_no_panel() {
        let panel = crate::model::Panel {
            ordinal: 0,
            source: rect(0.0, 0.0, 1.0, 1.0),
            window: rect(0.0, 0.0, 1.0, 1.0),
            image: rect(0.0, 0.0, 1.0, 1.0),
        };
        // The region alone: its `activate` names an id nothing carries.
        let region = vec![panel_pair(panel, 5).remove(0)];
        assert!(page_panels(&region).is_empty());
    }

    #[test]
    fn both_halves_of_a_pair_are_panel_elements() {
        let panel = crate::model::Panel {
            ordinal: 0,
            source: rect(0.0, 0.0, 1.0, 1.0),
            window: rect(0.0, 0.0, 1.0, 1.0),
            image: rect(0.0, 0.0, 1.0, 1.0),
        };
        for item in panel_pair(panel, 9) {
            assert!(is_panel_element(&item));
        }
        // A page image is not one.
        let image = IonValue::Struct(vec![(
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Image as u64),
        )]);
        assert!(!is_panel_element(&image));
    }

    #[test]
    fn read_px_accepts_bare_int_and_value_struct() {
        let fields = vec![
            (KfxSymbol::FixedWidth as u64, IonValue::Int(900)),
            (
                KfxSymbol::FixedHeight as u64,
                IonValue::Struct(vec![(KfxSymbol::Value as u64, IonValue::Int(1280))]),
            ),
            (KfxSymbol::Layout as u64, IonValue::Int(0)),
        ];
        assert_eq!(read_px(&fields, KfxSymbol::FixedWidth), Some(900));
        assert_eq!(read_px(&fields, KfxSymbol::FixedHeight), Some(1280));
        // Non-positive and absent → None.
        assert_eq!(read_px(&fields, KfxSymbol::Layout), None);
        assert_eq!(read_px(&fields, KfxSymbol::Width), None);
    }
}
