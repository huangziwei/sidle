//! Aozora Bunko `.txt` parser.
//!
//! Faithful port of `parseTxt` + `convertAozoraLine` from
//! `/Users/ziweih/projects/tools/aozora-epub.html`. The HTML tool is the
//! spec; output XHTML structure should be functionally identical.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// Parsed Aozora document, ready for the EPUB builder.
#[derive(Debug, Clone)]
pub struct Document {
    pub title: String,
    pub author: String,
    /// Inner XHTML of the main body: a sequence of `<p>`, `<h2>`, `<h3>`,
    /// `<h4>`, and image elements. Not wrapped in `<html>`/`<body>`.
    pub body_xhtml: String,
    pub toc: Vec<TocEntry>,
    pub colophon: String,
    /// Image filenames referenced via `［＃...（filename）入る］`. The EPUB
    /// builder uses this to know which zip entries to include + manifest.
    pub referenced_images: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TocEntry {
    /// HTML id attribute value, e.g. `"h1"`, `"h2"`.
    pub id: String,
    /// 2 for `<h2>`, 3 for `<h3>`, 4 for `<h4>`.
    pub level: u8,
    /// Plain text, ruby + `｜` markers stripped — used for nav labels.
    pub text: String,
}

// =========================================================================
// Decoding
// =========================================================================

/// Decode Aozora source bytes: UTF-8 (with BOM detection) preferred,
/// Shift-JIS fallback. Mirrors the HTML tool's `detectAndDecode`.
pub fn decode_bytes(bytes: &[u8]) -> Cow<'_, str> {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        let (s, _, _) = encoding_rs::UTF_8.decode(bytes);
        return s;
    }
    let (s, _, malformed) = encoding_rs::UTF_8.decode(bytes);
    if !malformed {
        return s;
    }
    let (s, _, _) = encoding_rs::SHIFT_JIS.decode(bytes);
    s
}

// =========================================================================
// Top-level parser
// =========================================================================

/// Parse Aozora `.txt` source into a [`Document`]. Mirrors the HTML tool's
/// `parseTxt`.
pub fn parse_txt(text: &str) -> Document {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    // First non-empty line = title; second = author.
    let mut header_lines: Vec<&str> = Vec::new();
    let mut header_idx = 0;
    for (i, line) in lines.iter().enumerate() {
        if header_lines.len() >= 2 {
            break;
        }
        if !line.trim().is_empty() {
            header_lines.push(line.trim());
        }
        header_idx = i;
    }
    let title = header_lines.first().copied().unwrap_or("").to_string();
    let author = header_lines.get(1).copied().unwrap_or("").to_string();
    let _ = header_idx;

    // Body starts after the last `----...` separator.
    let sep_re = Regex::new(r"^-{5,}").unwrap();
    let mut body_start = 0;
    for (j, line) in lines.iter().enumerate() {
        if sep_re.is_match(line.trim()) {
            body_start = j + 1;
        }
    }

    // Body ends at the first trailing `底本：` line; everything after that
    // up to `青空文庫作成ファイル` / `このファイルは` is the colophon.
    let colophon_start_re = Regex::new(r"^底本[：:]").unwrap();
    let colophon_end_re = Regex::new(r"^(青空文庫作成ファイル|このファイルは)").unwrap();
    let mut body_end = lines.len();
    for k in (body_start + 1..lines.len()).rev() {
        if colophon_start_re.is_match(lines[k].trim()) {
            body_end = k;
            break;
        }
    }
    let mut colophon_lines: Vec<String> = Vec::new();
    for c in body_end..lines.len() {
        let cl = lines[c].trim();
        if colophon_end_re.is_match(cl) {
            break;
        }
        if !cl.is_empty() {
            colophon_lines.push(cl.to_string());
        }
    }
    let colophon = colophon_lines.join("\n");

    // Trim leading + trailing empty lines from the body slice.
    let mut body_lines: Vec<&str> = lines[body_start..body_end].to_vec();
    while body_lines.first().is_some_and(|l| l.trim().is_empty()) {
        body_lines.remove(0);
    }
    while body_lines.last().is_some_and(|l| l.trim().is_empty()) {
        body_lines.pop();
    }

    let mut state = BodyState::default();
    for raw in body_lines {
        process_line(raw, &mut state);
    }

    Document {
        title,
        author,
        body_xhtml: state.html,
        toc: state.toc,
        colophon,
        referenced_images: state.referenced_images,
    }
}

