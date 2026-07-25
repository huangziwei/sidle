//! High-performance HTML transformation for MOBI/KF8 processing.
//!
//! Handles kindle: reference transformation and attribute stripping.

use std::collections::HashMap;

use bstr::ByteSlice;
use memchr::memmem;

use super::parse_base32;
use super::parser::DivElement;

/// Result of finding a kindle reference in the input.
struct KindleRef {
    /// End position (after closing quote/paren).
    end: usize,
    /// Type of reference.
    kind: RefKind,
}

enum RefKind {
    /// kindle:flow:XXXX?mime=...
    Flow { flow_num: usize },
    /// kindle:pos:fid:XXXX:off:YYYY
    PosFid { elem_idx: usize, offset: usize },
    /// kindle:pos:fid:XXXX (old format)
    PosFidOld { elem_idx: usize },
    /// kindle:embed:XXXX?mime=...
    Embed { img_idx: usize, ext: &'static str },
    /// Malformed reference to remove.
    Malformed,
}

/// Transform kindle: references in HTML to standard EPUB-style paths.
///
/// Converts:
/// - `kindle:flow:XXXX` → `styles/styleNNNN.css`
/// - `kindle:pos:fid:XXXX:off:YYYY` → `partNNNN.html#id` or `partNNNN.html`
/// - `kindle:embed:XXXX` → `images/image_NNNN.ext`
///
/// `embed_paths` maps a resource index to the asset path the importer
/// actually extracted (extension from the record's magic bytes). An embed
/// URL's own `?mime=` is the publisher's claim about that record and can
/// disagree with the bytes, so the map wins where it has an entry — same
/// resolution [`rewrite_kindle_embed_in_css`] uses on the CSS side.
pub fn transform_kindle_refs(
    html: &[u8],
    elems: &[DivElement],
    raw_text: &[u8],
    file_starts: &[(u32, u32)],
    embed_paths: &HashMap<usize, String>,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(html.len());
    let mut pos = 0;

    let finder = memmem::Finder::new(b"kindle:");

    while let Some(rel_start) = finder.find(&html[pos..]) {
        let start = pos + rel_start;
        output.extend_from_slice(&html[pos..start]);

        if let Some(kindle_ref) = parse_kindle_ref(&html[start..]) {
            let replacement =
                generate_replacement(&kindle_ref, elems, raw_text, file_starts, embed_paths);
            output.extend_from_slice(&replacement);
            pos = start + kindle_ref.end;
        } else {
            output.extend_from_slice(b"kindle:");
            pos = start + 7;
        }
    }

    output.extend_from_slice(&html[pos..]);
    output
}

/// Parse a kindle: reference starting at the given position.
fn parse_kindle_ref(data: &[u8]) -> Option<KindleRef> {
    if !data.starts_with(b"kindle:") {
        return None;
    }

    let end_pos = data[7..]
        .iter()
        .position(|&b| b == b'"' || b == b'\'' || b == b')')?;
    let end = 7 + end_pos;
    let content = &data[7..end];

    let kind = if content.starts_with(b"flow:") {
        let id_end = content[5..].find_byte(b'?').unwrap_or(content.len() - 5);
        let flow_num = parse_base32(&content[5..5 + id_end]);
        RefKind::Flow { flow_num }
    } else if content.starts_with(b"pos:fid:") {
        parse_pos_fid(content)
    } else if content.starts_with(b"embed:") {
        let id_end = content[6..].find_byte(b'?').unwrap_or(content.len() - 6);
        let img_num = parse_base32(&content[6..6 + id_end]);
        let img_idx = img_num.saturating_sub(1);

        let ext = if content.find(b"image/png").is_some() {
            "png"
        } else if content.find(b"image/gif").is_some() {
            "gif"
        } else {
            "jpg"
        };

        RefKind::Embed { img_idx, ext }
    } else {
        RefKind::Malformed
    };

    Some(KindleRef { end, kind })
}

/// Parse kindle:pos:fid:... format.
fn parse_pos_fid(content: &[u8]) -> RefKind {
    let rest = &content[8..]; // After "pos:fid:"

    let fid_end = rest.find_byte(b':').unwrap_or(rest.len());
    let elem_idx = parse_base32(&rest[..fid_end]);

    if fid_end < rest.len() && rest[fid_end..].starts_with(b":off:") {
        let off_start = fid_end + 5;
        let offset = parse_base32(&rest[off_start..]);
        RefKind::PosFid { elem_idx, offset }
    } else {
        RefKind::PosFidOld { elem_idx }
    }
}

/// Generate replacement text for a kindle reference.
fn generate_replacement(
    kindle_ref: &KindleRef,
    elems: &[DivElement],
    raw_text: &[u8],
    file_starts: &[(u32, u32)],
    embed_paths: &HashMap<usize, String>,
) -> Vec<u8> {
    match &kindle_ref.kind {
        RefKind::Flow { flow_num } => {
            let css_idx = flow_num.saturating_sub(1);
            format!("styles/style{:04}.css", css_idx).into_bytes()
        }
        RefKind::PosFid { elem_idx, offset } => {
            let (file_num, anchor) =
                resolve_pos_fid(*elem_idx, *offset, elems, raw_text, file_starts);
            if let Some((val, is_aid)) = anchor {
                let id = format_anchor_name(&val, is_aid);
                format!("part{:04}.html#{}", file_num, id).into_bytes()
            } else {
                format!("part{:04}.html", file_num).into_bytes()
            }
        }
        RefKind::PosFidOld { elem_idx } => {
            let file_num = elems
                .get(*elem_idx)
                .map(|e| e.file_number as usize)
                .unwrap_or(0);
            format!("part{:04}.html", file_num).into_bytes()
        }
        RefKind::Embed { img_idx, ext } => {
            // The extracted asset's name is authoritative — its extension comes
            // from the record's magic bytes. Fall back to the URL's `?mime=`
            // only for an index that was never extracted (no asset to name).
            if let Some(path) = embed_paths.get(img_idx) {
                return path.clone().into_bytes();
            }
            format!("images/image_{:04}.{}", img_idx, ext).into_bytes()
        }
        RefKind::Malformed => Vec::new(),
    }
}

/// Resolve a `kindle:pos:fid` target to its skeleton file number and the
/// nearest anchor attribute: `(value, came_from_aid)`.
fn resolve_pos_fid(
    elem_idx: usize,
    offset: usize,
    elems: &[DivElement],
    raw_text: &[u8],
    file_starts: &[(u32, u32)],
) -> (usize, Option<(String, bool)>) {
    let Some(elem) = elems.get(elem_idx) else {
        return (0, None);
    };
    // A chunk dropped during reassembly (out of bounds / bad insert) has no
    // reassembled position — the link is unresolvable.
    if elem.reassembled_pos == u32::MAX {
        return (elem.file_number as usize, None);
    }
    // Reassembled chunk start + off (KindleUnpack's `getIDTag`): `raw_text`
    // is the reassembled flow (chunks spliced into their skeletons), and
    // `reassembled_pos` is where this chunk's content lands there. Resolving
    // against the on-disk `[skeleton][chunk]` flow instead makes the backward
    // walk from a chunk-start land on the skeleton's tail element.
    let file_num = elem.file_number as usize;
    let target_pos = elem.reassembled_pos + offset as u32;
    let anchor = find_nearest_id_kind(raw_text, target_pos as usize, file_num, file_starts);
    (file_num, anchor)
}

/// The anchor name a resolved attribute produces in hrefs: `aid` attributes
/// get an `aid-` prefix (KindleUnpack convention), real ids pass through.
fn format_anchor_name(val: &str, is_aid: bool) -> String {
    if is_aid {
        format!("aid-{}", val)
    } else {
        val.to_string()
    }
}

/// Collect the set of `aid` attribute values that are link targets.
///
/// KF8 stamps kindlegen's `aid` attribute on skeleton elements; internal
/// links (`kindle:pos:fid:XXXX:off:YYYY`) resolve to the nearest `id`,
/// `name`, or `aid` attribute. When the nearest is an `aid`, the emitted
/// href fragment is `#aid-{value}` — so that element must keep an
/// `id="aid-{value}"` in the output or the link dangles. This scans every
/// pos:fid reference in `scan_text` (the whole decompressed text, so links
/// inside auxiliary flows count too) plus the NCX TOC byte positions in
/// `extra_positions` (`(byte_pos, file_num)` pairs — `resolve_toc` runs the
/// same nearest-attribute lookup), and returns the aid values those targets
/// resolve to. Mirrors calibre's `linked_aids` (mobi8.py).
pub fn collect_linked_aids(
    scan_text: &[u8],
    resolve_text: &[u8],
    elems: &[DivElement],
    file_starts: &[(u32, u32)],
    extra_positions: &[(usize, usize)],
) -> std::collections::HashSet<String> {
    let mut linked = std::collections::HashSet::new();
    let finder = memmem::Finder::new(b"kindle:pos:fid:");
    let mut pos = 0;
    while let Some(rel) = finder.find(&scan_text[pos..]) {
        let start = pos + rel;
        match parse_kindle_ref(&scan_text[start..]) {
            Some(KindleRef {
                end,
                kind: RefKind::PosFid { elem_idx, offset },
            }) => {
                let (_, anchor) =
                    resolve_pos_fid(elem_idx, offset, elems, resolve_text, file_starts);
                if let Some((val, true)) = anchor {
                    linked.insert(val);
                }
                pos = start + end;
            }
            Some(KindleRef { end, .. }) => pos = start + end,
            None => pos = start + b"kindle:pos:fid:".len(),
        }
    }
    for &(byte_pos, file_num) in extra_positions {
        if let Some((val, true)) =
            find_nearest_id_kind(resolve_text, byte_pos, file_num, file_starts)
        {
            linked.insert(val);
        }
    }
    linked
}

/// Resolve a `kindle:pos:fid` byte position to the anchor a `#…` href should
/// target, `aid-`-prefixing kindlegen `aid` values. See
/// `find_nearest_id_kind` for the resolution rule.
pub fn find_nearest_id_fast(
    raw_text: &[u8],
    pos: usize,
    file_num: usize,
    file_starts: &[(u32, u32)],
) -> Option<String> {
    find_nearest_id_kind(raw_text, pos, file_num, file_starts)
        .map(|(val, is_aid)| format_anchor_name(&val, is_aid))
}

/// Resolve a `kindle:pos:fid` byte position (in the reassembled flow:
/// `reassembled_pos + off`) to the owning element's anchor, returning
/// `(value, came_from_aid)`.
///
/// Mirrors KindleUnpack's `getIDTag`: walk backward tag by tag from `pos` to
/// the opening tag that owns the position, and return its anchor — preferring
/// a real `id` (then `name`) over the transient kindlegen `aid`. The
/// preference is what keeps this consistent with
/// [`strip_kindle_attributes_fast`]: an element that already carries an `id`
/// keeps it (a second `id` would be malformed XML), so a link into that
/// element must resolve to the existing `id`, not to an `aid-…` that never
/// gets injected. Elements with only an `aid` resolve to `aid-{value}` and the
/// stripper injects the matching `id`. Walking stops at `<body>` (→ `None`,
/// i.e. link to the top of the file).
fn find_nearest_id_kind(
    raw_text: &[u8],
    pos: usize,
    file_num: usize,
    file_starts: &[(u32, u32)],
) -> Option<(String, bool)> {
    // Calculate file bounds
    let (file_start, file_end) = {
        let mut start = 0usize;
        let mut end = raw_text.len();

        for (i, &(start_pos, fnum)) in file_starts.iter().enumerate() {
            if fnum as usize == file_num {
                start = start_pos as usize;
                if let Some(&(next_start, _)) = file_starts.get(i + 1) {
                    end = next_start as usize;
                }
                break;
            }
        }
        (start.min(raw_text.len()), end.min(raw_text.len()))
    };

    let pos = pos.clamp(file_start, file_end);

    // Upper bound (exclusive) for the `<` search — include the byte at `pos`
    // so a position sitting on an element's opening `<` resolves to that
    // element, not the previous one.
    let mut hi = (pos + 1).min(file_end);
    while hi > file_start {
        let Some(rel) = memchr::memrchr(b'<', &raw_text[file_start..hi]) else {
            break;
        };
        let lt = file_start + rel;
        let gt = memchr::memchr(b'>', &raw_text[lt..file_end])
            .map(|r| lt + r + 1)
            .unwrap_or(file_end);
        let tag = &raw_text[lt..gt];

        if tag.starts_with(b"<body") {
            // Reached the section wrapper: the link targets the file's top.
            return None;
        }
        // Only opening element tags carry an anchor.
        if !tag.starts_with(b"</")
            && !tag.starts_with(b"<!")
            && !tag.starts_with(b"<?")
            && let Some(anchor) = extract_tag_anchor(tag)
        {
            return Some(anchor);
        }

        // No anchor here (close tag / comment / anchorless open) — step
        // strictly before this `<` and keep walking back.
        hi = lt;
    }

    None
}

/// Extract an opening tag's anchor, preferring a real `id` (then `name`) over
/// the kindlegen `aid`. Returns `(value, came_from_aid)`.
fn extract_tag_anchor(tag: &[u8]) -> Option<(String, bool)> {
    if let Some(v) = attr_value_in_tag(tag, b"id") {
        Some((v, false))
    } else if let Some(v) = attr_value_in_tag(tag, b"name") {
        Some((v, false))
    } else {
        attr_value_in_tag(tag, b"aid").map(|v| (v, true))
    }
}

/// Read a single-/double-quoted attribute value from a tag's bytes. The
/// leading space in the needle (` name=`) prevents matching a longer
/// attribute name (`data-name=`). Returns `None` for a missing, unquoted,
/// empty, or non-id-charset value.
fn attr_value_in_tag(tag: &[u8], attr_name: &[u8]) -> Option<String> {
    let mut needle = Vec::with_capacity(attr_name.len() + 2);
    needle.push(b' ');
    needle.extend_from_slice(attr_name);
    needle.push(b'=');

    let start = memmem::find(tag, &needle)?;
    let after = start + needle.len();
    let quote = *tag.get(after)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let value_start = after + 1;
    let rel_end = tag[value_start..].iter().position(|&b| b == quote)?;
    let value = &tag[value_start..value_start + rel_end];
    if value.is_empty()
        || !value
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':' || b == b'.')
    {
        return None;
    }
    Some(String::from_utf8_lossy(value).into_owned())
}

