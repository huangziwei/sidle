//! Filepos handling for MOBI format.

use std::collections::{BTreeMap, HashMap, HashSet};

/// Collect all filepos target values from `<a filepos=NNNNN>` attributes.
///
/// Returns a set of byte positions that are referenced as link targets.
pub fn collect_filepos_targets(html: &[u8]) -> HashSet<usize> {
    let mut targets = HashSet::new();
    let mut pos = 0;

    while pos < html.len() {
        // Look for filepos= pattern (may or may not have quotes)
        if pos + 8 < html.len() && html[pos..].starts_with(b"filepos=") {
            let val_start = pos + 8;
            let mut start = val_start;

            // Skip optional quote
            if start < html.len() && (html[start] == b'"' || html[start] == b'\'') {
                start += 1;
            }

            // Skip leading zeros
            while start < html.len() && html[start] == b'0' {
                start += 1;
            }

            // Parse digits
            let mut val_end = start;
            while val_end < html.len() && html[val_end].is_ascii_digit() {
                val_end += 1;
            }

            // If we only had zeros, back up to include at least one
            if val_end == start && start > val_start && html[start - 1] == b'0' {
                start -= 1;
            }

            if val_end > start {
                if let Ok(filepos) = String::from_utf8_lossy(&html[start..val_end]).parse::<usize>()
                {
                    targets.insert(filepos);
                }
            } else if val_end == start {
                // Just "0" or empty after zeros
                targets.insert(0);
            }
            pos = val_end;
        } else {
            pos += 1;
        }
    }

    targets
}

