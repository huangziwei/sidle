//! What kind of page a content document is, judged from its markup alone.
//!
//! A page of pictures and no text is a structural marker in real books: it is
//! how a cover is authored, and how each volume of a 合本版 or a boxed set
//! announces itself. Two questions are asked of that shape, and they are not the
//! same question, so they are two predicates here rather than one shared by
//! callers who mean different things:
//!
//! - **Where does a volume begin?** — [`image_only_source`]. Any page of
//!   pictures qualifies, however many it holds: a publisher is free to run a
//!   volume's cover, frontispiece and title plate together on one page, and it
//!   is still the page the volume opens on.
//! - **Is this document nothing but the cover?** — [`single_image_source`],
//!   which the EPUB exporter uses to drop a source cover page it is about to
//!   emit a second time. One image, because a page carrying a title plate as
//!   well is not a duplicate of anything and dropping it would lose the plate.

/// The source of the first image on a page of pictures — a document whose body
/// holds at least one image and renders no text of its own — or `None` when the
/// document is anything else.
///
/// Images count in both shapes, raster (`<img src>`) and SVG-wrapped
/// (`<image href>` / `xlink:href`), and the first in document order is the one
/// that names the page: on a volume's opening page that is its cover, with the
/// title plate behind it. Text is judged on the body only, so a
/// `<head><title>` never counts against a page.
pub(crate) fn image_only_source(html: &str) -> Option<&str> {
    let body = body_of(html);
    renders_no_text(body).then(|| first_image_source(body))?
}

/// The image source of a single-image, text-free document, or `None` when the
/// document is anything else — [`image_only_source`] held to exactly one image.
pub(crate) fn single_image_source(html: &str) -> Option<&str> {
    let body = body_of(html);
    // `<img` is not a prefix of `<image`, so the two counts never overlap.
    if body.matches("<img").count() + body.matches("<image").count() != 1 {
        return None;
    }
    image_only_source(html)
}

/// Whether a document is a page of pictures — see [`image_only_source`].
pub(crate) fn is_image_only_page(html: &str) -> bool {
    image_only_source(html).is_some()
}

/// The document's body, or the whole of it when it declares none.
fn body_of(html: &str) -> &str {
    match (html.find("<body"), html.rfind("</body>")) {
        (Some(s), Some(e)) if e > s => {
            let open_end = html[s..e].find('>').map(|i| s + i + 1).unwrap_or(s);
            &html[open_end..e]
        }
        _ => html,
    }
}

/// The source of the first image in `body`, in document order.
fn first_image_source(body: &str) -> Option<&str> {
    // Searching bare `href="` also finds the `xlink:href="` an SVG wrapper uses.
    let at = |tag: &str, attr| body.find(tag).map(|i| (i, attr));
    let (from, attr) = match (at("<img", "src=\""), at("<image", "href=\"")) {
        (Some(raster), Some(vector)) => std::cmp::min_by_key(raster, vector, |&(i, _)| i),
        (found, None) | (None, found) => found?,
    };
    body[from..]
        .split_once(attr)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(src, _)| src)
}

/// Whether a body renders no text of its own: with every tag stripped, nothing
/// but whitespace remains.
fn renders_no_text(body: &str) -> bool {
    let mut depth = 0i32;
    for c in body.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = (depth - 1).max(0),
            c if depth == 0 && !c.is_whitespace() => return false,
            _ => {}
        }
    }
    true
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
        assert_eq!(image_only_source(raster), Some("images/cover.jpeg"));
        assert_eq!(image_only_source(vector), Some("images/cover.jpeg"));
        // The head title is not body text.
        let titled =
            "<html><head><title>Cover</title></head><body><img src=\"cover.jpeg\"/></body></html>";
        assert!(is_image_only_page(titled));
    }

    /// A publisher is free to run a volume's cover and its title plate together
    /// on one page. That is still a page of pictures, and the cover is the
    /// picture it opens with — but it is not a document that holds nothing but
    /// the cover, so the exporter must not read it as a duplicate of one.
    #[test]
    fn a_cover_run_together_with_a_title_plate_is_still_a_page_of_pictures() {
        let pair = "<html><body><div><img src=\"cover.jpg\"/></div>\
                    <div><img src=\"title.jpg\"/></div></body></html>";
        assert_eq!(image_only_source(pair), Some("cover.jpg"));
        assert_eq!(single_image_source(pair), None);
        // Whichever shape comes first names the page.
        let svg_first = "<html><body><svg><image href=\"plate.jpg\"/></svg>\
                         <img src=\"title.jpg\"/></body></html>";
        assert_eq!(image_only_source(svg_first), Some("plate.jpg"));
    }

    #[test]
    fn text_or_no_image_at_all_disqualifies_a_page() {
        let with_text = "<html><body><img src=\"cover.jpeg\"/><p>Chapter One</p></body></html>";
        let none = "<html><body><p>Chapter One</p></body></html>";
        let empty = "<html><body></body></html>";
        for html in [with_text, none, empty] {
            assert!(!is_image_only_page(html));
            assert_eq!(single_image_source(html), None);
        }
    }
}