/// Inline SVG flow content into `<img src="kindle:flow:NNNN..."/>` references.
///
/// KF8 illustration / full-page-art pages embed their SVG content in
/// auxiliary flows and reference it from the body XHTML via
/// `<img src="kindle:flow:NNNN?mime=image/svg+xml"/>`. The flow itself
/// contains `<svg viewBox="..."><image xlink:href="kindle:embed:NN"/></svg>`
/// wrapping a raster image. Calibre's `mobi8.py` handles this by inlining
/// the SVG bytes where the `<img>` tag was — see
/// calibre's MOBI input `reader/mobi8.py` around the
/// `image_tag_pattern.search(from_svg)` branch.
///
/// Must run BEFORE `transform_kindle_refs`: the inlined SVG content
/// contains `kindle:embed:NNNN` references that the regular transform
/// rewrites to `images/image_NNNN.ext`. Once this pass has run, every
/// remaining `kindle:flow:` reference is a CSS link (in `<link>` tags)
/// and the regular transform handles it.
pub fn inline_svg_flows(
    html: &[u8],
    flow_table: &[(usize, usize)],
    decompressed_text: &[u8],
) -> Vec<u8> {
    let mut output = Vec::with_capacity(html.len() * 2);
    let mut pos = 0;

    let needle = b"src=\"kindle:flow:";
    let finder = memmem::Finder::new(needle);

    while let Some(rel) = finder.find(&html[pos..]) {
        let src_pos = pos + rel;

        // Walk backward to the '<' that opens this tag.
        let Some(tag_start) = html[..src_pos].iter().rposition(|&b| b == b'<') else {
            output.extend_from_slice(&html[pos..]);
            return output;
        };

        // Read the tag name and reject anything that isn't <img> / <image>.
        // <link href="kindle:flow:..."> is a CSS link — leave it for
        // `transform_kindle_refs` to rewrite.
        let tag_body = tag_start + 1;
        let tag_name_end = tag_body
            + html[tag_body..]
                .iter()
                .position(|&b| b == b' ' || b == b'\t' || b == b'\n' || b == b'/' || b == b'>')
                .unwrap_or(html.len() - tag_body);
        let tag_name = &html[tag_body..tag_name_end];
        let is_img =
            tag_name.eq_ignore_ascii_case(b"img") || tag_name.eq_ignore_ascii_case(b"image");

        // Find the closing `>` of this tag.
        let Some(rel_end) = memchr::memchr(b'>', &html[tag_start..]) else {
            output.extend_from_slice(&html[pos..]);
            return output;
        };
        let tag_end = tag_start + rel_end + 1;

        if !is_img {
            // Not an inlineable tag — copy unchanged and continue past it.
            output.extend_from_slice(&html[pos..tag_end]);
            pos = tag_end;
            continue;
        }

        // Parse the flow number out of `kindle:flow:NNNN?...` or
        // `kindle:flow:NNNN"`. The number ends at `?` or the closing quote.
        let num_start = src_pos + needle.len();
        let num_end = html[num_start..]
            .iter()
            .position(|&b| b == b'?' || b == b'"' || b == b'\'')
            .map(|p| num_start + p)
            .unwrap_or(html.len());
        let flow_num = parse_base32(&html[num_start..num_end]);

        // Locate `<svg` within the flow content. Inline from there to the
        // flow's end (mirrors calibre `mobi8.py`'s `flowpart[start:]`
        // slice — strips any leading `<?xml-stylesheet ...?>` PI that KF8
        // prepends, since those aren't valid mid-XHTML).
        let svg_range = flow_table.get(flow_num).and_then(|&(start, end)| {
            let end = end.min(decompressed_text.len());
            if start > end {
                return None;
            }
            let head = &decompressed_text[start..end];
            // Sniff window — `<svg` should be near the top if this flow
            // is actually SVG. 1024 bytes is comfortably past any PI / BOM.
            let sniff_end = head.len().min(1024);
            memmem::Finder::new(b"<svg")
                .find(&head[..sniff_end])
                .map(|svg_pos| (start + svg_pos, end))
        });

        if let Some((svg_start, svg_end)) = svg_range {
            output.extend_from_slice(&html[pos..tag_start]);
            output.extend_from_slice(&decompressed_text[svg_start..svg_end]);
            pos = tag_end;
        } else {
            // Flow missing, not SVG, or out of range — leave the tag alone.
            // `transform_kindle_refs` will rewrite the URL to a CSS path,
            // which produces a broken `<img src="...css">` but matches
            // prior behavior rather than silently dropping content.
            output.extend_from_slice(&html[pos..tag_end]);
            pos = tag_end;
        }
    }

    output.extend_from_slice(&html[pos..]);
    output
}

