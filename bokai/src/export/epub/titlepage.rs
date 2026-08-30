//! SVG cover wrapper for the EPUB titlepage — one builder for the normalized and
//! raw exports alike, so every synthesized cover page has the same shape.

/// Build the titlepage document. `dims` is the cover's pixel size
/// (`None` / zero → the `<img>` fallback variant). `viewport` states the page
/// box a pre-paginated content document carries.
pub(crate) fn build_titlepage(
    cover_href: &str,
    dims: Option<(u32, u32)>,
    viewport: Option<(u32, u32)>,
) -> String {
    let href = escape_xml(cover_href);
    let viewport_meta = match viewport {
        Some((vw, vh)) if vw > 0 && vh > 0 => {
            format!("<meta name=\"viewport\" content=\"width={vw}, height={vh}\"/>\n")
        }
        _ => String::new(),
    };
    match dims {
        Some((w, h)) if w > 0 && h > 0 => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE html>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\">\n\
             <head>\n\
             <meta http-equiv=\"Content-Type\" content=\"text/html; charset=UTF-8\"/>\n\
             {viewport_meta}\
             <title>Cover</title>\n\
             <style type=\"text/css\">\n\
             @page {{padding: 0pt; margin:0pt}}\n\
             html, body {{ height: 100%; width: 100%; }}\n\
             body {{ text-align: center; padding:0pt; margin: 0pt; }}\n\
             svg {{ display: block; height: 100%; width: 100%; }}\n\
             </style>\n\
             </head>\n\
             <body>\n\
             <svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" version=\"1.1\" width=\"100%\" height=\"100%\" viewBox=\"0 0 {w} {h}\" preserveAspectRatio=\"xMidYMid meet\">\n\
             <image width=\"{w}\" height=\"{h}\" xlink:href=\"{href}\"/>\n\
             </svg>\n\
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