/// Transform MOBI HTML matching KindleUnpack's approach:
pub fn transform_mobi_html(
    html: &[u8],
    assets: &[std::path::PathBuf],
    extra_anchor_positions: &[u32],
) -> Vec<u8> {
    use std::collections::HashMap;

    // Step 1: Collect all filepos targets
    let targets = collect_filepos_targets(html);

    // Step 2: Build position map for anchor insertion
    let mut position_map: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for &position in &targets {
        if position > 0 && position <= html.len() {
            let anchor = format!("<a id=\"filepos{}\" />", position);
            position_map
                .entry(position)
                .or_default()
                .extend_from_slice(anchor.as_bytes());
        }
    }

    // Also insert anchors at extra positions (NCX entries, etc.)
    for &position in extra_anchor_positions {
        let pos = position as usize;
        if pos > 0 && pos <= html.len() {
            position_map
                .entry(pos)
                .or_insert_with(|| format!("<a id=\"filepos{}\" />", pos).into_bytes());
        }
    }

    // Step 3: Build recindex -> asset path mapping.
    let mut recindex_map: HashMap<u32, String> = HashMap::new();
    for asset in assets {
        if let Some(offset) = super::asset_record_offset(asset) {
            recindex_map.insert(offset + 1, asset.to_string_lossy().to_string());
        }
    }

    // Step 4: Insert anchors at positions (like KindleUnpack's dataList building)
    let mut with_anchors = Vec::with_capacity(html.len() + position_map.len() * 30);
    let mut last_pos = 0;

    for (&end_pos, anchor_bytes) in &position_map {
        if end_pos == 0 || end_pos > html.len() {
            continue;
        }
        with_anchors.extend_from_slice(&html[last_pos..end_pos]);
        with_anchors.extend_from_slice(anchor_bytes);
        last_pos = end_pos;
    }
    with_anchors.extend_from_slice(&html[last_pos..]);

    // Step 5: Convert filepos=NNNNN to href="#fileposNNNNN" and handle recindex
    let mut output = Vec::with_capacity(with_anchors.len());
    let mut pos = 0;

    while pos < with_anchors.len() {
        // Look for filepos= pattern
        if pos + 8 < with_anchors.len() && with_anchors[pos..].starts_with(b"filepos=") {
            let val_start = pos + 8;
            let mut start = val_start;
            let mut has_quote = false;

            // Skip optional quote
            if start < with_anchors.len()
                && (with_anchors[start] == b'"' || with_anchors[start] == b'\'')
            {
                has_quote = true;
                start += 1;
            }

            // Parse digits (including leading zeros which we strip in output)
            let digit_start = start;
            while start < with_anchors.len() && with_anchors[start].is_ascii_digit() {
                start += 1;
            }

            // Skip closing quote if present
            let mut end = start;
            if has_quote
                && end < with_anchors.len()
                && (with_anchors[end] == b'"' || with_anchors[end] == b'\'')
            {
                end += 1;
            }

            if start > digit_start {
                // Parse the number, stripping leading zeros
                let num_str = String::from_utf8_lossy(&with_anchors[digit_start..start]);
                if let Ok(filepos_num) = num_str.trim_start_matches('0').parse::<u64>() {
                    output.extend_from_slice(b"href=\"#filepos");
                    output.extend_from_slice(filepos_num.to_string().as_bytes());
                    output.push(b'"');
                    pos = end;
                    continue;
                } else if num_str.chars().all(|c| c == '0') {
                    // All zeros = position 0
                    output.extend_from_slice(b"href=\"#filepos0\"");
                    pos = end;
                    continue;
                }
            } else {
                // Empty or malformed filepos (no digits) - skip the entire attribute
                pos = end;
                continue;
            }
        }

        // Look for recindex=" pattern
        if pos + 10 < with_anchors.len() && with_anchors[pos..].starts_with(b"recindex=\"") {
            let val_start = pos + 10;
            if let Some(val_end_rel) = with_anchors[val_start..].iter().position(|&b| b == b'"') {
                let val_end = val_start + val_end_rel;
                let recindex = std::str::from_utf8(&with_anchors[val_start..val_end])
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok());

                if let Some(path) = recindex.and_then(|n| recindex_map.get(&n)) {
                    output.extend_from_slice(b"src=\"");
                    output.extend_from_slice(path.as_bytes());
                    output.push(b'"');
                    pos = val_end + 1;
                    continue;
                }
            }
        }

        // An `<img>` may carry any of `lowrecindex` / `recindex` /
        if with_anchors[pos] == b'<'
            && with_anchors[pos + 1..].starts_with(b"img")
            && with_anchors
                .get(pos + 4)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
            && let Some(end_rel) = memchr::memchr(b'>', &with_anchors[pos..])
        {
            let end = pos + end_rel + 1;
            output.extend_from_slice(&resolve_image_record(
                &with_anchors[pos..end],
                &recindex_map,
            ));
            pos = end;
            continue;
        }

        // Copy byte as-is
        output.push(with_anchors[pos]);
        pos += 1;
    }

    // Step 6: Remove empty anchors (like KindleUnpack does)
    remove_empty_anchors(&mut output);

    // Step 7: Canonicalize element names and escape bare ampersands. MOBI6
    // markup is case-free HTML4 with unescaped `&` in running text; every
    // consumer downstream of here treats the result as XHTML.
    let output = super::transform::lowercase_tag_names(&output);
    let output = super::transform::escape_bare_ampersands(&output);

    // Step 8: Legacy layout attributes (`<p height width align>`) → inline
    // CSS, so the spacing/indent/justification kindlegen encoded as
    // attributes survives into valid XHTML5.
    let output = super::transform::convert_legacy_block_attrs(&output);

    // Step 9: The same treatment for whole elements HTML5 no longer defines
    // (`<font>`, `<center>`) and MOBI's periodical-only ones (`<block>`,
    // `<articlename>`, `<contributor>`), which were never HTML at all.
    super::transform::convert_obsolete_elements(&output)
}