/// Strip Amazon-specific attributes from HTML.
///
/// Removes: aid="...", data-AmznRemoved..., data-AmznPageBreak="..."
/// Rewrite `kindle:flow:NNNN?...` URLs inside a CSS file to sibling-relative
/// `styleNNNN.css` paths.
///
/// Native Amazon AZW3 stylesheets often chain-load each other with
/// `@import url(kindle:flow:0001?mime=text/css);`. Bokai emits the flow-table
/// CSS verbatim (P1.1), which preserves these unresolvable URLs — Apple Books
/// silently drops the import and the chained rules (writing-mode among them)
/// never load. Calibre-exported AZW3s don't have this defect because calibre
/// pre-resolves imports during its EPUB → AZW3 stage.
///
/// Sibling-relative output (`style0000.css` rather than `styles/style0000.css`)
/// because the CSS file is itself already inside `styles/` in the EPUB zip.
pub fn rewrite_kindle_flow_in_css(css: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(css.len());
    let mut pos = 0;
    let needle = b"kindle:flow:";
    while let Some(rel) = memmem::find(&css[pos..], needle) {
        let start = pos + rel;
        out.extend_from_slice(&css[pos..start]);
        let after_kw = start + needle.len();
        // Parse the flow number until the first non-base32 char.
        let mut num_end = after_kw;
        while num_end < css.len() {
            let b = css[num_end];
            let is_b32 = b.is_ascii_digit() || (b'A'..=b'V').contains(&b);
            if !is_b32 {
                break;
            }
            num_end += 1;
        }
        if num_end == after_kw {
            // Not a real flow ref — emit the literal and continue past `kindle:flow:`.
            out.extend_from_slice(needle);
            pos = after_kw;
            continue;
        }
        let flow_num = parse_base32(&css[after_kw..num_end]);
        let css_idx = flow_num.saturating_sub(1);
        let replacement = format!("style{:04}.css", css_idx);
        out.extend_from_slice(replacement.as_bytes());
        // Skip the optional `?mime=...` query string up to the closing `)`,
        // `"`, or `'` so we don't leave a dangling mime hint behind.
        let mut tail = num_end;
        if tail < css.len() && css[tail] == b'?' {
            while tail < css.len() {
                let b = css[tail];
                if b == b')' || b == b'"' || b == b'\'' {
                    break;
                }
                tail += 1;
            }
        }
        pos = tail;
    }
    out.extend_from_slice(&css[pos..]);
    out
}

/// Ensure the `<html>` root tag carries both `xml:lang` and `lang` attributes.
///
/// Calibre's AZW3 exporter scrubs `xml:lang` from the source HTML and leaves
/// only `lang=`, which makes count-level diffs against the original publisher
/// EPUB show a per-spine-doc xml:lang deficit (every chapter is `-1`). EPUB 3
/// recommends both — XHTML processors honor `xml:lang`, HTML5 processors
/// honor `lang`. Emitting both is universally compatible.
///
/// Behavior:
/// - Has both: untouched.
/// - Has only `lang=`: adds `xml:lang=` with the same value.
/// - Has only `xml:lang=`: adds `lang=` with the same value.
/// - Has neither: adds both with `default_lang` (skipped when empty).
/// - No `<html` tag at all: input returned unchanged.
pub fn ensure_html_lang_dual(html: &[u8], default_lang: &str) -> Vec<u8> {
    let Some(tag_start) = memmem::find(html, b"<html") else {
        return html.to_vec();
    };
    let after_html = tag_start + 5;
    let Some(rel_end) = memchr::memchr(b'>', &html[after_html..]) else {
        return html.to_vec();
    };
    let tag_end = after_html + rel_end;
    let attrs = &html[after_html..tag_end];

    // A repeated `lang`/`xml:lang` is malformed XML (epubcheck RSC-016
    // fatal, which kills all content checks for the file). Earlier versions
    // of this pass produced exactly that whenever the source `<html>` led
    // with a lang attribute, so already-converted books can carry the dup —
    // keep only the first occurrence of each before filling in gaps.
    let mut lang_spans = attr_spans(attrs, b"lang");
    let mut xml_lang_spans = attr_spans(attrs, b"xml:lang");
    if lang_spans.len() > 1 || xml_lang_spans.len() > 1 {
        let mut drop = if lang_spans.len() > 1 {
            lang_spans.split_off(1)
        } else {
            Vec::new()
        };
        if xml_lang_spans.len() > 1 {
            drop.extend(xml_lang_spans.split_off(1));
        }
        drop.sort_unstable();
        let mut rebuilt = Vec::with_capacity(html.len());
        rebuilt.extend_from_slice(&html[..after_html]);
        let mut p = 0;
        for (s, e) in drop {
            let mut s = s;
            // Eat the whitespace run before the dropped attribute.
            while s > p && attrs[s - 1].is_ascii_whitespace() {
                s -= 1;
            }
            rebuilt.extend_from_slice(&attrs[p..s]);
            p = e;
        }
        rebuilt.extend_from_slice(&attrs[p..]);
        rebuilt.extend_from_slice(&html[tag_end..]);
        return ensure_html_lang_dual(&rebuilt, default_lang);
    }

    let existing_lang = extract_attr_value(attrs, b"lang");
    let existing_xml_lang = extract_attr_value(attrs, b"xml:lang");

    let (lang_val, xml_lang_val) = match (existing_lang, existing_xml_lang) {
        (Some(_), Some(_)) => return html.to_vec(),
        (Some(l), None) => (None, Some(l)),
        (None, Some(x)) => (Some(x), None),
        (None, None) if !default_lang.is_empty() => {
            (Some(default_lang.as_bytes()), Some(default_lang.as_bytes()))
        }
        (None, None) => return html.to_vec(),
    };

    let mut out = Vec::with_capacity(html.len() + 32);
    out.extend_from_slice(&html[..tag_end]);
    if let Some(v) = lang_val {
        out.push(b' ');
        out.extend_from_slice(b"lang=\"");
        out.extend_from_slice(v);
        out.push(b'"');
    }
    if let Some(v) = xml_lang_val {
        out.push(b' ');
        out.extend_from_slice(b"xml:lang=\"");
        out.extend_from_slice(v);
        out.push(b'"');
    }
    out.extend_from_slice(&html[tag_end..]);
    out
}

