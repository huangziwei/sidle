//! Author-drawn comic panels read out of a fixed-layout page's own markup: an
//! `app-amzn-magnify` region per panel, and a target holding a window onto a
//! magnified copy of the page. Each rect resolves against its CSS containing
//! block — region and window against `viewport`, image against the window.

use std::collections::HashMap;

use crate::model::{Panel, PanelRect};

/// The class marking a magnifiable region's anchor.
const MAGNIFY_CLASS: &str = "app-amzn-magnify";

/// Read every panel on a page. `css` holds the text of each stylesheet the page
/// links, `viewport` its pixel box for resolving px lengths. Panels come back
/// in `ordinal` order.
pub fn parse_panels(html: &str, css: &[String], viewport: (u32, u32)) -> Vec<Panel> {
    let rules = id_rules(css);

    let mut panels: Vec<Panel> = Vec::new();
    for (source, ordinal, target_id) in magnify_anchors(html, &rules, viewport) {
        // `target_id` names a window holding the magnified image.
        let Some((window, image)) = target_contents(html, &target_id, &rules, viewport) else {
            continue;
        };
        panels.push(Panel {
            ordinal,
            source,
            window,
            image,
        });
    }
    panels.sort_by_key(|p| p.ordinal);
    panels
}

/// A rectangle mid-parse: each side is present or not, in percent or px.
#[derive(Debug, Clone, Copy, Default)]
struct Rect {
    left: Option<Length>,
    top: Option<Length>,
    width: Option<Length>,
    height: Option<Length>,
}

#[derive(Debug, Clone, Copy)]
enum Length {
    Percent(f32),
    Px(f32),
}

impl Length {
    /// As a fraction of an axis `extent` px long.
    fn fraction(self, extent: f32) -> f32 {
        match self {
            Length::Percent(p) => p / 100.0,
            Length::Px(v) if extent > 0.0 => v / extent,
            Length::Px(_) => 0.0,
        }
    }
}

impl Rect {
    /// Fill any side this rect leaves open from `other`.
    fn merge(&mut self, other: Rect) {
        self.left = self.left.or(other.left);
        self.top = self.top.or(other.top);
        self.width = self.width.or(other.width);
        self.height = self.height.or(other.height);
    }

    /// A rect with a width and a height resolves; an open side reads as `0`.
    fn resolve(self, (vw, vh): (u32, u32)) -> Option<PanelRect> {
        let (w, h) = (vw as f32, vh as f32);
        Some(PanelRect {
            left: self.left.map_or(0.0, |l| l.fraction(w)),
            top: self.top.map_or(0.0, |l| l.fraction(h)),
            width: self.width?.fraction(w),
            height: self.height?.fraction(h),
        })
    }
}

/// `top` / `left` / `width` / `height` out of a declaration block.
fn parse_rect(decls: &str) -> Rect {
    let mut rect = Rect::default();
    for decl in decls.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let length = if let Some(p) = value.strip_suffix('%') {
            p.trim().parse::<f32>().ok().map(Length::Percent)
        } else if let Some(p) = value.strip_suffix("px") {
            p.trim().parse::<f32>().ok().map(Length::Px)
        } else {
            None
        };
        let Some(length) = length else { continue };
        match name.trim().to_ascii_lowercase().as_str() {
            "left" => rect.left = Some(length),
            "top" => rect.top = Some(length),
            "width" => rect.width = Some(length),
            "height" => rect.height = Some(length),
            _ => {}
        }
    }
    rect
}

/// Every `#id { … }` rule's rect, later rules winning.
fn id_rules(css: &[String]) -> HashMap<String, Rect> {
    let mut out: HashMap<String, Rect> = HashMap::new();
    for sheet in css {
        let mut rest = sheet.as_str();
        while let Some(hash) = rest.find('#') {
            rest = &rest[hash + 1..];
            let Some(open) = rest.find('{') else { break };
            let selector = rest[..open].trim();
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            let block = &rest[open + 1..open + close];
            // `#reg-1` and `#reg-1 img` name different boxes; the descendant
            // form is keyed by its whole selector.
            let key = selector.split_whitespace().collect::<Vec<_>>().join(" ");
            if !key.is_empty() && !key.contains(',') {
                out.entry(key).or_default().merge(parse_rect(block));
            }
            rest = &rest[open + close..];
        }
    }
    out
}