// =========================================================================
// Body-loop state machine
// =========================================================================

#[derive(Default)]
struct BodyState {
    indent_level: u32,
    block_styles: Vec<&'static str>,
    heading_id: u32,
    toc: Vec<TocEntry>,
    html: String,
    referenced_images: Vec<String>,
}

fn block_style_class(name: &str) -> Option<&'static str> {
    match name {
        "横組み" => Some("yokogumi"),
        "ゴシック体" => Some("gothic"),
        "斜体" => Some("italic"),
        "罫囲み" | "枠囲み" => Some("keigakomi"),
        "破線罫囲み" | "破線枠囲み" => Some("keigakomi-dashed"),
        "二重罫囲み" | "二重枠囲み" => Some("keigakomi-double"),
        _ => None,
    }
}

static INDENT_START_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃ここから[０-９0-9]+字下げ[^］]*］").unwrap());
static INDENT_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃ここで字下げ終わり］").unwrap());
static BLOCK_START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"［＃ここから(横組み|ゴシック体|斜体|破線罫囲み|破線枠囲み|二重罫囲み|二重枠囲み|罫囲み|枠囲み)］").unwrap()
});
static BLOCK_END_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"［＃ここで(横組み|ゴシック体|斜体|[^］]*囲み)終わり］").unwrap()
});
static PAGE_BREAK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃(改ページ|改丁|ページの左右中央)］").unwrap());
static PAGE_BREAK_LINE_ONLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^［＃(改ページ|改丁|ページの左右中央)］\s*$").unwrap());
static HEADING_PRECEDES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃[大中小]見出し］").unwrap());
/// Postfix heading marker (Aozora's *other* heading convention, not
/// supported by the original HTML tool): `［＃「TEXT」は<大|中|小>見出し］`
/// says the immediately-preceding `TEXT` is a heading. Common in long
/// Aozora prose (e.g. 夏目漱石『吾輩は猫である』, 寺田寅彦『柿の種』);
/// without this, those books produce no TOC.
static POSTFIX_HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃「([^」]+)」は([大中小])見出し］").unwrap());
static HEADING_OOMIDASHI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃大見出し］(.+?)［＃大見出し終わり］").unwrap());
static HEADING_NAKAMIDASHI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃中見出し］(.+?)［＃中見出し終わり］").unwrap());
static HEADING_KOMIDASHI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃小見出し］(.+?)［＃小見出し終わり］").unwrap());
static INDENT_SINGLE_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃[０-９0-9]+字下げ］").unwrap());
static EDITORIAL_BASE_NOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃「[^」]*」は底本では[^］]*］").unwrap());

