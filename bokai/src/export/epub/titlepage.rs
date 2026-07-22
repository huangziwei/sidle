//! SVG cover wrapper for the EPUB titlepage — one builder for the normalized and
//! raw exports alike, so every synthesized cover page has the same shape.
//!
//! An SVG `viewBox` sized to the cover image's pixel dimensions, with the image
//! referenced via `xlink:href`, renders full-bleed in Apple Books / Kindle: the
//! `viewBox` is self-contained CSS-wise, bypassing the body-margin defaults a
//! plain `<img>` would inherit. Without dimensions the `viewBox` would collapse,
//! so a bare `<img>` wrapper ships instead. The cover page is identified through
//! the OPF (`<meta name="cover">` / `properties="cover-image"` and the guide
//! reference), not an in-page marker.

/// Build the titlepage document. `dims` is the cover's pixel size
/// (`None` / zero → the `<img>` fallback variant).
pub(crate) fn build_titlepage(cover_href: &str, dims: Option<(u32, u32)>) -> String {
    let href = escape_xml(cover_href);
    match dims {
        Some((w, h)) if w > 0 && h > 0 => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\">\n\
             <head>\n\
             <meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/>\n\
             <title>Cover</title>\n\
             <style type=\"text/css\">\n\
             @page {{padding: 0pt; margin:0pt}}\n\
             body {{ text-align: center; padding:0pt; margin: 0pt; }}\n\
             </style>\n\
             </head>\n\
             <body>\n\
             <div>\n\
             <svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" version=\"1.1\" width=\"100%\" height=\"100%\" viewBox=\"0 0 {w} {h}\" preserveAspectRatio=\"xMidYMid meet\">\n\
             <image width=\"{w}\" height=\"{h}\" xlink:href=\"{href}\"/>\n\
             </svg>\n\
             </div>\n\
             </body>\n\
             </html>\n"
        ),
        _ => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\">\n\
             <head>\n\
             <meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/>\n\
             <title>Cover</title>\n\
             </head>\n\
             <body><div><img src=\"{href}\" alt=\"\"/></div></body>\n\
             </html>\n"
        ),
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
