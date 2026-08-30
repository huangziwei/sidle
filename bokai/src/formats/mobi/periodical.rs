//! Front matter for a periodical issue.

use super::index::NcxEntry;

/// Render the contents page for a periodical issue, or `None` when the index
/// is not a periodical tree (no sections, so nothing to lay out).
pub fn issue_front_matter(
    ncx: &[NcxEntry],
    title: &str,
    date: Option<&str>,
    href_for: impl Fn(&NcxEntry) -> String,
    thumbnail_for: impl Fn(u32) -> Option<String>,
) -> Option<String> {
    let sections: Vec<(usize, &NcxEntry)> = ncx
        .iter()
        .enumerate()
        .filter(|(_, e)| is_kind(e, "section", 1))
        .collect();
    if sections.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(4096);
    out.push_str("<div style=\"margin: 0\">\n");

    // Masthead: the publication, then the issue date under a rule.
    out.push_str(&format!(
        "<div style=\"text-align: center; margin-bottom: 1.5em; border-bottom: 1px solid; \
         padding-bottom: 0.5em\">\n<h1 style=\"margin: 0\">{}</h1>\n",
        escape(title)
    ));
    if let Some(date) = date.filter(|d| !d.is_empty()) {
        out.push_str(&format!(
            "<p style=\"margin: 0.25em 0 0; font-size: small\">{}</p>\n",
            escape(date)
        ));
    }
    out.push_str("</div>\n");

    for (index, section) in sections {
        let articles: Vec<&NcxEntry> = ncx
            .iter()
            .filter(|a| a.parent == index as i32 && is_kind(a, "article", 2))
            .collect();
        if articles.is_empty() {
            continue;
        }

        out.push_str(&format!(
            "<h2 style=\"margin: 1.2em 0 0.4em\"><a href=\"{}\">{}</a> \
             <span style=\"font-size: small\">({})</span></h2>\n",
            escape(&href_for(section)),
            escape(&text_of(section)),
            articles.len()
        ));

        for article in articles {
            let href = escape(&href_for(article));
            out.push_str("<div style=\"margin: 0 0 0.9em\">\n");
            if let Some(src) = article.image.and_then(&thumbnail_for) {
                out.push_str(&format!(
                    "<a href=\"{href}\"><img src=\"{}\" alt=\"\" \
                     style=\"max-width: 100%; margin-bottom: 0.2em\" /></a>\n",
                    escape(&src)
                ));
            }
            out.push_str(&format!(
                "<p style=\"margin: 0\"><a href=\"{href}\"><b>{}</b></a></p>\n",
                escape(&text_of(article))
            ));
            if let Some(author) = non_empty(article.author.as_deref()) {
                out.push_str(&format!(
                    "<p style=\"margin: 0; font-size: small; font-style: italic\">{}</p>\n",
                    escape(author)
                ));
            }
            if let Some(description) = non_empty(article.description.as_deref()) {
                out.push_str(&format!(
                    "<p style=\"margin: 0; font-size: small\">{}</p>\n",
                    escape(description)
                ));
            }
            out.push_str("</div>\n");
        }
    }

    out.push_str("</div>\n");
    Some(out)
}