/// Rewrite one `<img>` tag, turning its record references into a single `src`.
fn resolve_image_record(tag: &[u8], recindex_map: &HashMap<u32, String>) -> Vec<u8> {
    const NAME_END: usize = 4; // `<img`
    let mut attrs_end = tag.len() - 1;
    let self_closing = attrs_end > NAME_END && tag[attrs_end - 1] == b'/';
    if self_closing {
        attrs_end -= 1;
    }
    let attrs = &tag[NAME_END..attrs_end];

    let mut src: Option<&String> = None;
    let mut drop_spans: Vec<(usize, usize)> = Vec::new();
    for attr in [&b"lowrecindex"[..], b"recindex", b"hirecindex"] {
        for (s, e) in super::transform::attr_spans(attrs, attr) {
            if let Some(v) = super::transform::extract_attr_value(&attrs[s..e], attr)
                && let Ok(n) = String::from_utf8_lossy(v).trim().parse::<u32>()
                && let Some(path) = recindex_map.get(&n)
            {
                src = Some(path);
            }
            drop_spans.push((s, e));
        }
    }
    if drop_spans.is_empty() {
        return tag.to_vec();
    }

    drop_spans.sort_unstable();
    let mut out = Vec::with_capacity(tag.len() + 32);
    out.extend_from_slice(&tag[..NAME_END]);
    let mut cursor = 0;
    for (s, e) in &drop_spans {
        let mut s = *s;
        while s > cursor && attrs[s - 1].is_ascii_whitespace() {
            s -= 1;
        }
        out.extend_from_slice(&attrs[cursor..s]);
        cursor = *e;
    }
    out.extend_from_slice(&attrs[cursor..]);
    if let Some(path) = src {
        out.extend_from_slice(b" src=\"");
        out.extend_from_slice(path.as_bytes());
        out.push(b'"');
    }
    if self_closing {
        out.push(b'/');
    }
    out.push(b'>');
    out
}

