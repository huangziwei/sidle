use crate::html::resolve_path;
use crate::util::percent_decode;

pub fn inline_css_imports<F>(src: &str, base: &str, mut load: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && src[i..].to_ascii_lowercase().starts_with("@import") {
            let mut j = i + "@import".len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let parsed = if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                quoted_url(src, j)
            } else if j + 4 <= bytes.len() && src[j..j + 4].eq_ignore_ascii_case("url(") {
                url_function(src, j)
            } else {
                None
            };
            if let Some((url, mut k)) = parsed {
                while k < bytes.len() && bytes[k] != b';' && bytes[k] != b'}' {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b';' {
                    k += 1;
                }
                out.push_str(&src[copied..i]);
                let child = resolve_path(base, &percent_decode(url));
                if let Some(child_css) = load(&child) {
                    out.push_str(&child_css);
                    out.push('\n');
                }
                copied = k;
                i = k;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&src[copied..]);
    out
}

pub fn css_import_targets(src: &str, base: &str) -> Vec<String> {
    let mut targets = Vec::new();
    inline_css_imports(src, base, |child| {
        targets.push(child.to_string());
        None
    });
    targets
}

fn quoted_url(src: &str, q_pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let quote = bytes[q_pos];
    let start = q_pos + 1;
    let end_rel = bytes[start..].iter().position(|&b| b == quote)?;
    Some((&src[start..start + end_rel], start + end_rel + 1))
}

fn url_function(src: &str, u_pos: usize) -> Option<(&str, usize)> {
    let bytes = src.as_bytes();
    let mut p = u_pos + 4;
    while p < bytes.len() && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    let (url, end) = if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
        quoted_url(src, p)?
    } else {
        let url_start = p;
        while p < bytes.len() && bytes[p] != b')' && !bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        (&src[url_start..p], p)
    };
    let mut k = end;
    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    if k >= bytes.len() || bytes[k] != b')' {
        return None;
    }
    Some((url, k + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_parent_dir_in_url() {
        let mut requested: Vec<String> = Vec::new();
        let out = inline_css_imports(
            r#"@import url("../Styles/style0007.css"); body {}"#,
            "OEBPS/Styles/style0011.css",
            |p| {
                requested.push(p.to_string());
                Some(".vrtl { writing-mode: vertical-rl }".into())
            },
        );
        assert_eq!(requested, vec!["OEBPS/Styles/style0007.css"]);
        assert!(out.contains(".vrtl"));
        assert!(out.contains("body {}"));
    }

    #[test]
    fn handles_bare_url_function_and_lists_targets() {
        let targets = css_import_targets(
            "@import url(flow0007.css);\n@import 'flow0008.css';",
            "OEBPS/Styles/flow0011.css",
        );
        assert_eq!(
            targets,
            vec!["OEBPS/Styles/flow0007.css", "OEBPS/Styles/flow0008.css"]
        );
    }
}