/// Does this entry claim the given kind, either by tag 5 or by its depth?
fn is_kind(entry: &NcxEntry, kind: &str, level: i32) -> bool {
    match entry.kind.as_deref() {
        Some(declared) => declared.eq_ignore_ascii_case(kind),
        None => entry.level == level,
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// An entry's label with any entities resolved, matching how the same string is
/// read for the table of contents.
fn text_of(entry: &NcxEntry) -> String {
    quick_xml::escape::unescape(&entry.text)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| entry.text.clone())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> NcxEntry {
        NcxEntry {
            name: String::new(),
            text: String::new(),
            pos: 0,
            length: 0,
            level: 0,
            parent: -1,
            pos_fid: None,
            kind: None,
            children: None,
            description: None,
            author: None,
            image: None,
        }
    }

    fn entry(text: &str, kind: &str, level: i32, parent: i32) -> NcxEntry {
        NcxEntry {
            text: text.to_string(),
            kind: Some(kind.to_string()),
            level,
            parent,
            ..blank()
        }
    }

    /// The shape every issue has: a periodical root, sections, and articles
    /// whose editorial metadata is present on only some of them.
    fn issue() -> Vec<NcxEntry> {
        let mut root = entry("Table of Contents", "periodical", 0, -1);
        root.children = Some((1, 2));
        let mut talk = entry("The Talk of the Town", "section", 1, 0);
        talk.pos = 381;
        let mut cartoons = entry("Cartoons", "section", 1, 0);
        cartoons.pos = 337130;

        let mut comment = entry("COMMENT: SAFER STREETS", "article", 2, 1);
        comment.pos = 391;
        comment.author = Some("BY AMY DAVIDSON".to_string());
        comment.description = Some("On June 3, 1999, Loretta Lynch…".to_string());

        let mut mail = entry("THE MAIL", "article", 2, 1);
        mail.pos = 500;
        mail.description = Some("Letters from our readers.".to_string());

        let mut katz = entry("FARLEY KATZ", "article", 2, 2);
        katz.pos = 337140;
        katz.image = Some(11);

        vec![root, talk, cartoons, comment, mail, katz]
    }

    fn render(ncx: &[NcxEntry]) -> Option<String> {
        issue_front_matter(
            ncx,
            "The New Yorker",
            Some("2014-12-14"),
            |e| format!("chapter_0.xhtml#filepos{}", e.pos),
            |offset| Some(format!("images/image_{offset:04}.jpg")),
        )
    }

    #[test]
    fn every_section_and_article_is_listed_and_linked() {
        let html = render(&issue()).expect("a periodical renders");

        assert!(html.contains("<h1 style=\"margin: 0\">The New Yorker</h1>"));
        assert!(html.contains("2014-12-14"));
        // Section heading links to the section and states its article count.
        assert!(
            html.contains(
                "<a href=\"chapter_0.xhtml#filepos381\">The Talk of the Town</a> \
                 <span style=\"font-size: small\">(2)</span>"
            ),
            "{html}"
        );
        assert!(
            html.contains(
                "<a href=\"chapter_0.xhtml#filepos391\"><b>COMMENT: SAFER STREETS</b></a>"
            )
        );
        assert!(html.contains("BY AMY DAVIDSON"));
        assert!(html.contains("On June 3, 1999, Loretta Lynch…"));
        assert!(html.contains("Cartoons</a> <span style=\"font-size: small\">(1)</span>"));
    }

    #[test]
    fn absent_metadata_leaves_no_empty_element() {
        let html = render(&issue()).unwrap();

        // THE MAIL has a standfirst but no byline; the cartoon has a thumbnail
        // and neither. Neither shape emits a blank paragraph.
        assert!(html.contains("Letters from our readers."));
        assert!(!html.contains("font-style: italic\"></p>"));
        assert!(html.contains("<img src=\"images/image_0011.jpg\""));
        assert_eq!(html.matches("<img").count(), 1, "only the one thumbnail");
    }

    #[test]
    fn a_section_with_no_articles_is_skipped() {
        let mut ncx = issue();
        ncx.push(entry("Empty", "section", 1, 0));
        let html = render(&ncx).unwrap();
        assert!(!html.contains("Empty"));
    }

    #[test]
    fn a_book_has_no_front_matter() {
        // A plain book's NCX is a flat list of chapters: no sections, so there
        // is no periodical structure to lay out.
        let ncx = vec![entry("Chapter 1", "", 0, -1), entry("Chapter 2", "", 0, -1)];
        assert!(render(&ncx).is_none());
    }

    #[test]
    fn labels_are_escaped_once() {
        let mut ncx = issue();
        ncx[1].text = "Reporting &amp; Essays".to_string();
        ncx[3].author = Some("BY A. T. & T.".to_string());
        let html = render(&ncx).unwrap();

        // The label arrives entity-encoded and the byline does not; both end up
        // encoded exactly once.
        assert!(html.contains("Reporting &amp; Essays<"), "{html}");
        assert!(html.contains("BY A. T. &amp; T."), "{html}");
        assert!(!html.contains("&amp;amp;"), "{html}");
    }
}
