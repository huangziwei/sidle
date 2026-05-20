//! `/kindle` — server-rendered gallery for the experimental browser on a
//! jailbroken Kindle (and any desktop browser for testing).
//!
//! Inline HTML/CSS, no JS. Filter state lives in the URL: each chip click is
//! a full navigation. Defaults to the same sort order the desktop app uses
//! (newest-imported first — comes from `db::list_books`).
//!
//! Hrefs are *relative* (`/dl/{id}?token=…`, `/cover/{id}?token=…`) so the
//! page works as-is in a desktop browser. Phase 5's KUAL helper will rewrite
//! the cover-cell `href` to `http://127.0.0.1:8765/dl/<id>` at install time
//! so the on-device tcpsvd handler can intercept and save to `/Sidle/`.

use std::collections::HashMap;
use std::fmt::Write;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;

use sidle_core::library::db;

use crate::{AppState, check_token, open_db};

pub async fn page(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Html<String>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let conn = open_db(&state.paths)?;
    let books = db::list_books(&conn).map_err(|err| {
        tracing::error!(?err, "list_books failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Only show books that have a KFX on disk — they're the downloadable
    // ones. EPUB-only / still-converting rows would 404 from /dl.
    let usable: Vec<&db::BookRow> = books.iter().filter(|b| b.kfx_path.is_some()).collect();

    let lang_filter = query.get("lang").map(|s| s.as_str()).filter(|s| !s.is_empty());

    // Distinct languages across the full library (not the filtered set) so
    // chips don't disappear once a filter is applied.
    let mut langs: Vec<&str> = usable
        .iter()
        .map(|b| b.language.as_str())
        .filter(|l| !l.is_empty())
        .collect();
    langs.sort();
    langs.dedup();

    let visible: Vec<&&db::BookRow> = usable
        .iter()
        .filter(|b| match lang_filter {
            Some(l) => b.language == l,
            None => true,
        })
        .collect();

    let token = state.token.as_ref();

    let mut out = String::with_capacity(8 * 1024);
    out.push_str(HEAD);

    // Filter chips ----------------------------------------------------------
    out.push_str(r#"<header class="chips">"#);
    push_chip(
        &mut out,
        "All",
        &url("/kindle", token, &[]),
        lang_filter.is_none(),
    );
    for l in &langs {
        push_chip(
            &mut out,
            l,
            &url("/kindle", token, &[("lang", l)]),
            lang_filter == Some(*l),
        );
    }
    out.push_str("</header>");

    // Gallery ---------------------------------------------------------------
    if visible.is_empty() {
        out.push_str(r#"<main class="empty">No books.</main>"#);
    } else {
        out.push_str(r#"<main class="grid">"#);
        for b in &visible {
            let id = b.id;
            let cover = url(&format!("/cover/{id}"), token, &[]);
            let dl = url(&format!("/dl/{id}"), token, &[]);
            let _ = write!(
                out,
                concat!(
                    r#"<a class="cell" href="{dl}">"#,
                    r#"<img class="cover" src="{cover}" alt="">"#,
                    r#"<div class="title">{title}</div>"#,
                    r#"<div class="author">{author}</div>"#,
                    r#"</a>"#,
                ),
                dl = esc(&dl),
                cover = esc(&cover),
                title = esc(&b.title),
                author = esc(&b.author),
            );
        }
        out.push_str("</main>");
    }

    out.push_str(FOOT);
    Ok(Html(out))
}

/// Build a `/kindle?token=…&lang=…` URL with the token always first so the
/// auth check works for every navigation.
fn url(path: &str, token: &str, params: &[(&str, &str)]) -> String {
    let mut q = String::with_capacity(path.len() + token.len() + 16);
    q.push_str(path);
    q.push('?');
    q.push_str("token=");
    pct_encode(&mut q, token);
    for (k, v) in params {
        q.push('&');
        q.push_str(k);
        q.push('=');
        pct_encode(&mut q, v);
    }
    q
}

fn push_chip(out: &mut String, label: &str, href: &str, active: bool) {
    let cls = if active { "chip active" } else { "chip" };
    let _ = write!(
        out,
        r#"<a class="{cls}" href="{href}">{label}</a>"#,
        cls = cls,
        href = esc(href),
        label = esc(label),
    );
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Minimal percent-encoding for URL query values. We only ever pass ASCII
/// hex tokens and library-provided strings (language codes, etc.), so this
/// covers the cases that actually appear without pulling in `percent-encoding`.
fn pct_encode(out: &mut String, s: &str) {
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

const HEAD: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>sidle</title>
<style>
  * { box-sizing: border-box; }
  html, body { margin: 0; padding: 0; background: #fff; color: #111;
    font-family: -apple-system, BlinkMacSystemFont, "Helvetica Neue", Arial, sans-serif; }
  body { padding: 12px; }
  .chips { display: flex; flex-wrap: wrap; gap: 8px; padding-bottom: 12px;
    border-bottom: 1px solid #ddd; margin-bottom: 12px; }
  .chip { display: inline-block; padding: 10px 14px; min-height: 44px;
    line-height: 1.2; border: 1px solid #888; border-radius: 999px;
    color: #111; text-decoration: none; font-size: 14px; background: #fff; }
  .chip.active { background: #111; color: #fff; border-color: #111; }
  .grid { display: grid; grid-template-columns: repeat(2, 1fr); gap: 16px; }
  @media (min-width: 480px) { .grid { grid-template-columns: repeat(3, 1fr); } }
  @media (min-width: 768px) { .grid { grid-template-columns: repeat(4, 1fr); } }
  .cell { display: block; color: #111; text-decoration: none; }
  .cover { display: block; width: 100%; height: auto; border: 1px solid #ccc;
    background: #f4f4f4; }
  .title { margin-top: 6px; font-size: 13px; line-height: 1.25;
    display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical;
    overflow: hidden; }
  .author { font-size: 11px; color: #666; line-height: 1.2; margin-top: 2px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .empty { padding: 24px; text-align: center; color: #666; }
</style>
</head>
<body>
"#;

const FOOT: &str = "\n</body>\n</html>\n";
