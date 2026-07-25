//! What kind of page a content document is, judged from its markup alone.
//!
//! A full-bleed image page — one `<img>`/`<image>` and no visible text — is a
//! structural marker in real books: it is how a cover is authored, and how each
//! volume of a 合本版 announces itself. Two callers need the same judgement from
//! opposite directions (the EPUB exporter deciding which document *is* the
//! cover, the TOC repair deciding where a volume starts), so the predicate lives
//! here rather than in either of them.

/// The image source of a single-image, text-free document, or `None` when the
/// document is anything else.
///
/// "Single image" counts both raster (`<img src>`) and SVG-wrapped
/// (`<image href>` / `xlink:href`) shapes; exactly one must be present.
/// "Text-free" is judged on the body only, so a `<head><title>` never counts
/// against it.
pub(crate) fn single_image_source(html: &str) -> Option<&str> {
    let body = match (html.find("<body"), html.rfind("</body>")) {
        (Some(s), Some(e)) if e > s => {
            let open_end = html[s..e].find('>').map(|i| s + i + 1).unwrap_or(s);
            &html[open_end..e]
        }
        _ => html,
    };
    // Exactly one image, counting both shapes. `<img` is not a prefix of
    // `<image`, so the two counts never overlap.
    let raster = body.matches("<img").count();
    let vector = body.matches("<image").count();
    if raster + vector != 1 {
        return None;
    }
    // Searching bare `href="` also finds the `xlink:href="` an SVG wrapper uses.
    let (tag, attr) = if raster == 1 {
        ("<img", "src=\"")
    } else {
        ("<image", "href=\"")
    };
    let src = body
        .split_once(tag)
        .and_then(|(_, rest)| rest.split_once(attr))
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(src, _)| src)?;
    // No visible text: with all tags stripped, nothing but whitespace remains.
    let mut depth = 0i32;
    for c in body.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = (depth - 1).max(0),
            c if depth == 0 && !c.is_whitespace() => return None,
            _ => {}
        }
    }
    Some(src)
}

/// Whether a document is a full-bleed image page — see [`single_image_source`].
pub(crate) fn is_single_image_page(html: &str) -> bool {
    single_image_source(html).is_some()
}

/// The last path segment of a resource reference, for comparing hrefs that are
/// written relative to different directories.
pub(crate) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_image_with_no_body_text_is_an_image_page() {
        let raster = "<html><body><div><img src=\"images/cover.jpeg\"/></div></body></html>";
        let vector =
            "<html><body><svg><image xlink:href=\"images/cover.jpeg\"/></svg></body></html>";
        assert_eq!(single_image_source(raster), Some("images/cover.jpeg"));
        assert_eq!(single_image_source(vector), Some("images/cover.jpeg"));
        // The head title is not body text.
        let titled =
            "<html><head><title>Cover</title></head><body><img src=\"cover.jpeg\"/></body></html>";
        assert!(is_single_image_page(titled));
    }

    #[test]
    fn text_or_a_second_image_disqualifies_a_page() {
        let with_text = "<html><body><img src=\"cover.jpeg\"/><p>Chapter One</p></body></html>";
        let two = "<html><body><img src=\"a.jpg\"/><img src=\"b.jpg\"/></body></html>";
        let none = "<html><body><p>Chapter One</p></body></html>";
        assert!(!is_single_image_page(with_text));
        assert!(!is_single_image_page(two));
        assert!(!is_single_image_page(none));
    }
}