fn process_line(raw_in: &str, state: &mut BodyState) {
    let mut raw = raw_in.to_string();

    // Block indent start (consume marker; rest of line continues).
    if INDENT_START_RE.is_match(&raw) {
        state.indent_level += 1;
        raw = INDENT_START_RE.replace(&raw, "").to_string();
        if raw.trim().is_empty() {
            return;
        }
    }
    // Block indent end. Any trailing line content gets a single indented <p>,
    // then indent level resets (matches HTML tool — *not* a stack).
    if INDENT_END_RE.is_match(&raw) {
        raw = INDENT_END_RE.replace(&raw, "").to_string();
        if !raw.trim().is_empty() {
            let inner = convert_aozora_line(&raw, &mut state.referenced_images);
            state.html.push_str(r#"<p class="indent">"#);
            state.html.push_str(&inner);
            state.html.push_str("</p>\n");
        }
        state.indent_level = 0;
        return;
    }

    // Block-level style start.
    if let Some(caps) = BLOCK_START_RE.captures(&raw) {
        let name = caps.get(1).unwrap().as_str();
        if let Some(cls) = block_style_class(name) {
            state.block_styles.push(cls);
        }
        raw = BLOCK_START_RE.replace(&raw, "").to_string();
        if raw.trim().is_empty() {
            return;
        }
    }
    // Block-level style end.
    if BLOCK_END_RE.is_match(&raw) {
        state.block_styles.pop();
        // Note: JS uses a looser end-strip regex than start. Match its
        // behavior — strip anything `［＃ここで...終わり］`.
        let end_strip = Regex::new(r"［＃ここで[^］]*終わり］").unwrap();
        raw = end_strip.replace(&raw, "").to_string();
        if raw.trim().is_empty() {
            return;
        }
    }

    // Page / section breaks reset block state. If the whole line is just a
    // page-break marker, skip it; otherwise strip and continue.
    if PAGE_BREAK_RE.is_match(&raw) {
        state.indent_level = 0;
        state.block_styles.clear();
        if PAGE_BREAK_LINE_ONLY_RE.is_match(raw.trim()) {
            return;
        }
        raw = PAGE_BREAK_RE.replace_all(&raw, "").to_string();
    }

    // Heading lines reset block state.
    if HEADING_PRECEDES_RE.is_match(&raw) {
        state.indent_level = 0;
        state.block_styles.clear();
    }

    // 大見出し → <h2>
    if let Some(caps) = HEADING_OOMIDASHI_RE.captures(&raw) {
        state.heading_id += 1;
        let inner_marker = caps.get(1).unwrap().as_str().to_string();
        // Editorial notes (`［＃「…」は底本では…］`) stripped before plain-text
        // extraction so they don't end up in the TOC label.
        let h_text = strip_editorial_notes_for_heading(&inner_marker);
        let plain = plain_text_for_heading(&h_text);
        let id = format!("h{}", state.heading_id);
        state.toc.push(TocEntry {
            id: id.clone(),
            level: 2,
            text: plain,
        });
        let inner = convert_aozora_line(&h_text, &mut state.referenced_images);
        state.html.push_str(&format!("<h2 id=\"{}\">{}</h2>\n", id, inner));
        return;
    }
    if let Some(caps) = HEADING_NAKAMIDASHI_RE.captures(&raw) {
        state.heading_id += 1;
        let inner_marker = caps.get(1).unwrap().as_str();
        let h_text = strip_editorial_notes_for_heading(inner_marker);
        let plain = plain_text_for_heading(&h_text);
        let id = format!("h{}", state.heading_id);
        state.toc.push(TocEntry {
            id: id.clone(),
            level: 3,
            text: plain,
        });
        let inner = convert_aozora_line(&h_text, &mut state.referenced_images);
        state.html.push_str(&format!("<h3 id=\"{}\">{}</h3>\n", id, inner));
        return;
    }
    if let Some(caps) = HEADING_KOMIDASHI_RE.captures(&raw) {
        state.heading_id += 1;
        let inner = caps.get(1).unwrap().as_str().to_string();
        let plain = plain_text_for_heading(&inner);
        let id = format!("h{}", state.heading_id);
        state.toc.push(TocEntry {
            id: id.clone(),
            level: 4,
            text: plain,
        });
        let converted = convert_aozora_line(&inner, &mut state.referenced_images);
        state
            .html
            .push_str(&format!("<h4 id=\"{}\">{}</h4>\n", id, converted));
        return;
    }

    // Inline indent prefix `［＃N字下げ］` — strip everywhere on the line.
    raw = INDENT_SINGLE_PREFIX_RE.replace_all(&raw, "").to_string();

    // Postfix heading form: `TEXT［＃「TEXT」は<大|中|小>見出し］`. Detect
    // before the regular-paragraph fallback. The preceding text may carry
    // `｜`/`《…》` ruby markup; compare against the bracketed plain form
    // after stripping markup (via `plain_text_for_heading`), and render
    // the raw form so `convert_aozora_line` can emit `<ruby>` inside the
    // heading. Strict superset over the JS reference — see the
    // [[postfix_heading_re]] doc comment.
    if let Some(caps) = POSTFIX_HEADING_RE.captures(&raw) {
        let m = caps.get(0).unwrap();
        let target = caps.get(1).unwrap().as_str().to_string();
        let level_kind = caps.get(2).unwrap().as_str();
        let head = &raw[..m.start()];
        let head_trimmed = head.trim_end();
        if plain_text_for_heading(head_trimmed) == target {
            let level = match level_kind {
                "大" => 2,
                "中" => 3,
                _ => 4,
            };
            state.heading_id += 1;
            let id = format!("h{}", state.heading_id);
            state.toc.push(TocEntry {
                id: id.clone(),
                level,
                text: target.clone(), // already plain
            });
            let converted = convert_aozora_line(head_trimmed, &mut state.referenced_images);
            state.html.push_str(&format!(
                "<h{} id=\"{}\">{}</h{}>\n",
                level, id, converted, level
            ));
            return;
        }
        // Postfix annotation present but the preceding text doesn't match
        // the quoted target verbatim (rare — usually means the heading text
        // contains other markup we'd need to process first). Fall through;
        // the inline-strip in `convert_aozora_line` will drop the marker so
        // the heading still renders as a paragraph (matches HTML-tool
        // behavior for this edge).
    }

    if raw.trim().is_empty() {
        state.html.push_str("<p><br/></p>\n");
        return;
    }

    let mut classes: Vec<&str> = Vec::new();
    if state.indent_level > 0 {
        classes.push("indent");
    }
    for cls in &state.block_styles {
        classes.push(cls);
    }
    let p_attr = if classes.is_empty() {
        String::new()
    } else {
        format!(r#" class="{}""#, classes.join(" "))
    };
    let inner = convert_aozora_line(&raw, &mut state.referenced_images);
    state.html.push_str(&format!("<p{}>{}</p>\n", p_attr, inner));
}

fn strip_editorial_notes_for_heading(s: &str) -> String {
    EDITORIAL_BASE_NOTE_RE.replace_all(s, "").to_string()
}

/// Plain-text version of a heading: ruby + explicit-marker `｜` removed.
/// Mirrors the JS `replace(/《[^》]*》/g, '').replace(/｜/g, '')`.
fn plain_text_for_heading(s: &str) -> String {
    static RUBY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"《[^》]*》").unwrap());
    RUBY_RE.replace_all(s, "").replace('｜', "")
}