/// Byte spans (`start..end` into `attrs`) of every `name="…"`/`name='…'`
/// occurrence, whitespace-boundary matched so `xml:lang` never matches a
/// search for `lang`. Used to detect (and drop) repeated attributes.
fn attr_spans(attrs: &[u8], name: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let finder = memmem::Finder::new(name);
    let mut from = 0;
    while let Some(rel) = finder.find(&attrs[from..]) {
        let start = from + rel;
        let bounded = start == 0 || attrs[start - 1].is_ascii_whitespace();
        if !bounded {
            from = start + 1;
            continue;
        }
        let mut i = start + name.len();
        while i < attrs.len() && attrs[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= attrs.len() || attrs[i] != b'=' {
            from = start + 1;
            continue;
        }
        i += 1;
        while i < attrs.len() && attrs[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < attrs.len() && (attrs[i] == b'"' || attrs[i] == b'\'') {
            let quote = attrs[i];
            i += 1;
            while i < attrs.len() && attrs[i] != quote {
                i += 1;
            }
            if i < attrs.len() {
                i += 1;
            }
            spans.push((start, i));
            from = i;
        } else {
            from = start + 1;
        }
    }
    spans
}

/// Convert legacy MOBI6 block-layout attributes to inline CSS.
///
/// Kindlegen's MOBI6 paragraph model puts layout in attributes —
/// `<p height="1em" width="0pt" align="justify">` — which are invalid in
/// XHTML5 (epubcheck RSC-005) and silently ignored by EPUB readers, losing
/// the source's paragraph spacing/indent/justification. Mapping (per the
/// MOBI periodical format docs and calibre's reader): `height` →
/// `margin-top`, `width` → `text-indent` (paragraphs only), `align` →
/// `text-align`, merged into any existing `style` attribute. Applies to
/// `<p>`, `<div>`, `<blockquote>`; unit-less values get `px`.
pub fn convert_legacy_block_attrs(html: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(html.len());
    let mut pos = 0;
    while let Some(rel) = memchr::memchr(b'<', &html[pos..]) {
        let start = pos + rel;
        out.extend_from_slice(&html[pos..start]);
        let Some(end_rel) = memchr::memchr(b'>', &html[start..]) else {
            out.extend_from_slice(&html[start..]);
            return out;
        };
        let end = start + end_rel + 1;
        out.extend_from_slice(&convert_block_tag(&html[start..end]));
        pos = end;
    }
    out.extend_from_slice(&html[pos..]);
    out
}

/// [`convert_legacy_block_attrs`] helper: rewrite one
/// `<p|div|blockquote|img …>` open tag, translating its legacy layout
/// attributes into a `style` attr.
fn convert_block_tag(tag: &[u8]) -> Vec<u8> {
    let name_end = tag[1..]
        .iter()
        .position(|b| !b.is_ascii_alphanumeric())
        .map(|p| p + 1)
        .unwrap_or(tag.len());
    let name = &tag[1..name_end];
    let is_para = name.eq_ignore_ascii_case(b"p");
    let is_img = name.eq_ignore_ascii_case(b"img");
    if !(is_para
        || is_img
        || name.eq_ignore_ascii_case(b"div")
        || name.eq_ignore_ascii_case(b"blockquote"))
    {
        return tag.to_vec();
    }
    // Keep a self-closing tag's `/` out of the attribute slice so the
    // rebuilt tag can re-append it after any added `style`.
    let mut attrs_end = tag.len() - 1;
    let self_closing = attrs_end > name_end && tag[attrs_end - 1] == b'/';
    if self_closing {
        attrs_end -= 1;
    }
    let attrs = &tag[name_end..attrs_end];

    // CSS length: pass units through, default bare numbers to px.
    let css_len = |v: &[u8]| -> Option<String> {
        let s = String::from_utf8_lossy(v).trim().to_string();
        if s.is_empty() {
            return None;
        }
        Some(if s.bytes().all(|b| b.is_ascii_digit()) {
            format!("{s}px")
        } else {
            s
        })
    };
    let mut decls: Vec<String> = Vec::new();
    let mut drop_spans: Vec<(usize, usize)> = Vec::new();
    if is_img {
        // Only `align` is invalid on `<img>` (width/height as bare integers
        // are legal HTML). HTML4 image alignment: left/right floated the
        // image, everything else set the inline vertical alignment.
        for (s, e) in attr_spans(attrs, b"align") {
            if let Some(v) = extract_attr_value(&attrs[s..e], b"align") {
                let v = String::from_utf8_lossy(v).trim().to_ascii_lowercase();
                match v.as_str() {
                    "left" | "right" => decls.push(format!("float: {v}")),
                    "top" | "middle" | "bottom" | "baseline" => {
                        decls.push(format!("vertical-align: {v}"))
                    }
                    _ => {}
                }
            }
            drop_spans.push((s, e));
        }
    } else {
        for (attr, prop, paragraphs_only) in [
            (&b"height"[..], "margin-top", false),
            (&b"width"[..], "text-indent", true),
            (&b"align"[..], "text-align", false),
        ] {
            for (s, e) in attr_spans(attrs, attr) {
                // The attribute is invalid XHTML on any of these tags, so it
                // is always dropped; the CSS translation is emitted only
                // where the kindlegen semantics are known (`width` =
                // paragraph indent).
                let value = if paragraphs_only && !is_para {
                    None
                } else {
                    extract_attr_value(&attrs[s..e], attr)
                };
                if let Some(v) = value {
                    let css = if attr == b"align" {
                        let s = String::from_utf8_lossy(v).trim().to_ascii_lowercase();
                        matches!(s.as_str(), "left" | "right" | "center" | "justify").then_some(s)
                    } else {
                        css_len(v)
                    };
                    if let Some(css) = css {
                        decls.push(format!("{prop}: {css}"));
                    }
                }
                drop_spans.push((s, e));
            }
        }
    }
    if drop_spans.is_empty() {
        return tag.to_vec();
    }

    // Rebuild: name + surviving attrs (+ merged style) + '>'.
    drop_spans.sort_unstable();
    let mut kept = Vec::with_capacity(attrs.len());
    let mut cursor = 0;
    for (s, e) in &drop_spans {
        let mut s = *s;
        while s > cursor && attrs[s - 1].is_ascii_whitespace() {
            s -= 1;
        }
        kept.extend_from_slice(&attrs[cursor..s]);
        cursor = *e;
    }
    kept.extend_from_slice(&attrs[cursor..]);

    let mut out = Vec::with_capacity(tag.len());
    out.extend_from_slice(&tag[..name_end]);
    if !decls.is_empty() {
        let new_decls = decls.join("; ");
        if let Some((s, e)) = attr_spans(&kept, b"style").first().copied() {
            // Merge into the existing style attribute, before its close quote.
            let close = kept[..e]
                .iter()
                .rposition(|&b| b == b'"' || b == b'\'')
                .unwrap_or(e - 1);
            out.extend_from_slice(&kept[..close]);
            let existing = extract_attr_value(&kept[s..e], b"style").unwrap_or_default();
            if !existing.is_empty() {
                if existing.trim_ascii_end().ends_with(b";") {
                    out.push(b' ');
                } else {
                    out.extend_from_slice(b"; ");
                }
            }
            out.extend_from_slice(new_decls.as_bytes());
            out.extend_from_slice(&kept[close..]);
        } else {
            out.extend_from_slice(&kept);
            out.extend_from_slice(format!(" style=\"{new_decls}\"").as_bytes());
        }
    } else {
        out.extend_from_slice(&kept);
    }
    if self_closing {
        out.push(b'/');
    }
    out.push(b'>');
    out
}

/// Find `attr="value"` (or `attr='value'`) inside an attribute byte slice and
/// return the value. Looks for the attribute name preceded by ASCII
/// whitespace OR appearing at the start of the slice — avoids matching e.g.
/// `xml:lang` when searching for `lang`.
fn extract_attr_value<'a>(attrs: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let (start, end) = attr_spans(attrs, name).into_iter().next()?;
    let span = &attrs[start..end];
    let q = span.iter().position(|&b| b == b'"' || b == b'\'')?;
    let quote = span[q];
    let value_start = q + 1;
    let value_len = span[value_start..].iter().position(|&b| b == quote)?;
    Some(&span[value_start..value_start + value_len])
}

/// Drop `<link>` elements whose `href` escapes the package root (contains
/// `..`).
///
/// KF8 books converted from Aozora HTML carry a verbatim
/// `<link rel="alternate stylesheet" href="../styles/aNNNNN_h.css" title="横組">`
/// for the horizontal writing-mode variant — a reference to a stylesheet that
/// was never embedded as a flow. calibre passes it through and its reader just
/// ignores the dangling *alternate* sheet, but in bokai's flat EPUB layout the
/// parts and `styles/` are siblings under the OPF root, so a `..` href both
/// points at a missing file and climbs out of the container: two EPUB-3
/// violations that make a strict consumer (Apple Books, the downstream
/// `epub_to_kfx` job, our own `validate::source::epub`) reject the book. The
/// `kindle:flow:` sheets are rewritten to sibling `styles/styleNNNN.css`
/// paths, so no stylesheet bokai actually emits needs `..` — dropping these
/// dangling links is safe and loses nothing a reader would have applied.
pub fn strip_root_escaping_links(html: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(html.len());
    let mut pos = 0;

    while pos < html.len() {
        let Some(rel) = memchr::memchr(b'<', &html[pos..]) else {
            output.extend_from_slice(&html[pos..]);
            break;
        };
        let tag_start = pos + rel;
        output.extend_from_slice(&html[pos..tag_start]);

        let Some(rel_end) = memchr::memchr(b'>', &html[tag_start..]) else {
            output.extend_from_slice(&html[tag_start..]);
            break;
        };
        let tag_end = tag_start + rel_end + 1;

        if is_root_escaping_link(&html[tag_start..tag_end]) {
            // Drop the element. Swallow one trailing newline so we don't leave
            // a blank line where the <link> was (each sits on its own line).
            pos = tag_end;
            if html.get(pos) == Some(&b'\n') {
                pos += 1;
            }
        } else {
            output.extend_from_slice(&html[tag_start..tag_end]);
            pos = tag_end;
        }
    }

    output
}

/// True for a `<link ...>` start tag whose `href` value contains `..`.
fn is_root_escaping_link(tag: &[u8]) -> bool {
    let Some(rest) = tag.strip_prefix(b"<link") else {
        return false;
    };
    // Require a delimiter after the element name so `<linkfoo>` doesn't match.
    if !matches!(
        rest.first(),
        Some(b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>')
    ) {
        return false;
    }
    let Some(h) = memmem::find(tag, b"href=") else {
        return false;
    };
    let after = &tag[h + 5..];
    let (quote, body) = match after.first() {
        Some(&q @ (b'"' | b'\'')) => (q, &after[1..]),
        _ => return false,
    };
    let Some(close) = memchr::memchr(quote, body) else {
        return false;
    };
    memmem::find(&body[..close], b"..").is_some()
}

/// Drop `href`s on `<a>` elements that point at raster images.
///
/// kindlegen's in-book TOC sometimes links its "Cover" row straight at the
/// cover JPEG (`kindle:embed:…`, which [`transform_kindle_refs`] rewrites to
/// `images/image_NNNN.jpg`). EPUB 3 forbids hyperlinks to non-content
/// documents (epubcheck RSC-010) and every downstream consumer drops the
/// link anyway — keep the label, lose the href.
pub fn unlink_image_anchors(html: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(html.len());
    let mut pos = 0;
    while let Some(rel) = memmem::find(&html[pos..], b"<a ") {
        let start = pos + rel;
        out.extend_from_slice(&html[pos..start]);
        let Some(end_rel) = memchr::memchr(b'>', &html[start..]) else {
            out.extend_from_slice(&html[start..]);
            return out;
        };
        let end = start + end_rel + 1;
        out.extend_from_slice(&drop_image_href(&html[start..end]));
        pos = end;
    }
    out.extend_from_slice(&html[pos..]);
    out
}

/// [`unlink_image_anchors`] helper: rewrite one `<a …>` tag without its
/// `href` when that href targets an `images/…` raster file.
fn drop_image_href(tag: &[u8]) -> Vec<u8> {
    let attrs_start = 2; // past "<a"
    let attrs_end = tag.len() - 1; // before '>'
    let attrs = &tag[attrs_start..attrs_end];
    for (s, e) in attr_spans(attrs, b"href") {
        let span = &attrs[s..e];
        let Some(q) = span.iter().position(|&b| b == b'"' || b == b'\'') else {
            continue;
        };
        let val = &span[q + 1..span.len().saturating_sub(1)];
        let exts: [&[u8]; 4] = [b".jpg", b".jpeg", b".png", b".gif"];
        if val.starts_with(b"images/") && exts.iter().any(|ext| val.ends_with(ext)) {
            let mut s = s;
            while s > 0 && attrs[s - 1].is_ascii_whitespace() {
                s -= 1;
            }
            let mut out = Vec::with_capacity(tag.len());
            out.extend_from_slice(&tag[..attrs_start + s]);
            out.extend_from_slice(&attrs[e..]);
            out.push(b'>');
            return out;
        }
    }
    tag.to_vec()
}

/// Rewrite `kindle:embed:XXXX(?mime=…)` URLs in CSS to stylesheet-relative
/// asset paths via the embed-index → path map the importer built from its
/// discovered assets (fonts and images alike). Stylesheets live in
/// `styles/`, so every rewritten path gets a `../` prefix. Refs whose index
/// isn't in the map are left verbatim — [`strip_kindle_embed_font_faces`]
/// runs after this pass and drops the `@font-face` rules that still dangle.
pub fn rewrite_kindle_embed_in_css(
    css: &[u8],
    paths: &std::collections::HashMap<usize, String>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(css.len());
    let mut pos = 0;
    let needle = b"kindle:embed:";
    while let Some(rel) = memmem::find(&css[pos..], needle) {
        let start = pos + rel;
        out.extend_from_slice(&css[pos..start]);
        let after_kw = start + needle.len();
        // Parse the embed id until the first non-base32 char.
        let mut num_end = after_kw;
        while num_end < css.len() {
            let b = css[num_end];
            let is_b32 = b.is_ascii_digit() || (b'A'..=b'V').contains(&b);
            if !is_b32 {
                break;
            }
            num_end += 1;
        }
        // `kindle:embed:0001` is resource index 0 (1-based ids, like the
        // HTML-side `RefKind::Embed` rewrite).
        let embed_idx = parse_base32(&css[after_kw..num_end]).saturating_sub(1);
        let Some(path) = (num_end > after_kw)
            .then(|| paths.get(&embed_idx))
            .flatten()
        else {
            // Unresolvable — emit the literal so the strip pass sees it.
            out.extend_from_slice(needle);
            pos = after_kw;
            continue;
        };
        out.extend_from_slice(b"../");
        out.extend_from_slice(path.as_bytes());
        // Skip the optional `?mime=...` query up to the URL delimiter.
        let mut tail = num_end;
        if tail < css.len() && css[tail] == b'?' {
            while tail < css.len() {
                let b = css[tail];
                if b == b')' || b == b'"' || b == b'\'' {
                    break;
                }
                tail += 1;
            }
        }
        pos = tail;
    }
    out.extend_from_slice(&css[pos..]);
    out
}

/// Drop `@font-face` rules whose `src` references a `kindle:embed:` resource
/// that [`rewrite_kindle_embed_in_css`] could not resolve to an extracted
/// asset. A dangling URL would fail epubcheck (RSC-008/OPF-014); dropping the
/// whole rule lets the `font-family` fall back down its declared stack.
pub fn strip_kindle_embed_font_faces(css: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(css.len());
    let mut pos = 0;
    let finder = memmem::Finder::new(b"@font-face");
    while let Some(rel) = finder.find(&css[pos..]) {
        let start = pos + rel;
        let Some(open_rel) = memchr::memchr(b'{', &css[start..]) else {
            break;
        };
        let open = start + open_rel;
        let Some(close_rel) = memchr::memchr(b'}', &css[open..]) else {
            break;
        };
        let close = open + close_rel;
        out.extend_from_slice(&css[pos..start]);
        let block = &css[start..=close];
        if memmem::find(block, b"kindle:embed:").is_none() {
            out.extend_from_slice(block);
        }
        pos = close + 1;
    }
    out.extend_from_slice(&css[pos..]);
    out
}

/// Strip Amazon-specific attributes (`aid`, `data-Amzn*`) from every tag —
/// except link-target aids: an `aid` whose value is in `linked_aids` (some
/// `kindle:pos:fid` link resolves to it) is rewritten to `id="aid-{value}"`
/// so the `#aid-{value}` hrefs [`transform_kindle_refs`] emits actually
/// resolve. Pass an empty set to strip unconditionally.
pub fn strip_kindle_attributes_fast(
    html: &[u8],
    linked_aids: &std::collections::HashSet<String>,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(html.len());
    let mut pos = 0;

    while pos < html.len() {
        if let Some(tag_start) = memchr::memchr(b'<', &html[pos..]) {
            let abs_tag_start = pos + tag_start;
            output.extend_from_slice(&html[pos..abs_tag_start]);

            if let Some(tag_end) = memchr::memchr(b'>', &html[abs_tag_start..]) {
                let abs_tag_end = abs_tag_start + tag_end + 1;
                let tag = &html[abs_tag_start..abs_tag_end];

                let cleaned = clean_tag(tag, linked_aids);
                output.extend_from_slice(&cleaned);

                pos = abs_tag_end;
            } else {
                output.extend_from_slice(&html[abs_tag_start..]);
                break;
            }
        } else {
            output.extend_from_slice(&html[pos..]);
            break;
        }
    }

    output
}

/// Clean a single tag by removing Amazon-specific attributes.
fn clean_tag(tag: &[u8], linked_aids: &std::collections::HashSet<String>) -> Vec<u8> {
    // Skip comments and special tags
    if tag.starts_with(b"<!--")
        || tag.starts_with(b"<!DOCTYPE")
        || tag.starts_with(b"<?")
        || tag.starts_with(b"</")
    {
        return tag.to_vec();
    }

    let mut result = Vec::with_capacity(tag.len());
    let mut i = 0;
    let mut injected_id = false;

    // Copy tag name
    result.push(b'<');
    i += 1;

    while i < tag.len() && tag[i] != b' ' && tag[i] != b'>' && tag[i] != b'/' {
        result.push(tag[i]);
        i += 1;
    }

    // Process attributes
    while i < tag.len() {
        // Skip whitespace
        while i < tag.len() && (tag[i] == b' ' || tag[i] == b'\t' || tag[i] == b'\n') {
            result.push(tag[i]);
            i += 1;
        }

        if i >= tag.len() || tag[i] == b'>' || tag[i] == b'/' {
            break;
        }

        // Get attribute name
        let attr_start = i;
        while i < tag.len() && tag[i] != b'=' && tag[i] != b' ' && tag[i] != b'>' && tag[i] != b'/'
        {
            i += 1;
        }
        let attr_name = &tag[attr_start..i];

        // Check if this is an attribute to strip
        let should_strip = attr_name == b"aid"
            || attr_name.starts_with(b"data-Amzn")
            || attr_name.starts_with(b"data-amzn");

        if should_strip {
            // Skip the attribute value, capturing it — a link-target aid is
            // rewritten to an id below.
            let mut value: &[u8] = b"";
            if i < tag.len() && tag[i] == b'=' {
                i += 1;
                if i < tag.len() && (tag[i] == b'"' || tag[i] == b'\'') {
                    let quote = tag[i];
                    i += 1;
                    let value_start = i;
                    while i < tag.len() && tag[i] != quote {
                        i += 1;
                    }
                    value = &tag[value_start..i];
                    if i < tag.len() {
                        i += 1;
                    }
                } else {
                    let value_start = i;
                    while i < tag.len() && tag[i] != b' ' && tag[i] != b'>' {
                        i += 1;
                    }
                    value = &tag[value_start..i];
                }
            }
            // A linked aid becomes the element's id — `transform_kindle_refs`
            // emits hrefs pointing at `#aid-{value}`. Skipped when the tag
            // already carries an id (a second id attribute would be malformed
            // XML), so that rare link stays unresolved rather than breaking
            // the document.
            if attr_name == b"aid"
                && !injected_id
                && let Ok(val) = std::str::from_utf8(value)
                && linked_aids.contains(val)
                && !tag_has_id_attr(tag)
            {
                result.extend_from_slice(b"id=\"aid-");
                result.extend_from_slice(value);
                result.push(b'"');
                injected_id = true;
            }
        } else {
            // Keep this attribute
            result.extend_from_slice(attr_name);
            if i < tag.len() && tag[i] == b'=' {
                result.push(b'=');
                i += 1;
                if i < tag.len() && (tag[i] == b'"' || tag[i] == b'\'') {
                    let quote = tag[i];
                    result.push(quote);
                    i += 1;
                    let value_start = i;
                    while i < tag.len() && tag[i] != quote {
                        i += 1;
                    }
                    result.extend_from_slice(&tag[value_start..i]);
                    if i < tag.len() {
                        result.push(quote);
                        i += 1;
                    }
                } else {
                    let value_start = i;
                    while i < tag.len() && tag[i] != b' ' && tag[i] != b'>' {
                        i += 1;
                    }
                    result.extend_from_slice(&tag[value_start..i]);
                }
            }
        }
    }

    // Copy closing
    while i < tag.len() {
        result.push(tag[i]);
        i += 1;
    }

    // Ensure img tags have alt attribute
    if result.starts_with(b"<img ") || result.starts_with(b"<IMG ") {
        return ensure_img_alt(&result);
    }

    result
}

/// Whether a raw tag slice carries a real `id` attribute. Attribute-walk,
/// not substring search — ` aid=` and tab/newline separators must not fool
/// it, since a false negative here would inject a second id attribute
/// (malformed XML) in `clean_tag`.
fn tag_has_id_attr(tag: &[u8]) -> bool {
    let mut i = 1; // past '<'
    while i < tag.len()
        && tag[i] != b' '
        && tag[i] != b'\t'
        && tag[i] != b'\n'
        && tag[i] != b'>'
        && tag[i] != b'/'
    {
        i += 1;
    }
    while i < tag.len() {
        while i < tag.len() && (tag[i] == b' ' || tag[i] == b'\t' || tag[i] == b'\n') {
            i += 1;
        }
        if i >= tag.len() || tag[i] == b'>' || tag[i] == b'/' {
            break;
        }
        let attr_start = i;
        while i < tag.len() && tag[i] != b'=' && tag[i] != b' ' && tag[i] != b'>' && tag[i] != b'/'
        {
            i += 1;
        }
        if &tag[attr_start..i] == b"id" {
            return true;
        }
        if i < tag.len() && tag[i] == b'=' {
            i += 1;
            if i < tag.len() && (tag[i] == b'"' || tag[i] == b'\'') {
                let quote = tag[i];
                i += 1;
                while i < tag.len() && tag[i] != quote {
                    i += 1;
                }
                if i < tag.len() {
                    i += 1;
                }
            } else {
                while i < tag.len() && tag[i] != b' ' && tag[i] != b'>' {
                    i += 1;
                }
            }
        }
    }
    false
}

/// Ensure img tag has alt attribute.
fn ensure_img_alt(tag: &[u8]) -> Vec<u8> {
    if memmem::find(tag, b"alt=").is_some() {
        return tag.to_vec();
    }

    // Insert `alt=""` before the closing punctuation. For self-closing
    // `<img …/>` the alt must go *before* the slash; inserting it between
    // the `/` and `>` produces `<img …/ alt="">`, which Apple Books rejects
    // as "attributes construct error". A naive `rposition('/' or '>')`
    // returns the `>` index and lands the alt in that broken slot.
    let insert_pos = if tag.ends_with(b"/>") {
        tag.len() - 2
    } else if tag.ends_with(b">") {
        tag.len() - 1
    } else {
        return tag.to_vec();
    };

    let mut result = Vec::with_capacity(tag.len() + 7);
    result.extend_from_slice(&tag[..insert_pos]);
    if !result.ends_with(b" ") {
        result.push(b' ');
    }
    result.extend_from_slice(b"alt=\"\"");
    result.extend_from_slice(&tag[insert_pos..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_base32() {
        assert_eq!(parse_base32(b"0000"), 0);
        assert_eq!(parse_base32(b"0001"), 1);
        assert_eq!(parse_base32(b"000V"), 31);
        assert_eq!(parse_base32(b"0010"), 32);
    }

    #[test]
    fn test_strip_aid_attribute() {
        let input = b"<p aid=\"0001\">Hello</p>";
        let output = strip_kindle_attributes_fast(input, &Default::default());
        let output_str = String::from_utf8_lossy(&output);
        eprintln!("Output: {:?}", output_str);
        assert!(!output.contains_str("aid="));
        // After stripping aid, there may be trailing whitespace before >
        assert!(
            output_str.starts_with("<p") && output_str.contains(">Hello</p>"),
            "Expected <p...>Hello</p>, got: {}",
            output_str
        );
    }

    #[test]
    fn test_linked_aid_becomes_id() {
        // A link somewhere resolves to `#aid-5N3C2`, so the element carrying
        // aid="5N3C2" must keep that identity as an id; unlinked aids are
        // stripped as before.
        let linked: std::collections::HashSet<String> = ["5N3C2".to_string()].into();
        let input = b"<p aid=\"5N3C2\">target</p><p aid=\"XXXX\">other</p>";
        let out = strip_kindle_attributes_fast(input, &linked);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("id=\"aid-5N3C2\""),
            "linked aid must be rewritten to an id, got: {s}"
        );
        assert!(
            !s.contains("aid=\""),
            "raw aid attributes must not survive: {s}"
        );
        assert!(!s.contains("XXXX"), "unlinked aid must be stripped: {s}");
    }

    #[test]
    fn test_linked_aid_on_tag_with_existing_id_is_dropped() {
        // The tag already has an id — injecting a second id attribute would
        // be malformed XML, so the aid is stripped instead.
        let linked: std::collections::HashSet<String> = ["B4".to_string()].into();
        let input = b"<h1 id=\"ch1\" aid=\"B4\">One</h1>";
        let out = strip_kindle_attributes_fast(input, &linked);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("id=\"ch1\""), "existing id must survive: {s}");
        assert!(!s.contains("aid-B4"), "no second id may be injected: {s}");
        assert!(!s.contains("aid=\""), "aid attribute must be stripped: {s}");
    }

    #[test]
    fn test_tag_has_id_attr() {
        assert!(tag_has_id_attr(b"<p id=\"x\">"));
        assert!(tag_has_id_attr(b"<p class=\"c\" id='x'>"));
        // ` aid=` is not ` id=`.
        assert!(!tag_has_id_attr(b"<p aid=\"x\">"));
        // `id` inside an attribute VALUE doesn't count.
        assert!(!tag_has_id_attr(b"<p title=\"id=\">"));
        // Tab/newline separators still find the id.
        assert!(tag_has_id_attr(b"<p\tid=\"x\">"));
        assert!(tag_has_id_attr(b"<p\nid=\"x\">"));
        assert!(!tag_has_id_attr(b"<br/>"));
    }

    #[test]
    fn test_lang_as_first_attribute_is_found() {
        // `<html lang=… xml:lang=…>` with lang as the FIRST attribute: the
        // old candidate walk never looked at the first attribute (the attrs
        // slice starts with the separator space), returned None for `lang`,
        // and appended a duplicate — an XML well-formedness error (epubcheck
        // RSC-016) on every retail AZW3 whose html tag leads with lang.
        let html = b"<html lang=\"en-US\" xml:lang=\"en-US\" xmlns=\"http://www.w3.org/1999/xhtml\"><head></head></html>";
        assert_eq!(ensure_html_lang_dual(html, "en"), html);
    }

    #[test]
    fn test_duplicate_lang_attr_deduped() {
        // A repeated attribute is an XML well-formedness error (epubcheck
        // RSC-016); older bokai builds emitted this shape themselves (see
        // `test_lang_as_first_attribute_is_found`).
        let html = b"<html lang=\"en-US\" xml:lang=\"en-US\" xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"en-US\"><head></head></html>";
        let out = ensure_html_lang_dual(html, "en");
        let s = String::from_utf8_lossy(&out);
        assert_eq!(
            s.matches("lang=").count(),
            2, // one lang= + one xml:lang=
            "exactly one lang and one xml:lang must survive: {s}"
        );
        assert!(s.contains("xml:lang=\"en-US\""));
        assert!(s.contains("xmlns=\"http://www.w3.org/1999/xhtml\""));

        // Duplicate lang and NO xml:lang: dedupe, then the pair-up fills it in.
        let html = b"<html lang=\"ja\" lang=\"ja\"><head></head></html>";
        let out = ensure_html_lang_dual(html, "en");
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s.matches("lang=\"ja\"").count(), 2, "lang + xml:lang: {s}");
        assert!(s.contains("xml:lang=\"ja\""));

        // No duplicates: byte-identical to the old behavior.
        let html = b"<html lang=\"en\" xml:lang=\"en\"><head></head></html>";
        assert_eq!(ensure_html_lang_dual(html, ""), html);
    }

    #[test]
    fn test_strip_kindle_embed_font_faces() {
        let css = b"/* fonts */\n@font-face {\n\tfont-family:\"X\";\n\tsrc:url(kindle:embed:0001);\n}\n\n.para { margin: 0; }\n@font-face { font-family:\"Y\"; src:url(fonts/y.ttf); }\n";
        let out = strip_kindle_embed_font_faces(css);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("kindle:embed"), "embed font-face dropped: {s}");
        assert!(
            s.contains("font-family:\"Y\""),
            "local-src font-face kept: {s}"
        );
        assert!(s.contains(".para { margin: 0; }"));
    }

    #[test]
    fn test_find_nearest_id_prefers_enclosing_id() {
        // pos:fid resolution walks back to the element owning the (on-disk)
        // position and returns its anchor, preferring a real id/name over the
        // transient kindlegen aid — so a link into an already-id'd element
        // resolves to the surviving id, not an aid-… the stripper won't
        // re-inject onto an id'd tag.
        let raw = b"<html><head></head><body ><div class=\"main\" aid=\"AA\">\
<p id=\"para\" aid=\"BB\">text</p><h1 aid=\"CC\">head</h1></div></body></html>";
        let file_starts = [(0u32, 0u32)];

        // A position on the <p> that carries both id and aid → the id wins.
        let p_at = memmem::find(raw, b"<p id=").unwrap() + 3;
        assert_eq!(
            find_nearest_id_fast(raw, p_at, 0, &file_starts).as_deref(),
            Some("para"),
            "an existing id is preferred over the aid"
        );
        // A position on the aid-only <h1> → its aid, prefixed.
        let h1_at = memmem::find(raw, b"<h1 aid=").unwrap() + 3;
        assert_eq!(
            find_nearest_id_fast(raw, h1_at, 0, &file_starts).as_deref(),
            Some("aid-CC"),
        );
        // A position on the aid-only wrapper div → its aid.
        let div_at = memmem::find(raw, b"<div class").unwrap() + 3;
        assert_eq!(
            find_nearest_id_fast(raw, div_at, 0, &file_starts).as_deref(),
            Some("aid-AA"),
        );
        // A body-level position (before any content element) → None: the link
        // targets the top of the file, with no fragment.
        let body_at = memmem::find(raw, b"<body ").unwrap() + 3;
        assert_eq!(find_nearest_id_fast(raw, body_at, 0, &file_starts), None);
    }

    #[test]
    fn test_convert_legacy_block_attrs() {
        // The kindlegen MOBI6 paragraph model: layout as attributes.
        let html = b"<p height=\"1em\" width=\"0pt\" align=\"justify\">text</p>\
<div align=\"center\">c</div>\
<blockquote width=\"2em\" height=\"3\">q</blockquote>\
<img src=\"i.jpg\" width=\"100\" height=\"50\"/>\
<p style=\"color: red\" height=\"1em\">styled</p>";
        let out = convert_legacy_block_attrs(html);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains(
                "<p style=\"margin-top: 1em; text-indent: 0pt; text-align: justify\">text</p>"
            ),
            "paragraph trio converted: {s}"
        );
        assert!(
            s.contains("<div style=\"text-align: center\">c</div>"),
            "div align converted: {s}"
        );
        // Blockquote: height converts (bare number → px), width is a
        // paragraph-only mapping and is dropped rather than mistranslated.
        assert!(
            s.contains("<blockquote style=\"margin-top: 3px\">q</blockquote>"),
            "blockquote height only: {s}"
        );
        assert!(
            s.contains("<img src=\"i.jpg\" width=\"100\" height=\"50\"/>"),
            "img width/height untouched: {s}"
        );
        assert!(
            s.contains("<p style=\"color: red; margin-top: 1em\">styled</p>"),
            "merged into existing style: {s}"
        );

        // img align: left/right float, baseline/middle vertical-align;
        // width/height stay (valid HTML on img).
        let imgs = b"<img src=\"a.jpg\" align=\"baseline\" width=\"333\" height=\"500\">\
<img src=\"b.jpg\" align=\"left\"/>";
        let out = convert_legacy_block_attrs(imgs);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains(
                "<img src=\"a.jpg\" width=\"333\" height=\"500\" style=\"vertical-align: baseline\">"
            ),
            "img baseline align: {s}"
        );
        assert!(
            s.contains("<img src=\"b.jpg\" style=\"float: left\"/>"),
            "img left float: {s}"
        );
    }

    #[test]
    fn test_rewrite_kindle_embed_in_css() {
        // Font and image refs rewrite to stylesheet-relative asset paths
        // (embed ids are 1-based); an unmapped ref stays verbatim so the
        // strip pass can drop its rule.
        let css = b"@font-face { font-family:\"A\"; src:url(kindle:embed:0005?mime=application/x-font-ttf); }\n\
.bg { background: url('kindle:embed:0002'); }\n\
@font-face { font-family:\"B\"; src:url(kindle:embed:000A); }\n";
        let paths: std::collections::HashMap<usize, String> = [
            (4usize, "fonts/font_0004.ttf".to_string()),
            (1usize, "images/image_0001.jpg".to_string()),
        ]
        .into();
        let out = rewrite_kindle_embed_in_css(css, &paths);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("src:url(../fonts/font_0004.ttf)"),
            "font ref rewritten, mime query dropped: {s}"
        );
        assert!(
            s.contains("url('../images/image_0001.jpg')"),
            "image ref rewritten: {s}"
        );
        assert!(
            s.contains("kindle:embed:000A"),
            "unmapped ref left for the strip pass: {s}"
        );
        // The strip pass then drops only the dangling rule.
        let stripped = strip_kindle_embed_font_faces(&out);
        let s2 = String::from_utf8_lossy(&stripped);
        assert!(s2.contains("font-family:\"A\""), "resolved font kept: {s2}");
        assert!(
            !s2.contains("font-family:\"B\""),
            "dangling font dropped: {s2}"
        );
    }

    #[test]
    fn embed_ref_extension_comes_from_the_extracted_asset_not_the_mime_query() {
        // A real Amazon 合本版 declares `mime=image/png` on a record whose
        // bytes are JPEG; naming the href from the query left it pointing at
        // an `images/image_0200.png` the exporter never wrote (RSC-007).
        // `0069` is base32 201 — the 1-based id of resource index 200.
        let html = b"<image xlink:href=\"kindle:embed:0069?mime=image/png\"/>\
<img src=\"kindle:embed:0002?mime=image/jpeg\"/>";
        let paths: HashMap<usize, String> =
            [(200usize, "images/image_0200.jpg".to_string())].into();
        let out = transform_kindle_refs(html, &[], b"", &[], &paths);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("xlink:href=\"images/image_0200.jpg\""),
            "extracted asset wins over the mime query: {s}"
        );
        // An index with no extracted asset keeps the mime-derived name — the
        // reference is unresolvable either way, and this is what the older
        // books rely on.
        assert!(
            s.contains("src=\"images/image_0001.jpg\""),
            "unmapped index falls back to the mime query: {s}"
        );
    }

    #[test]
    fn test_unlink_image_anchors() {
        // The in-book TOC "Cover" row linking the raw cover JPEG loses its
        // href (RSC-010); content-document links are untouched.
        let html = b"<p><a href=\"images/image_0002.jpg\" >Cover</a></p>\
<p><a href=\"part0003.html#d1\">Part One</a></p><img src=\"images/image_0001.jpg\"/>";
        let out = unlink_image_anchors(html);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("<a href=\"images/"), "image href dropped: {s}");
        assert!(s.contains(">Cover</a>"), "label survives: {s}");
        assert!(
            s.contains("<a href=\"part0003.html#d1\">"),
            "doc link kept: {s}"
        );
        assert!(
            s.contains("<img src=\"images/image_0001.jpg\"/>"),
            "img untouched: {s}"
        );
    }

    #[test]
    fn test_collect_linked_aids() {
        // One skeleton file (file 0 starting at 0). The body has two aid
        // elements; a pos:fid link (elem 1, off 0) targets the second one's
        // on-disk byte position.
        let html = b"<body ><a href=\"kindle:pos:fid:0001:off:0000000000\">go</a>\
<p aid=\"AA\">first</p><p aid=\"BB\">second</p></body>";
        let first_p = memmem::find(html, b"<p aid=\"AA\"").unwrap() as u32;
        let second_p = memmem::find(html, b"<p aid=\"BB\"").unwrap() as u32;
        // `reassembled_pos` is where the chunk's content lands in the
        // reassembled flow; here the body is a single flat block (no skeleton/
        // chunk split), so it equals the byte position. An `off` of 0 lands on
        // the opening tag.
        let elem = |reassembled_pos: u32| DivElement {
            insert_pos: reassembled_pos,
            toc_text: None,
            file_number: 0,
            sequence_number: 0,
            start_pos: 0,
            length: 0,
            reassembled_pos,
        };
        let elems = vec![elem(first_p), elem(second_p)];
        let file_starts = [(0u32, 0u32)];
        let linked = collect_linked_aids(html, html, &elems, &file_starts, &[]);
        assert!(
            linked.contains("BB"),
            "pos:fid:0001 resolves to the BB element, got {linked:?}"
        );
        assert!(
            !linked.contains("AA"),
            "AA is not a link target, got {linked:?}"
        );

        // NCX positions count as link sources too: a position on the first
        // <p> marks AA linked.
        let linked = collect_linked_aids(
            html,
            html,
            &elems,
            &file_starts,
            &[(first_p as usize, 0usize)],
        );
        assert!(
            linked.contains("AA"),
            "NCX position at the first <p> must mark AA linked, got {linked:?}"
        );
    }

    #[test]
    fn test_strip_root_escaping_links() {
        // The exact dangling Aozora horizontal-alternate sheet: dropped.
        let head = concat!(
            "<link rel=\"stylesheet\" href=\"styles/style0000.css\" type=\"text/css\"/>\n",
            "<link class=\"vertical\" rel=\"stylesheet\" href=\"styles/style0002.css\" title=\"縦組\"/>\n",
            "<link class=\"horizontal\" rel=\"alternate stylesheet\" href=\"../styles/a00301_h.css\" title=\"横組\"/>\n",
            "<title>人間失格</title>",
        );
        let out = strip_root_escaping_links(head.as_bytes());
        let s = String::from_utf8_lossy(&out);
        assert!(
            !s.contains("a00301_h.css"),
            "escaping <link> must be dropped"
        );
        assert!(!s.contains(".."), "no `..` href should survive");
        // Valid sibling-relative stylesheet links are untouched.
        assert!(s.contains("styles/style0000.css"));
        assert!(s.contains("styles/style0002.css"));
        assert!(s.contains("<title>人間失格</title>"));
    }

    #[test]
    fn test_strip_root_escaping_links_preserves_other_elements() {
        // `..` in a non-<link> element (a body anchor / an id) is left alone —
        // we only neutralize stylesheet <link>s, not content or anchor targets.
        let html = b"<a href=\"../other.html\">x</a><h4 id=\"a00301_0007_n0004\">\xe4\xb8\x80</h4>";
        let out = strip_root_escaping_links(html);
        assert_eq!(out, html, "non-<link> elements must pass through verbatim");

        // A <link> whose href is clean is kept.
        let ok = b"<link rel=\"stylesheet\" href=\"styles/style0001.css\"/>";
        assert_eq!(strip_root_escaping_links(ok), ok);

        // `<linkfoo>` is not a <link> element.
        let notlink = b"<linkfoo href=\"../x\"/>";
        assert_eq!(strip_root_escaping_links(notlink), notlink);
    }

    #[test]
    fn test_img_alt() {
        // Self-closing: alt must go before the `/>`, not between `/` and `>`
        // (which Apple Books rejects as "attributes construct error").
        let output = ensure_img_alt(b"<img src=\"test.jpg\"/>");
        let s = String::from_utf8_lossy(&output);
        assert_eq!(s, r#"<img src="test.jpg" alt=""/>"#);

        // Non-self-closing: alt before the `>`.
        let output = ensure_img_alt(b"<img src=\"test.jpg\">");
        let s = String::from_utf8_lossy(&output);
        assert_eq!(s, r#"<img src="test.jpg" alt="">"#);

        // Already has alt — return unchanged.
        let output = ensure_img_alt(b"<img src=\"test.jpg\" alt=\"x\"/>");
        let s = String::from_utf8_lossy(&output);
        assert_eq!(s, r#"<img src="test.jpg" alt="x"/>"#);
    }
}
