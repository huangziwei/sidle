//! What kind of page a content document is, judged from its markup alone.

/// The plate a page opens on — the source of its first image, when the body
/// renders no text in front of it — or `None` for a page that opens on text.
pub(crate) fn opening_plate_source(html: &str) -> Option<&str> {
    let body = body_of(html);
    let (at, src) = first_image(body)?;
    renders_no_text(&body[..at]).then_some(src)
}

/// The image source of a single-image, text-free document, or `None` when the
/// document is anything else — [`is_image_only_page`] held to exactly one image.
pub(crate) fn single_image_source(html: &str) -> Option<&str> {
    let body = body_of(html);
    // `<img` is not a prefix of `<image`, so the two counts never overlap.
    if body.matches("<img").count() + body.matches("<image").count() != 1 {
        return None;
    }
    let (_, src) = first_image(body)?;
    renders_no_text(body).then_some(src)
}

/// Whether a document is a page of pictures and nothing else: at least one
/// image, and no text anywhere in the body.
pub(crate) fn is_image_only_page(html: &str) -> bool {
    let body = body_of(html);
    first_image(body).is_some() && renders_no_text(body)
}

/// The copyright sign, and the references markup writes it with instead. The
/// last two are one reference spelled two ways; both are in the wild.
const COPYRIGHT_SIGN: &str = "\u{a9}";
const COPYRIGHT_REFERENCES: [&str; 4] = ["&copy;", "&#169;", "&#xa9;", "&#x00a9;"];

/// How far from the sign the year may sit and still be part of the same notice.
/// Wide enough for the rights holder's name to come between them — `© Tom
/// Doherty Associates, LLC 2001` — and no wider.
const NOTICE_SPAN: usize = 48;

/// Whether a page carries a work's own rights statement.
pub(crate) fn states_own_rights(html: &str) -> bool {
    // References are folded back into the sign first, so the scan below has one
    // thing to look for however the document happened to spell it.
    let mut body = body_of(html).to_ascii_lowercase();
    for reference in COPYRIGHT_REFERENCES {
        if body.contains(reference) {
            body = body.replace(reference, COPYRIGHT_SIGN);
        }
    }
    body.match_indices(COPYRIGHT_SIGN).any(|(at, sign)| {
        let after = at + sign.len();
        states_a_year(&body[after..window_end(&body, after)])
    })
}