// =========================================================================
// Inline annotation processing (convertAozoraLine)
// =========================================================================

fn convert_aozora_line(line: &str, images: &mut Vec<String>) -> String {
    let mut s = escape_xml(line);

    // Image refs: ［＃description（filename、dims）入る］
    static IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃([^（］]*)（([^、]+)、[^）]*）入る］").unwrap()
    });
    s = replace_all_with(&IMG_RE, &s, |caps| {
        let alt = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        let filename = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        images.push(filename.to_string());
        format!(
            r#"<img src="../images/{}" alt="{}"/>"#,
            filename, alt
        )
    });

    // Ruby with explicit ｜ marker: ｜base《reading》
    static RUBY_MARKER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"｜([^《]+?)《([^》]+)》").unwrap());
    s = replace_all_with(&RUBY_MARKER_RE, &s, |caps| {
        format!(
            "<ruby>{}<rp>（</rp><rt>{}</rt><rp>）</rp></ruby>",
            &caps[1], &caps[2]
        )
    });

    // Ruby on bare CJK runs: kanji《reading》
    static RUBY_CJK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"([\u{4E00}-\u{9FFF}\u{3400}-\u{4DBF}\u{F900}-\u{FAFF}]+)《([^》]+)》")
            .unwrap()
    });
    s = replace_all_with(&RUBY_CJK_RE, &s, |caps| {
        format!(
            "<ruby>{}<rp>（</rp><rt>{}</rt><rp>）</rp></ruby>",
            &caps[1], &caps[2]
        )
    });

    // --- Block-form paired annotations ---
    for form in PAIRED_FORMS.iter() {
        s = replace_all_with(&form.re, &s, |caps| {
            format!("{}{}{}", form.open, &caps[1], form.close)
        });
    }

    // Batsu (ばつ or ×) bouten — two open/close forms.
    static BATSU_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃(?:ばつ|×)傍点］([^［]*)［＃(?:ばつ|×)傍点終わり］").unwrap()
    });
    s = replace_all_with(&BATSU_RE, &s, |caps| {
        format!(r#"<em class="batsu">{}</em>"#, &caps[1])
    });

    // 罫囲み / 枠囲み — two name forms map to the same class. Handle each
    // pair explicitly so opening/closing names can differ.
    static FRAME_PLAIN_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃(?:罫囲み|枠囲み)］([^［]*)［＃(?:罫囲み|枠囲み)終わり］").unwrap()
    });
    s = replace_all_with(&FRAME_PLAIN_RE, &s, |caps| {
        format!(r#"<span class="keigakomi">{}</span>"#, &caps[1])
    });
    static FRAME_DASHED_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃破線(?:罫囲み|枠囲み)］([^［]*)［＃破線(?:罫囲み|枠囲み)終わり］")
            .unwrap()
    });
    s = replace_all_with(&FRAME_DASHED_RE, &s, |caps| {
        format!(r#"<span class="keigakomi-dashed">{}</span>"#, &caps[1])
    });
    static FRAME_DOUBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃二重(?:罫囲み|枠囲み)］([^［]*)［＃二重(?:罫囲み|枠囲み)終わり］")
            .unwrap()
    });
    s = replace_all_with(&FRAME_DOUBLE_RE, &s, |caps| {
        format!(r#"<span class="keigakomi-double">{}</span>"#, &caps[1])
    });


    // 返り点 (kanbun reading marks)
    static KAERITEN_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃(返り点)］([^［]*)［＃\u{8FD4}\u{308A}\u{70B9}終わり］").unwrap()
    });
    s = replace_all_with(&KAERITEN_RE, &s, |caps| {
        format!(r#"<sup class="kaeriten">{}</sup>"#, &caps[2])
    });

    // Font size: ［＃N段階大きな文字］...
    static BIGGER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃([０-９0-9]+)段階大きな文字］([^［]*)［＃大きな文字終わり］").unwrap()
    });
    s = replace_all_with(&BIGGER_RE, &s, |caps| {
        let n = parse_zenkaku_int(&caps[1]).unwrap_or(1);
        let em = 1.0 + n as f32 * 0.2;
        format!(r#"<span style="font-size:{}em">{}</span>"#, em, &caps[2])
    });
    // ［＃N段階小さな文字］...
    static SMALLER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"［＃([０-９0-9]+)段階小さな文字］([^［]*)［＃小さな文字終わり］").unwrap()
    });
    s = replace_all_with(&SMALLER_RE, &s, |caps| {
        let n = parse_zenkaku_int(&caps[1]).unwrap_or(1);
        let em = (1.0 - n as f32 * 0.1).max(0.6);
        format!(r#"<span style="font-size:{}em">{}</span>"#, em, &caps[2])
    });

    // --- Postfix annotations: ［＃「text」にXXX］ — wrap the immediately-
    // preceding occurrence of `text` in `s`. Process right-to-left so byte
    // positions stay valid after splicing.
    s = apply_postfix_annotations(&s);

    // Inline notes
    static WARIRYUU_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃割り注］([^［]*)［＃割り注終わり］").unwrap());
    s = replace_all_with(&WARIRYUU_RE, &s, |caps| {
        format!("<small>（{}）</small>", &caps[1])
    });

    // Gaiji: ※［＃description、code］ → keep ※
    static GAIJI_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"※［＃[^］]*］").unwrap());
    s = GAIJI_RE.replace_all(&s, "※").to_string();

    // Named special chars
    static KANTAN_GIMON_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃感嘆符疑問符、[^\]]*］").unwrap());
    s = KANTAN_GIMON_RE.replace_all(&s, "\u{2049}").to_string();
    static KANTAN_FUTATSU_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃感嘆符二つ、[^\]]*］").unwrap());
    s = KANTAN_FUTATSU_RE.replace_all(&s, "\u{203C}").to_string();
    static DAKUTEN_WA_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃濁点付き片仮名ワ、[^\]]*］").unwrap());
    s = DAKUTEN_WA_RE.replace_all(&s, "\u{30F7}").to_string();
    static ALEPH_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃アレフ、[^\]]*］").unwrap());
    s = ALEPH_RE.replace_all(&s, "\u{05D0}").to_string();

    // Strip editorial notes and heading-reference notes.
    s = EDITORIAL_BASE_NOTE_RE.replace_all(&s, "").to_string();
    static HEADING_REF_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃「[^」]*」は[大中小]見出し］").unwrap());
    s = HEADING_REF_RE.replace_all(&s, "").to_string();

    // Final catch-all: drop any remaining ［＃...］.
    static REMAINING_ANN_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"［＃[^］]*］").unwrap());
    s = REMAINING_ANN_RE.replace_all(&s, "").to_string();

    s
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