/// Remove empty anchor tags: `<a />` and `<a></a>`
fn remove_empty_anchors(html: &mut Vec<u8>) {
    // This is a simple implementation - could be optimized
    let html_str = String::from_utf8_lossy(html);

    // Remove <a /> and <a  /> patterns
    let cleaned = html_str
        .replace("<a />", "")
        .replace("<a  />", "")
        .replace("<a></a>", "")
        .replace("<a ></a>", "");

    html.clear();
    html.extend_from_slice(cleaned.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_collect_filepos_targets() {
        let html = b"<a filepos=1234>Link1</a> text <a filepos=5678>Link2</a>";
        let targets = collect_filepos_targets(html);

        assert!(targets.contains(&1234));
        assert!(targets.contains(&5678));
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_collect_filepos_with_quotes() {
        let html = b"<a filepos=\"0001234\">Link</a>";
        let targets = collect_filepos_targets(html);

        assert!(targets.contains(&1234));
    }

    #[test]
    fn test_transform_inserts_anchor_at_position() {
        // Position 50 should have an anchor inserted
        let mut html = vec![b' '; 100];
        html[0..6].copy_from_slice(b"<html>");
        html[50..60].copy_from_slice(b"<p>Hello</");
        // Add a link pointing to position 50
        let link = b"<a filepos=50>Link</a>";
        html.extend_from_slice(link);

        let result = transform_mobi_html(&html, &[], &[]);
        let result_str = String::from_utf8_lossy(&result);

        // Should have anchor at position 50
        assert!(
            result_str.contains("<a id=\"filepos50\" />"),
            "Should insert anchor: {}",
            result_str
        );
        // Should convert filepos to href
        assert!(
            result_str.contains("href=\"#filepos50\""),
            "Should convert href: {}",
            result_str
        );
    }

    #[test]
    fn test_transform_filepos_to_href() {
        let html = b"<a filepos=1234>Link</a>";
        let result = transform_mobi_html(html, &[], &[]);
        let result_str = String::from_utf8_lossy(&result);

        assert!(result_str.contains("href=\"#filepos1234\""));
        assert!(!result_str.contains("filepos="));
    }

    #[test]
    fn test_transform_recindex() {
        let assets = vec![PathBuf::from("images/image_0000.jpg")];
        let html = b"<img recindex=\"00001\">";
        let result = transform_mobi_html(html, &assets, &[]);
        let result_str = String::from_utf8_lossy(&result);

        assert!(result_str.contains("src=\"images/image_0000.jpg\""));
        assert!(!result_str.contains("recindex"));
    }

    #[test]
    fn recindex_counts_the_records_asset_discovery_skipped() {
        // A `RESC` at the head of the resource run takes offset 0, so the first
        // image is `image_0001` and its `recindex` is 2. Keying on the asset's
        // position in the list instead resolves to the next image along.
        let assets = vec![
            PathBuf::from("images/image_0001.jpg"),
            PathBuf::from("images/image_0002.jpg"),
        ];
        let result = transform_mobi_html(b"<img recindex=\"00002\">", &assets, &[]);
        let result_str = String::from_utf8_lossy(&result);

        assert!(result_str.contains("src=\"images/image_0001.jpg\""));
        assert!(!result_str.contains("image_0002"));
    }

    #[test]
    fn an_img_collapses_its_three_record_references_into_one_src() {
        // Matching `recindex="` as a substring also fires on the tail of
        // `hirecindex="`, which leaves a bogus `hisrc` beside the real `src`.
        let assets = vec![
            PathBuf::from("images/image_0000.jpg"),
            PathBuf::from("images/image_0001.jpg"),
        ];
        let result = transform_mobi_html(
            b"<img hirecindex=\"00002\" recindex=\"00001\">",
            &assets,
            &[],
        );
        let s = String::from_utf8_lossy(&result);

        assert!(
            !s.contains("hisrc"),
            "no attribute made of a partial match: {s}"
        );
        assert_eq!(s.matches("src=").count(), 1, "exactly one src: {s}");
        // Highest resolution available wins, whatever order they were written.
        assert!(s.contains("src=\"images/image_0001.jpg\""), "{s}");
    }

    #[test]
    fn an_img_with_no_resolvable_record_is_left_alone() {
        let result = transform_mobi_html(b"<img src=\"kept.jpg\" alt=\"a\">", &[], &[]);
        assert_eq!(
            String::from_utf8_lossy(&result),
            "<img src=\"kept.jpg\" alt=\"a\">"
        );
    }

    #[test]
    fn an_unpadded_recindex_still_resolves() {
        let assets = vec![PathBuf::from("images/image_0000.png")];
        let result = transform_mobi_html(b"<img recindex=\"1\">", &assets, &[]);

        assert!(String::from_utf8_lossy(&result).contains("src=\"images/image_0000.png\""));
    }

    #[test]
    fn test_transform_with_leading_zeros() {
        let html = b"<a filepos=0000100>Link</a>";
        let result = transform_mobi_html(html, &[], &[]);
        let result_str = String::from_utf8_lossy(&result);

        // Should strip leading zeros in href
        assert!(result_str.contains("href=\"#filepos100\""));
    }

    #[test]
    fn test_transform_empty_filepos_quoted() {
        // Empty filepos with quotes should be removed, leaving plain anchor
        let html = b"<a filepos=\"\">Link text</a>";
        let result = transform_mobi_html(html, &[], &[]);
        let result_str = String::from_utf8_lossy(&result);

        // The empty filepos="" attribute should be stripped
        assert!(
            !result_str.contains("filepos"),
            "Empty filepos should be removed: {}",
            result_str
        );
        // The link text should remain
        assert!(
            result_str.contains("Link text"),
            "Link text should remain: {}",
            result_str
        );
    }

    #[test]
    fn test_transform_empty_filepos_unquoted() {
        // Empty filepos without quotes (malformed) should be handled
        let html = b"<a filepos=>Link text</a>";
        let result = transform_mobi_html(html, &[], &[]);
        let result_str = String::from_utf8_lossy(&result);

        // The empty filepos= attribute should be stripped
        assert!(
            !result_str.contains("filepos"),
            "Empty filepos should be removed: {}",
            result_str
        );
        // The link text should remain
        assert!(
            result_str.contains("Link text"),
            "Link text should remain: {}",
            result_str
        );
    }

    #[test]
    fn test_transform_whitespace_only_filepos() {
        // filepos with only whitespace should be handled
        let html = b"<a filepos=\"  \">Link text</a>";
        let result = transform_mobi_html(html, &[], &[]);
        let result_str = String::from_utf8_lossy(&result);

        // The whitespace-only filepos should be stripped
        assert!(
            !result_str.contains("filepos"),
            "Whitespace-only filepos should be removed: {}",
            result_str
        );
    }
}