/// The end of a [`NOTICE_SPAN`]-long window from `at`, on a character boundary.
fn window_end(text: &str, at: usize) -> usize {
    let mut end = (at + NOTICE_SPAN).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Whether `text` holds a four-digit run that reads as a year — exactly four, so
/// that the digits of an ISBN or a street address are not mistaken for one.
fn states_a_year(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_digit())
        .any(|run| run.len() == 4 && matches!(run.as_bytes()[0], b'1' | b'2'))
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

/// The first image in `body` in document order, as `(where its tag opens, its
/// source)` — the offset is what tells a plate a page opens on from one set
/// partway down it.
fn first_image(body: &str) -> Option<(usize, &str)> {
    // Searching bare `href="` also finds the `xlink:href="` an SVG wrapper uses.
    let at = |tag: &str, attr| body.find(tag).map(|i| (i, attr));
    let (from, attr) = match (at("<img", "src=\""), at("<image", "href=\"")) {
        (Some(raster), Some(vector)) => std::cmp::min_by_key(raster, vector, |&(i, _)| i),
        (found, None) | (None, found) => found?,
    };
    body[from..]
        .split_once(attr)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(src, _)| (from, src))
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
        assert_eq!(opening_plate_source(raster), Some("images/cover.jpeg"));
        assert_eq!(opening_plate_source(vector), Some("images/cover.jpeg"));
        // The head title is not body text.
        let titled =
            "<html><head><title>Cover</title></head><body><img src=\"cover.jpeg\"/></body></html>";
        assert!(is_image_only_page(titled));
    }

    /// A publisher is free to run a volume's cover and its title plate together
    #[test]
    fn a_cover_run_together_with_a_title_plate_is_still_a_page_of_pictures() {
        let pair = "<html><body><div><img src=\"cover.jpg\"/></div>\
                    <div><img src=\"title.jpg\"/></div></body></html>";
        assert_eq!(opening_plate_source(pair), Some("cover.jpg"));
        assert!(is_image_only_page(pair));
        assert_eq!(single_image_source(pair), None);
        // Whichever shape comes first names the page.
        let svg_first = "<html><body><svg><image href=\"plate.jpg\"/></svg>\
                         <img src=\"title.jpg\"/></body></html>";
        assert_eq!(opening_plate_source(svg_first), Some("plate.jpg"));
    }

    /// The shape a Western collection sets a volume's opening on: the cover, the
    /// title plate, and then the work's own copyright notice, all one document.
    #[test]
    fn a_plate_run_into_the_works_own_front_matter_still_opens_a_volume() {
        let html = "<html><body><figure><img src=\"Woolcover.jpg\"/></figure>\
                    <figure><img src=\"Wooltitle.jpg\"/></figure>\
                    <div><p>Copyright © 2012 by Hugh Howey</p></div></body></html>";
        assert_eq!(opening_plate_source(html), Some("Woolcover.jpg"));
        assert!(!is_image_only_page(html));
        assert_eq!(single_image_source(html), None);
    }

    /// The notice is read by its own form — sign beside year — however the
    /// document spelled the sign, and in whatever language the words around it
    /// are set.
    #[test]
    fn a_copyright_notice_is_read_by_the_sign_beside_a_year() {
        for sign in ["©", "&copy;", "&#169;", "&#xA9;", "&#x00a9;"] {
            let html = format!(
                "<html><body><p>Copyright {sign} 2012 by Hugh Howey</p>\
                                <p>All rights reserved</p></body></html>"
            );
            assert!(states_own_rights(&html), "{sign} spells the sign");
        }
        // The rights holder may come between the sign and the year.
        assert!(states_own_rights(
            "<body><p>&#169; Tom Doherty Associates, LLC 2001</p></body>"
        ));
        // Neither the words nor the script they are set in is what is read.
        assert!(states_own_rights(
            "<body><p>© 2018 鴨志田一／KADOKAWA　無断複製を禁じます</p></body>"
        ));
    }

    /// A sign with no year beside it credits a photograph; a year with no sign
    /// is a date. Neither says who published anything.
    #[test]
    fn a_credit_line_is_not_a_rights_notice() {
        assert!(!states_own_rights(
            "<body><figcaption>© Christopher Michel</figcaption></body>"
        ));
        assert!(!states_own_rights(
            "<body><p>First published in 1999 by Bantam Books</p></body>"
        ));
        // Nor is a year far enough past the sign to belong to another sentence.
        assert!(!states_own_rights(
            "<body><p>© the author. This edition was set in Bembo and printed \
             in Great Britain in 1999.</p></body>"
        ));
        // A run of digits that is not four long is not a year.
        assert!(!states_own_rights(
            "<body><p>© ISBN 9780765310026</p></body>"
        ));
    }

    /// Text in front of the image is a chapter that happens to be illustrated,
    /// not a volume opening on its cover.
    #[test]
    fn a_page_that_opens_on_text_opens_no_volume() {
        let html = "<html><body><h1>Chapter One</h1><img src=\"scene.jpg\"/></body></html>";
        assert_eq!(opening_plate_source(html), None);
        assert!(!is_image_only_page(html));
    }

    #[test]
    fn text_or_no_image_at_all_disqualifies_a_page() {
        let none = "<html><body><p>Chapter One</p></body></html>";
        let empty = "<html><body></body></html>";
        for html in [none, empty] {
            assert!(!is_image_only_page(html));
            assert_eq!(single_image_source(html), None);
            assert_eq!(opening_plate_source(html), None);
        }
        // An image with text behind it is a page of pictures to nobody, but the
        // volume that opens on it opens on it all the same.
        let with_text = "<html><body><img src=\"cover.jpeg\"/><p>Chapter One</p></body></html>";
        assert!(!is_image_only_page(with_text));
        assert_eq!(single_image_source(with_text), None);
        assert_eq!(opening_plate_source(with_text), Some("cover.jpeg"));
    }
}