struct PairedForm {
    open: &'static str,
    close: &'static str,
    re: Regex,
}

fn paired_form(name: &str, open: &'static str, close: &'static str) -> PairedForm {
    let pat = format!(r"［＃{0}］([^［]*)［＃{0}終わり］", regex::escape(name));
    PairedForm {
        open,
        close,
        re: Regex::new(&pat).unwrap(),
    }
}

/// Pre-compiled paired-annotation regex set. Order matches the HTML tool's
/// `convertAozoraLine`. Longest names first so e.g. `二重傍線` is consumed
/// before `傍線` would otherwise greedy-match the inner text.
static PAIRED_FORMS: LazyLock<Vec<PairedForm>> = LazyLock::new(|| {
    vec![
        // sesame variants
        paired_form("白ゴマ傍点", r#"<em class="open-sesame">"#, "</em>"),
        paired_form("黒三角傍点", r#"<em class="triangle">"#, "</em>"),
        paired_form("白三角傍点", r#"<em class="open-triangle">"#, "</em>"),
        paired_form("二重丸傍点", r#"<em class="double-circle">"#, "</em>"),
        paired_form("蛇の目傍点", r#"<em class="double-circle">"#, "</em>"),
        paired_form("白丸傍点", r#"<em class="open-circle">"#, "</em>"),
        paired_form("丸傍点", r#"<em class="circle">"#, "</em>"),
        paired_form("傍点", r#"<em class="sesame">"#, "</em>"),
        // underline / strikethrough variants — long names first
        paired_form("二重取消線", r#"<span class="strikethrough-double">"#, "</span>"),
        paired_form("取消線", r#"<span class="strikethrough">"#, "</span>"),
        paired_form("二重傍線", r#"<span class="underline-double">"#, "</span>"),
        paired_form("傍線", r#"<span class="underline">"#, "</span>"),
        paired_form("波線", r#"<span class="underline-wavy">"#, "</span>"),
        paired_form("破線", r#"<span class="underline-dashed">"#, "</span>"),
        paired_form("鎖線", r#"<span class="underline-dotted">"#, "</span>"),
        // style + script
        paired_form("太字", "<strong>", "</strong>"),
        paired_form("ゴシック体", r#"<span class="gothic">"#, "</span>"),
        paired_form("斜体", "<i>", "</i>"),
        paired_form("横組み", r#"<span class="yokogumi">"#, "</span>"),
        paired_form("上付き小文字", "<sup>", "</sup>"),
        paired_form("下付き小文字", "<sub>", "</sub>"),
        paired_form("行右小書き", "<sup>", "</sup>"),
        paired_form("行左小書き", "<sub>", "</sub>"),
    ]
});

fn replace_all_with(re: &Regex, s: &str, mut f: impl FnMut(&Captures) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for caps in re.captures_iter(s) {
        let m = caps.get(0).unwrap();
        out.push_str(&s[last..m.start()]);
        out.push_str(&f(&caps));
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

fn parse_zenkaku_int(s: &str) -> Option<u32> {
    // Convert full-width digits to half-width, then parse.
    let mut buf = String::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if (0xFF10..=0xFF19).contains(&cp) {
            buf.push(char::from_u32('0' as u32 + (cp - 0xFF10))?);
        } else {
            buf.push(c);
        }
    }
    buf.parse().ok()
}

// =========================================================================
// Postfix annotations
// =========================================================================

/// Map of `key` suffix → `(open_tag_inner, close_tag_name)` for postfix
/// annotations `［＃「text」に<key>］` / `［＃「text」は<key>］`. The pair
/// wraps the immediately-preceding occurrence of `text`.
fn postfix_table() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("に傍点", r#"em class="sesame""#, "em"),
        ("に傍線", r#"span class="underline""#, "span"),
        ("に二重傍線", r#"span class="underline-double""#, "span"),
        ("に波線", r#"span class="underline-wavy""#, "span"),
        ("に破線", r#"span class="underline-dashed""#, "span"),
        ("に鎖線", r#"span class="underline-dotted""#, "span"),
        ("は太字", "strong", "strong"),
        ("はゴシック体", r#"span class="gothic""#, "span"),
        ("は斜体", "i", "i"),
        ("は罫囲み", r#"span class="keigakomi""#, "span"),
        ("は枠囲み", r#"span class="keigakomi""#, "span"),
        ("は行右小書き", "sup", "sup"),
        ("は行左小書き", "sub", "sub"),
    ]
}

fn apply_postfix_annotations(s: &str) -> String {
    // Match keys longest-first so `に二重傍線` wins over `に傍線`.
    let mut keys: Vec<&str> = postfix_table().iter().map(|(k, _, _)| *k).collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()));
    let escaped_alt = keys
        .iter()
        .map(|k| regex::escape(k))
        .collect::<Vec<_>>()
        .join("|");
    let pat = format!(r"［＃「([^」]+)」({})］", escaped_alt);
    let re = Regex::new(&pat).unwrap();

    // Collect matches with byte positions, then splice right-to-left.
    let matches: Vec<(usize, usize, String, String)> = re
        .captures_iter(s)
        .map(|caps| {
            let m = caps.get(0).unwrap();
            (
                m.start(),
                m.end(),
                caps[1].to_string(),
                caps[2].to_string(),
            )
        })
        .collect();

    let table = postfix_table();
    let mut out = s.to_string();
    for (start, end, target, key) in matches.into_iter().rev() {
        let tags = table.iter().find(|(k, _, _)| *k == key).copied();
        let Some((_, open_inner, close_name)) = tags else {
            continue;
        };
        // Remove the annotation.
        out.replace_range(start..end, "");
        // Walk backward in the truncated buffer for the immediately-preceding
        // occurrence of `target` (plain text — may incidentally match inside
        // an HTML attribute value, but the HTML tool ships with the same
        // limitation and we preserve byte-level parity).
        let head = &out[..start];
        if let Some(target_idx) = head.rfind(&target) {
            let replacement = format!("<{}>{}</{}>", open_inner, target, close_name);
            out.replace_range(target_idx..target_idx + target.len(), &replacement);
        }
    }
    out
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_shift_jis() {
        // "あ" in Shift-JIS = 0x82 0xA0
        let bytes = b"\x82\xA0";
        assert_eq!(decode_bytes(bytes), "あ");
    }

    #[test]
    fn decodes_utf8_with_bom() {
        let bytes = b"\xEF\xBB\xBFhello";
        assert_eq!(decode_bytes(bytes), "hello");
    }

    #[test]
    fn parses_minimal_doc() {
        let src = "タイトル\n著者\n\n-------\n本文行\n";
        let doc = parse_txt(src);
        assert_eq!(doc.title, "タイトル");
        assert_eq!(doc.author, "著者");
        assert!(doc.body_xhtml.contains("本文行"));
    }

    #[test]
    fn ruby_with_explicit_marker() {
        let mut imgs = Vec::new();
        let out = convert_aozora_line("当主｜旗太郎《はたたろう》です", &mut imgs);
        assert!(out.contains("<ruby>旗太郎<rp>（</rp><rt>はたたろう</rt>"));
    }

    #[test]
    fn ruby_on_bare_kanji_run() {
        let mut imgs = Vec::new();
        let out = convert_aozora_line("法水《のりみず》が来た", &mut imgs);
        assert!(out.contains("<ruby>法水<rp>（</rp><rt>のりみず</rt>"));
    }

    #[test]
    fn sesame_paired() {
        let mut imgs = Vec::new();
        let out = convert_aozora_line("これは［＃傍点］強調［＃傍点終わり］だ", &mut imgs);
        assert!(out.contains(r#"<em class="sesame">強調</em>"#));
    }

    #[test]
    fn postfix_bold_wraps_preceding() {
        let mut imgs = Vec::new();
        let out = convert_aozora_line("ABCDE［＃「BCD」は太字］", &mut imgs);
        assert!(out.contains("<strong>BCD</strong>"));
        assert!(!out.contains("［＃"));
    }

    #[test]
    fn gaiji_collapses_to_marker() {
        let mut imgs = Vec::new();
        let out = convert_aozora_line("※［＃感嘆符疑問符、1-8-78］あ", &mut imgs);
        assert!(out.starts_with("※"));
        assert!(!out.contains("［＃"));
    }

    #[test]
    fn image_ref_collected() {
        let mut imgs = Vec::new();
        let out = convert_aozora_line("見ろ［＃図1（fig01.png、横480×縦320）入る］ここ", &mut imgs);
        assert_eq!(imgs, vec!["fig01.png".to_string()]);
        assert!(out.contains(r#"<img src="../images/fig01.png" alt="図1"/>"#));
    }

    #[test]
    fn block_indent_state_machine() {
        let src = "タイトル\n著者\n\n-------\n通常\n［＃ここから１字下げ］\n字下げ中\n［＃ここで字下げ終わり］\n通常2\n";
        let doc = parse_txt(src);
        assert!(doc.body_xhtml.contains(r#"<p class="indent">字下げ中</p>"#));
        assert!(doc.body_xhtml.contains("<p>通常2</p>"));
    }

    #[test]
    fn heading_emits_toc_entry() {
        let src = "T\nA\n\n-------\n［＃大見出し］序章［＃大見出し終わり］\n本文\n";
        let doc = parse_txt(src);
        assert_eq!(doc.toc.len(), 1);
        assert_eq!(doc.toc[0].level, 2);
        assert_eq!(doc.toc[0].text, "序章");
        assert!(doc.body_xhtml.contains(r#"<h2 id="h1">序章</h2>"#));
    }

    #[test]
    fn colophon_extracted() {
        let src = "T\nA\n\n-------\n本文\n\n底本：「テスト」テスト社\n　　　1990年1月1日初版\n青空文庫作成ファイル：\nfoo\n";
        let doc = parse_txt(src);
        assert!(doc.colophon.contains("底本：「テスト」テスト社"));
        assert!(!doc.colophon.contains("青空文庫"));
        // Colophon is not in the body.
        assert!(!doc.body_xhtml.contains("底本"));
    }
}