/// `(region rect, ordinal, target id)` for every magnify anchor, in document
/// order. The region is the innermost enclosing element resolving to a
/// rectangle, from an `#id` rule or an inline `style=`.
fn magnify_anchors(
    html: &str,
    rules: &HashMap<String, Rect>,
    viewport: (u32, u32),
) -> Vec<(PanelRect, u32, String)> {
    let mut open: Vec<Rect> = Vec::new();
    let mut out = Vec::new();
    for tag in tags(html) {
        let name = tag_name(tag);
        if tag.starts_with("</") {
            open.pop();
            continue;
        }
        let mut rect = parse_rect(&attr(tag, "style").unwrap_or_default());
        if let Some(id) = attr(tag, "id") {
            rect.merge(rules.get(&id).copied().unwrap_or_default());
        }
        let self_closing = tag.ends_with("/>") || matches!(name.as_str(), "img" | "br" | "meta");
        if !self_closing {
            open.push(rect);
        }
        if !attr(tag, "class").is_some_and(|c| c.split_whitespace().any(|t| t == MAGNIFY_CLASS)) {
            continue;
        }
        let Some(payload) = attr(tag, "data-app-amzn-magnify") else {
            continue;
        };
        let (Some(target), Some(ordinal)) = (
            json_string(&payload, "targetId"),
            json_number(&payload, "ordinal"),
        ) else {
            continue;
        };
        // The anchor's own 100%×100% box is skipped; the region is the first
        // enclosing rectangle stating a real size.
        let region = open
            .iter()
            .rev()
            .skip(usize::from(!self_closing))
            .find_map(|r| r.resolve(viewport))
            .or_else(|| rect.resolve(viewport));
        if let Some(region) = region {
            out.push((region, ordinal, target));
        }
    }
    out
}

/// The window and image rects inside a named magnify target. The window is the
/// innermost element enclosing the `<img>`, not a page-sized letterbox sibling.
/// `window` comes back in page fractions, `image` in window fractions.
fn target_contents(
    html: &str,
    target_id: &str,
    rules: &HashMap<String, Rect>,
    viewport: (u32, u32),
) -> Option<(PanelRect, PanelRect)> {
    let start = find_element(html, target_id)?;
    let depth_end = element_end(&html[start..]).map_or(html.len(), |e| start + e);
    let inner = &html[start..depth_end];

    // Enclosing elements, innermost last, each with the id its CSS rules key on.
    let mut open: Vec<(Rect, Option<String>)> = Vec::new();
    for tag in tags(inner) {
        let name = tag_name(tag);
        if tag.starts_with("</") {
            open.pop();
            continue;
        }
        let id = attr(tag, "id");
        let mut rect = parse_rect(&attr(tag, "style").unwrap_or_default());
        if let Some(id) = id.as_deref() {
            rect.merge(rules.get(id).copied().unwrap_or_default());
        }

        if name == "img" {
            let (window_rect, window_id) = open
                .iter()
                .rev()
                .find(|(r, id)| id.as_deref() != Some(target_id) && r.resolve(viewport).is_some())
                .map(|(r, id)| (r.resolve(viewport).unwrap(), id.clone()))?;
            if let Some(id) = window_id.as_deref() {
                rect.merge(rules.get(&format!("{id} img")).copied().unwrap_or_default());
            }
            for (side, attr_name) in [(&mut rect.width, "width"), (&mut rect.height, "height")] {
                if side.is_none() {
                    *side = attr(tag, attr_name)
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .map(Length::Px);
                }
            }
            let window_px = (
                (window_rect.width * viewport.0 as f32).round().max(1.0) as u32,
                (window_rect.height * viewport.1 as f32).round().max(1.0) as u32,
            );
            return Some((window_rect, rect.resolve(window_px)?));
        }

        if !tag.ends_with("/>") && !matches!(name.as_str(), "br" | "meta") {
            open.push((rect, id));
        }
    }
    None
}

/// Byte offset of the element carrying `id`.
fn find_element(html: &str, id: &str) -> Option<usize> {
    let mut at = 0;
    for tag in tags(html) {
        let offset = tag.as_ptr() as usize - html.as_ptr() as usize;
        if attr(tag, "id").as_deref() == Some(id) {
            return Some(offset);
        }
        at = offset;
    }
    let _ = at;
    None
}

/// Byte offset just past the element opening at the start of `s`.
fn element_end(s: &str) -> Option<usize> {
    let name = tag_name(tags(s).next()?);
    let mut depth = 0usize;
    for tag in tags(s) {
        if tag_name(tag) != name {
            continue;
        }
        let offset = tag.as_ptr() as usize - s.as_ptr() as usize;
        if tag.starts_with("</") {
            depth -= 1;
            if depth == 0 {
                return Some(offset + tag.len());
            }
        } else if !tag.ends_with("/>") {
            depth += 1;
        }
    }
    None
}

/// Every `<…>` tag in document order.
fn tags(html: &str) -> impl Iterator<Item = &str> {
    let mut rest = html;
    std::iter::from_fn(move || {
        loop {
            let open = rest.find('<')?;
            let close = rest[open..].find('>')?;
            let tag = &rest[open..open + close + 1];
            rest = &rest[open + close + 1..];
            if !tag.starts_with("<!") && !tag.starts_with("<?") {
                return Some(tag);
            }
        }
    })
}

/// A tag's lowercased element name.
fn tag_name(tag: &str) -> String {
    tag.trim_start_matches(['<', '/'])
        .split([' ', '\t', '\n', '\r', '>', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// An attribute's value, entity-decoded for the quote and ampersand forms a
/// JSON payload rides in on.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    loop {
        let at = lower[from..].find(name)? + from;
        let before = lower[..at].chars().next_back();
        let after = lower[at + name.len()..].chars().next();
        from = at + name.len();
        if !matches!(before, Some(c) if c.is_whitespace() || c == '<') {
            continue;
        }
        if !matches!(after, Some('=') | Some(' ')) {
            continue;
        }
        let rest = &tag[at + name.len()..];
        let eq = rest.find('=')?;
        let rest = rest[eq + 1..].trim_start();
        let quote = rest.chars().next()?;
        let value = if quote == '"' || quote == '\'' {
            let end = rest[1..].find(quote)? + 1;
            &rest[1..end]
        } else {
            let end = rest.find(['>', ' ']).unwrap_or(rest.len());
            &rest[..end]
        };
        return Some(
            value
                .replace("&quot;", "\"")
                .replace("&#34;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&"),
        );
    }
}

/// A JSON object's string member.
fn json_string(json: &str, key: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\""))?;
    let rest = &json[at..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// A JSON object's unsigned-integer member.
fn json_number(json: &str, key: &str) -> Option<u32> {
    let at = json.find(&format!("\"{key}\""))?;
    let rest = &json[at..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Geometry in a linked stylesheet, keyed by id.
    #[test]
    fn a_panel_reads_its_geometry_from_the_page_stylesheet() {
        let html = r#"<div class="fs">
            <img src="p.jpg" class="singlePage" width="1280" height="907"/>
            <div id="reg-1"><a class="app-amzn-magnify"
               data-app-amzn-magnify="{&quot;targetId&quot;: &quot;reg-1-magTargetParent&quot;, &quot;ordinal&quot;: 1}"></a></div>
            <div id="reg-1-magTargetParent" class="target-mag-parent">
              <div class="target-mag-lb"></div>
              <div id="reg-1-magTarget" class="target-mag">
                <img src="p.jpg" class="target-mag" width="2560" height="1814"/>
              </div>
            </div>
        </div>"#;
        let css = vec![
            "#reg-1 { top: 7.71%; left: 4.45%; height: 40.9%; width: 41.71%; }\n\
             #reg-1-magTarget { top: 3.74%; left: 2.34%; height: 81.69%; width: 83.35%; }\n\
             #reg-1-magTarget img { top: -18.78%; left: -10.6%; width: 2560px; height: 1814px; }"
                .to_string(),
        ];

        let panels = parse_panels(html, &css, (1280, 907));
        assert_eq!(panels.len(), 1);
        let p = panels[0];
        assert_eq!(p.ordinal, 1);
        assert!((p.source.left - 0.0445).abs() < 1e-4);
        assert!((p.source.width - 0.4171).abs() < 1e-4);
        assert!((p.window.height - 0.8169).abs() < 1e-4);
        // 2560px against a window 83.35% of a 1280px page.
        assert!((p.image.width - 2560.0 / (0.8335 * 1280.0)).abs() < 1e-3);
        assert!((p.image.left - -0.106).abs() < 1e-4);
    }

    /// The same numbers inline.
    #[test]
    fn a_panel_reads_its_geometry_from_inline_styles() {
        let html = r#"<div style="position:absolute; left:4.66%; top:4.74%; width:90.38%; height:19.29%;"
              id="reg-1"><a data-app-amzn-magnify="{&quot;targetId&quot;: &quot;reg-1-mag&quot;, &quot;ordinal&quot;:1}"
              class="app-amzn-magnify"></a></div>
            <div id="reg-1-mag" style="display:none; width:1800px; height:2700px;">
              <div style="position:absolute; left:0px; top:1061.9px; width:1800px; height:576px;">
                <img src="p.jpg" style="position:absolute; left:-93.9px; top:-141.5px; width:1990.1px; height:2985.2px;"/>
              </div>
            </div>"#;

        let panels = parse_panels(html, &[], (1800, 2700));
        assert_eq!(panels.len(), 1);
        let p = panels[0];
        assert!((p.source.width - 0.9038).abs() < 1e-4);
        // 1990.1px against a window the full 1800px page width.
        assert!((p.image.width - 1990.1 / 1800.0).abs() < 1e-4);
        assert!(p.image.top < 0.0);
    }

    /// Panels come back in the order the author numbered them.
    #[test]
    fn panels_sort_by_ordinal() {
        let page = |n: u32| {
            format!(
                r#"<div id="r{n}" style="left:0%; top:0%; width:10%; height:10%">
                     <a class="app-amzn-magnify" data-app-amzn-magnify='{{"targetId":"t{n}","ordinal":{n}}}'></a></div>
                   <div id="t{n}"><div id="w{n}" style="left:0%; top:0%; width:100%; height:100%">
                     <img style="left:0%; top:0%; width:200%; height:200%"/></div></div>"#
            )
        };
        let html = format!("{}{}", page(3), page(1));
        let panels = parse_panels(&html, &[], (100, 100));
        assert_eq!(
            panels.iter().map(|p| p.ordinal).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    /// A page with no magnify markup has no panels.
    #[test]
    fn a_plain_page_has_no_panels() {
        assert!(parse_panels("<div><img src=\"p.jpg\"/></div>", &[], (100, 100)).is_empty());
    }
}

#[cfg(test)]
mod geometry_invariant {
    use crate::model::Panel;

    /// The page-image rectangle a panel's magnified view shows: the window's
    /// own 0..1 box mapped back through `image`.
    fn visible(p: &Panel) -> (f32, f32, f32, f32) {
        (
            -p.image.left / p.image.width,
            -p.image.top / p.image.height,
            1.0 / p.image.width,
            1.0 / p.image.height,
        )
    }

    /// `visible` equals `source`: a magnified panel shows its own rectangle.
    #[test]
    #[ignore = "needs the AZW3 fixtures under artifacts/"]
    fn a_magnified_panel_shows_its_own_source_rect() {
        for path in [
            "../artifacts/graphicnovel-azw3/original/Tetris_ The Games People Play_B01M28OM76.azw3",
            "../artifacts/graphicnovel-azw3/original/Eight Million Ways to Die_B07984B2HK.azw3",
        ] {
            let book = crate::Book::open(path).unwrap();
            let mut checked = 0usize;
            let mut worst = 0.0f32;
            let mut worst_panel = None;
            let (mut over1, mut over05) = (0usize, 0usize);
            for entry in book.spine() {
                for p in &entry.panels {
                    let (u, v, du, dv) = visible(p);
                    let off = [
                        (u - p.source.left).abs(),
                        (v - p.source.top).abs(),
                        (du - p.source.width).abs(),
                        (dv - p.source.height).abs(),
                    ]
                    .into_iter()
                    .fold(0.0f32, f32::max);
                    if off > 0.01 {
                        over1 += 1;
                    }
                    if off > 0.005 {
                        over05 += 1;
                    }
                    if off > worst {
                        worst = off;
                        worst_panel = Some((*p, (u, v, du, dv)));
                    }
                    checked += 1;
                }
            }
            assert!(checked > 500, "{path}: only {checked} panels");
            println!(
                "{path}: {checked} panels, worst {worst:.5}, over 1%: {over1}, over 0.5%: {over05}"
            );
            // At most one panel in a hundred may exceed 1%.
            assert!(
                over1 * 100 < checked,
                "{path}: {over1} of {checked} panels off by over 1%, worst {worst}\n  \
                 worst: {worst_panel:#?}"
            );
        }
    }
}
