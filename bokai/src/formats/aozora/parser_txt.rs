//! Aozora Bunko `.txt` parser.

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
    for line in &lines[body_end..] {
        let cl = line.trim();
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
    // Close any box left open by malformed/missing `終わり` markers so the
    // body XHTML is always well-formed.
    close_open_boxes(&mut state);

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

/// A `字下げ` block: how far the first line of a paragraph is indented and how
/// far the lines it wraps onto are, both in characters.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Indent {
    first: u32,
    wrap: u32,
}

impl Indent {
    fn flat(n: u32) -> Self {
        Indent { first: n, wrap: n }
    }

    /// `class`/`style` attribute text for a `<p>` carrying this indent. The
    /// stylesheet's `.indent` already means one character, so the common case
    /// needs no inline metrics.
    fn attrs(&self, extra_classes: &[&str]) -> String {
        let mut classes = vec!["indent"];
        classes.extend_from_slice(extra_classes);
        let mut out = format!(r#" class="{}""#, classes.join(" "));
        if *self != Indent::flat(1) {
            out.push_str(&format!(r#" style="margin-top:{}em"#, self.wrap));
            if self.first != self.wrap {
                out.push_str(&format!(
                    ";text-indent:-{}em",
                    self.wrap.saturating_sub(self.first)
                ));
            }
            out.push('"');
        }
        out
    }
}

/// Work a line's own markers deferred to after that line has been written.
/// `［＃ここで…終わり］` closes a range *including* the line it sits on, so the
/// style has to survive long enough to reach that line's `<p>`.
#[derive(Default)]
struct PendingClose {
    block_style: bool,
    indent: bool,
    box_p: bool,
}

#[derive(Default)]
struct BodyState {
    indent: Option<Indent>,
    /// Open `［＃ここから…］` block styles, innermost last. `None` for a marker
    /// that carries no rendering (see [`block_style_class`]) — it still occupies
    /// a slot so that its `終わり` pops the right entry.
    block_styles: Vec<Option<&'static str>>,
    /// State of the currently-open `罫囲み` (ruled box). `None` when not inside
    box_first: Option<bool>,
    heading_id: u32,
    toc: Vec<TocEntry>,
    html: String,
    referenced_images: Vec<String>,
}

/// Class for a `［＃ここから…］` block style, or `None` when the marker names a
/// print-layout fact that reflowable vertical text renders as ordinary text.
fn block_style_class(name: &str) -> Option<&'static str> {
    match name {
        "ゴシック体" => Some("gothic"),
        "斜体" => Some("italic"),
        "罫囲み" | "枠囲み" => Some("keigakomi"),
        "破線罫囲み" | "破線枠囲み" => Some("keigakomi-dashed"),
        "二重罫囲み" | "二重枠囲み" => Some("keigakomi-double"),
        _ => None,
    }
}

/// A `罫囲み`/`枠囲み` (ruled box) class. The box is emitted as ONE `<p>` whose
fn is_box_class(cls: &str) -> bool {
    cls.starts_with("keigakomi")
}

/// Close the open box `<p>`, if any. Page breaks and headings reset block
/// state; an unterminated box must not leak across such a reset (or past the
/// end of the body).
fn close_open_boxes(state: &mut BodyState) {
    if state.box_first.take().is_some() {
        state.html.push_str("</p>\n");
    }
}

/// `［＃ここからN字下げ］`, optionally `、折り返してM字下げ］`, and the
/// `［＃ここから改行天付き、折り返してM字下げ］` form whose first line is flush.
static INDENT_START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"［＃ここから(?:([０-９0-9]+)字下げ|改行天付き)(?:、折り返して([０-９0-9]+)字下げ)?[^］]*］")
        .unwrap()
});
static INDENT_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃ここで字下げ終わり］").unwrap());
static BLOCK_START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"［＃ここから(横組み|ゴシック体|斜体|破線罫囲み|破線枠囲み|二重罫囲み|二重枠囲み|罫囲み|枠囲み)］").unwrap()
});
static BLOCK_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃ここで(横組み|ゴシック体|斜体|[^］]*囲み)終わり］").unwrap());
/// `字詰め` (characters-per-line) block markers, e.g. `［＃ここから３５字詰め］`
static JIZUME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"［＃ここから[０-９0-9]+字詰め］|［＃ここで字詰め終わり］").unwrap()
});
static PAGE_BREAK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃(改ページ|改丁|ページの左右中央)］").unwrap());
static PAGE_BREAK_LINE_ONLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^［＃(改ページ|改丁|ページの左右中央)］\s*$").unwrap());
static HEADING_PRECEDES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃[大中小]見出し］").unwrap());
/// Postfix heading marker: `［＃「TEXT」は<大|中|小>見出し］` makes the preceding
/// `TEXT` a heading. The quoted capture is greedy, so a nested `「…」` matches.
static POSTFIX_HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃「(.+)」は([大中小])見出し］").unwrap());
static HEADING_OOMIDASHI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃大見出し］(.+?)［＃大見出し終わり］").unwrap());
static HEADING_NAKAMIDASHI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃中見出し］(.+?)［＃中見出し終わり］").unwrap());
static HEADING_KOMIDASHI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃小見出し］(.+?)［＃小見出し終わり］").unwrap());
/// A one-line indent: `［＃N字下げ］`, or `［＃天からN字下げ］` measured from the
/// top of the column (identical in reflowable text, where there is nothing else
/// to measure from).
static INDENT_SINGLE_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃(?:天から)?([０-９0-9]+)字下げ］").unwrap());
static EDITORIAL_BASE_NOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"［＃「[^」]*」は底本では[^］]*］").unwrap());
/// Notes the input made about the 底本 rather than about setting. They address the
/// reader of the source file, so they carry no markup and are removed.
static EDITORIAL_NOTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"［＃(?:ルビの)?「[^」]*」は底本では[^］]*］|［＃(?:「[^」]*」は)?ママ］").unwrap()
});

fn process_line(raw_in: &str, state: &mut BodyState) {
    let pending = process_line_inner(raw_in, state);
    // Deferred so that the line carrying a `終わり` marker is itself still
    // inside the range that marker closes.
    if pending.box_p {
        close_open_boxes(state);
    }
    if pending.block_style {
        state.block_styles.pop();
    }
    if pending.indent {
        state.indent = None;
    }
}

fn process_line_inner(raw_in: &str, state: &mut BodyState) -> PendingClose {
    let mut raw = raw_in.to_string();
    let mut pending = PendingClose::default();

    // Block indent start (consume marker; rest of line continues).
    if let Some(caps) = INDENT_START_RE.captures(&raw) {
        // `改行天付き` has no first-line depth: the first line starts flush.
        let first = caps
            .get(1)
            .and_then(|m| parse_zenkaku_int(m.as_str()))
            .unwrap_or(0);
        let wrap = caps
            .get(2)
            .and_then(|m| parse_zenkaku_int(m.as_str()))
            .unwrap_or(first);
        state.indent = Some(Indent { first, wrap });
        raw = INDENT_START_RE.replace(&raw, "").to_string();
        if raw.trim().is_empty() {
            return pending;
        }
    }
    // Block indent end. Trailing content on this line is still indented; the
    // reset happens after it has been written. Not a stack — matches the
    // annotation, where `字下げ終わり` closes whatever is open.
    if INDENT_END_RE.is_match(&raw) {
        raw = INDENT_END_RE.replace(&raw, "").to_string();
        pending.indent = true;
        if raw.trim().is_empty() {
            return pending;
        }
    }

    // `字詰め` (chars-per-line) block markers: consume as no-ops so they don't
    // fall through to the paragraph fallback and emit an empty `<p>`.
    if JIZUME_RE.is_match(&raw) {
        raw = JIZUME_RE.replace_all(&raw, "").to_string();
        if raw.trim().is_empty() {
            return pending;
        }
    }

    // Block-level style start. The `罫囲み`/`枠囲み` family opens a single box
    // `<p>` (its lines joined by `<br/>`); the other styles push a per-paragraph
    // class. The box `<p>` carries `indent` when inside a `字下げ` block.
    if let Some(caps) = BLOCK_START_RE.captures(&raw) {
        let name = caps.get(1).unwrap().as_str();
        let cls = block_style_class(name);
        match cls {
            Some(cls) if is_box_class(cls) => {
                close_open_boxes(state); // no nesting; flush any prior box
                let attrs = match state.indent {
                    Some(indent) => indent.attrs(&[cls]),
                    None => format!(r#" class="{}""#, cls),
                };
                state.html.push_str(&format!("<p{}>", attrs));
                state.box_first = Some(true);
            }
            // A box owns its own `<p>`, so it is not on the per-paragraph
            // stack; every other marker takes a slot whether or not it renders.
            _ => state.block_styles.push(cls),
        }
        raw = BLOCK_START_RE.replace(&raw, "").to_string();
        if raw.trim().is_empty() {
            return pending;
        }
    }
    // Block-level style end. A box name (`…囲み`) closes the box `<p>`; anything
    // else releases a per-paragraph block style once this line is out.
    if let Some(caps) = BLOCK_END_RE.captures(&raw) {
        let name = caps.get(1).unwrap().as_str();
        if name.contains("囲み") {
            pending.box_p = true;
        } else {
            pending.block_style = true;
        }
        // The end marker is matched more loosely than the start: strip anything
        // shaped `［＃ここで...終わり］` so an unpaired name leaves no residue.
        static END_STRIP_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃ここで[^］]*終わり］").unwrap());
        raw = END_STRIP_RE.replace(&raw, "").to_string();
        if raw.trim().is_empty() {
            return pending;
        }
    }

    // Page / section breaks reset block state. If the whole line is just a
    // page-break marker, skip it; otherwise strip and continue.
    if PAGE_BREAK_RE.is_match(&raw) {
        state.indent = None;
        close_open_boxes(state);
        state.block_styles.clear();
        pending = PendingClose::default();
        if PAGE_BREAK_LINE_ONLY_RE.is_match(raw.trim()) {
            return pending;
        }
        raw = PAGE_BREAK_RE.replace_all(&raw, "").to_string();
    }

    // Heading lines reset block state.
    if HEADING_PRECEDES_RE.is_match(&raw) {
        state.indent = None;
        close_open_boxes(state);
        state.block_styles.clear();
        pending = PendingClose::default();
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
        state
            .html
            .push_str(&format!("<h2 id=\"{}\">{}</h2>\n", id, inner));
        return pending;
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
        state
            .html
            .push_str(&format!("<h3 id=\"{}\">{}</h3>\n", id, inner));
        return pending;
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
        return pending;
    }

    // One-line indent prefix `［＃N字下げ］`. It indents only its own line, so
    // it overrides — never accumulates onto — an enclosing `字下げ` block.
    let mut line_indent = state.indent;
    if let Some(caps) = INDENT_SINGLE_PREFIX_RE.captures(&raw)
        && let Some(n) = caps.get(1).and_then(|m| parse_zenkaku_int(m.as_str()))
    {
        line_indent = Some(Indent::flat(n));
    }
    raw = INDENT_SINGLE_PREFIX_RE.replace_all(&raw, "").to_string();

    // Postfix heading form: `TEXT［＃「TEXT」は<大|中|小>見出し］`. Detect
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
            return pending;
        }
        // Postfix annotation present but the preceding text doesn't match
        // the quoted target verbatim (rare — usually means the heading text
        // contains other markup we'd need to process first). Fall through;
    }

    // Inside a `罫囲み` box: append this line to the open box `<p>`, joining
    // successive lines with `<br/>` (the box is a single text block so Kindle
    // keeps every line on one page).
    if let Some(first) = state.box_first {
        let inner = if raw.trim().is_empty() {
            String::new()
        } else {
            convert_aozora_line(&raw, &mut state.referenced_images)
        };
        if !first {
            state.html.push_str("<br/>");
        }
        state.html.push_str(&inner);
        state.box_first = Some(false);
        return pending;
    }

    if raw.trim().is_empty() {
        state.html.push_str("<p><br/></p>\n");
        return pending;
    }

    let block_classes: Vec<&str> = state.block_styles.iter().flatten().copied().collect();
    let p_attr = match line_indent {
        Some(indent) => indent.attrs(&block_classes),
        None if block_classes.is_empty() => String::new(),
        None => format!(r#" class="{}""#, block_classes.join(" ")),
    };
    let inner = convert_aozora_line(&raw, &mut state.referenced_images);
    state
        .html
        .push_str(&format!("<p{}>{}</p>\n", p_attr, inner));
    pending
}

fn strip_editorial_notes_for_heading(s: &str) -> String {
    EDITORIAL_BASE_NOTE_RE.replace_all(s, "").to_string()
}

/// Plain-text version of a heading: ruby + explicit-marker `｜` removed.
/// Mirrors the JS `replace(/《[^》]*》/g, '').replace(/｜/g, '')`.
fn plain_text_for_heading(s: &str) -> String {
    static RUBY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"《[^》]*》").unwrap());
    RUBY_RE.replace_all(s, "").replace('｜', "")
}

// =========================================================================
// Inline annotation processing (convertAozoraLine)

/// Convert one line of Aozora source to inline XHTML: ruby, gaiji, images and
/// the inline annotations. Exposed for the parts of a document that are not
/// body lines but are still written in the same notation (the colophon).
pub fn convert_line(line: &str, images: &mut Vec<String>) -> String {
    convert_aozora_line(line, images)
}

fn convert_aozora_line(line: &str, images: &mut Vec<String>) -> String {
    // Per-line fast paths. Most lines in a typical Aozora book have *no*
    let has_anno = line.contains('［');
    let has_ruby = line.contains('《');
    let has_gaiji = line.contains('※');

    // escape_xml is also a hot path; it allocates only when needed.
    let mut s: Cow<str> = escape_xml_lazy(line);

    if has_anno {
        // Image refs: ［＃description（filename、dims）入る］
        static IMG_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃([^（］]*)（([^、]+)、[^）]*）入る］").unwrap());
        s = re_replace_cow(&IMG_RE, s, |caps| {
            let alt = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let filename = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            images.push(filename.to_string());
            format!(r#"<img src="../images/{}" alt="{}"/>"#, filename, alt)
        });
    }

    // Gaiji resolution runs *before* ruby: the annotation sits between the
    // character and its reading (`顳※［＃「需＋頁」、第3水準1-94-6］《こめかみ》`),
    // so the ruby base only becomes visible once `※［＃…］` has collapsed to 顬.
    if has_gaiji {
        static GAIJI_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"※［＃([^］]*)］").unwrap());
        s = re_replace_cow(&GAIJI_RE, s, |caps| {
            // No code in the annotation (a prose-only glyph description) means
            // nothing to resolve — `※` stays as the Aozora placeholder.
            super::gaiji::resolve(&caps[1])
                .map(|c| c.into_owned())
                .unwrap_or_else(|| "※".to_string())
        });
    }

    if has_ruby {
        s = apply_ruby(s);
    }

    if has_anno {
        // Editorial notes come out before anything reads a range. They record
        s = re_replace_str_cow(&EDITORIAL_NOTE_RE, s, "");

        // --- Block-form paired annotations ---
        for form in PAIRED_FORMS.iter() {
            s = re_replace_cow(&form.re, s, |caps| {
                format!("{}{}{}", form.open, &caps[1], form.close)
            });
        }

        // Batsu (ばつ or ×) bouten — two open/close forms.
        static BATSU_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃(?:ばつ|×)傍点］([^［]*)［＃(?:ばつ|×)傍点終わり］").unwrap()
        });
        s = re_replace_cow(&BATSU_RE, s, |caps| {
            format!(r#"<em class="batsu">{}</em>"#, &caps[1])
        });

        // 罫囲み / 枠囲み — two name forms map to the same class.
        static FRAME_PLAIN_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃(?:罫囲み|枠囲み)］([^［]*)［＃(?:罫囲み|枠囲み)終わり］").unwrap()
        });
        s = re_replace_cow(&FRAME_PLAIN_RE, s, |caps| {
            format!(r#"<span class="keigakomi">{}</span>"#, &caps[1])
        });
        static FRAME_DASHED_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃破線(?:罫囲み|枠囲み)］([^［]*)［＃破線(?:罫囲み|枠囲み)終わり］")
                .unwrap()
        });
        s = re_replace_cow(&FRAME_DASHED_RE, s, |caps| {
            format!(r#"<span class="keigakomi-dashed">{}</span>"#, &caps[1])
        });
        static FRAME_DOUBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃二重(?:罫囲み|枠囲み)］([^［]*)［＃二重(?:罫囲み|枠囲み)終わり］")
                .unwrap()
        });
        s = re_replace_cow(&FRAME_DOUBLE_RE, s, |caps| {
            format!(r#"<span class="keigakomi-double">{}</span>"#, &caps[1])
        });

        // 返り点 (kanbun reading marks)
        static KAERITEN_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃(返り点)］([^［]*)［＃\u{8FD4}\u{308A}\u{70B9}終わり］").unwrap()
        });
        s = re_replace_cow(&KAERITEN_RE, s, |caps| {
            format!(r#"<sup class="kaeriten">{}</sup>"#, &caps[2])
        });

        // Font size: ［＃N段階大きな文字］...
        static BIGGER_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃([０-９0-9]+)段階大きな文字］([^［]*)［＃大きな文字終わり］").unwrap()
        });
        s = re_replace_cow(&BIGGER_RE, s, |caps| {
            let n = parse_zenkaku_int(&caps[1]).unwrap_or(1);
            let em = 1.0 + n as f32 * 0.2;
            format!(r#"<span style="font-size:{}em">{}</span>"#, em, &caps[2])
        });
        static SMALLER_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"［＃([０-９0-9]+)段階小さな文字］([^［]*)［＃小さな文字終わり］").unwrap()
        });
        s = re_replace_cow(&SMALLER_RE, s, |caps| {
            let n = parse_zenkaku_int(&caps[1]).unwrap_or(1);
            let em = (1.0 - n as f32 * 0.1).max(0.6);
            format!(r#"<span style="font-size:{}em">{}</span>"#, em, &caps[2])
        });

        // Postfix annotations: ［＃「text」にXXX］
        if s.contains('「') {
            let owned = apply_postfix_annotations(&s);
            if owned.len() != s.len() || owned != *s {
                s = Cow::Owned(owned);
            }
        }

        // 割り注 — a two-line note set inline. The marker states the *setting*;
        static WARICHU_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃割り注］([^［]*)［＃割り注終わり］").unwrap());
        s = re_replace_cow(&WARICHU_RE, s, |caps| {
            format!(r#"<span class="warichu">{}</span>"#, &caps[1])
        });

        // Strip editorial notes and heading-reference notes.
        s = re_replace_str_cow(&EDITORIAL_BASE_NOTE_RE, s, "");
        // Greedy `.*` mirrors POSTFIX_HEADING_RE, so a heading-ref marker whose quoted
        // text nests `「…」` is stripped by this pass and not left to the catch-all.
        static HEADING_REF_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃「.*」は[大中小]見出し］").unwrap());
        s = re_replace_str_cow(&HEADING_REF_RE, s, "");

        // 地付き / 地からN字上げ — a signature (e.g. 坂口安吾) set to the bottom
        static JIAGE_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃地から[０-９0-9]+字上げ］(.*)$").unwrap());
        s = re_replace_cow(&JIAGE_RE, s, |caps| {
            format!(r#"<br/><span class="chitsuki">{}</span>"#, &caps[1])
        });
        static CHITSUKI_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃地付き］(.*)$").unwrap());
        s = re_replace_cow(&CHITSUKI_RE, s, |caps| {
            format!(r#"<br/><span class="chitsuki">{}</span>"#, &caps[1])
        });

        // Final catch-all: drop any remaining ［＃...］.
        static REMAINING_ANN_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"［＃[^］]*］").unwrap());
        s = re_replace_str_cow(&REMAINING_ANN_RE, s, "");
    }

    s.into_owned()
}

// =========================================================================

/// The character classes an unmarked ruby base is allowed to span.
#[derive(PartialEq, Eq, Clone, Copy)]
enum RubyClass {
    Kanji,
    Hiragana,
    Katakana,
    Latin,
    /// Anything else — a Greek letter, ℵ, an operator. Ruby still attaches, but
    /// only to that single character: there is no run to extend over, and
    /// letting punctuation accumulate would swallow the sentence before it.
    Other,
}

fn ruby_class(c: char) -> RubyClass {
    match c {
        '\u{3005}' | '\u{3006}' | '\u{3007}' => RubyClass::Kanji, // 々〆〇
        '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2A6DF}'
        | '\u{2A700}'..='\u{2EBEF}'
        | '\u{2F800}'..='\u{2FA1F}' => RubyClass::Kanji,
        '\u{3041}'..='\u{309F}' => RubyClass::Hiragana,
        '\u{30A1}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9F}' => {
            RubyClass::Katakana
        }
        'A'..='Z' | 'a'..='z' | '0'..='9' => RubyClass::Latin,
        '\u{FF10}'..='\u{FF19}' | '\u{FF21}'..='\u{FF3A}' | '\u{FF41}'..='\u{FF5A}' => {
            RubyClass::Latin
        }
        _ => RubyClass::Other,
    }
}

/// Replace every `《reading》` with a `<ruby>` over the base that precedes it.
fn apply_ruby(s: Cow<'_, str>) -> Cow<'_, str> {
    if !s.contains('《') {
        return s;
    }
    let src: &str = &s;
    let mut out = String::with_capacity(src.len() + 64);
    // Byte index in `src` up to which `out` has been written.
    let mut copied = 0usize;
    let mut search = 0usize;

    while let Some(rel) = src[search..].find('《') {
        let open = search + rel;
        let Some(rel_close) = src[open..].find('》') else {
            break;
        };
        let close = open + rel_close;
        let reading = &src[open + '《'.len_utf8()..close];
        search = close + '》'.len_utf8();

        // An empty `《》` is the legend for the notation, not a reading.
        if reading.is_empty() {
            continue;
        }
        // The base has to lie in text this pass has not already consumed.
        let Some(base_start) = ruby_base_start(&src[copied..open]).map(|i| copied + i) else {
            continue;
        };
        out.push_str(&src[copied..base_start]);
        let base = &src[base_start..open];
        // `｜` only delimits the base; it never reaches the reader.
        let base = base.strip_prefix('｜').unwrap_or(base);
        if base.contains('<') {
            out.push_str(&fold_reading_into_alt(base, reading));
        } else {
            out.push_str("<ruby>");
            out.push_str(base);
            out.push_str("<rp>（</rp><rt>");
            out.push_str(reading);
            out.push_str("</rt><rp>）</rp></ruby>");
        }
        copied = search;
    }

    if copied == 0 {
        return s;
    }
    out.push_str(&src[copied..]);
    Cow::Owned(out)
}

/// Record a reading on the element that would have been its ruby base, by
fn fold_reading_into_alt(base: &str, reading: &str) -> String {
    let Some(alt_start) = base.find(r#" alt=""#).map(|i| i + r#" alt=""#.len()) else {
        return base.to_string();
    };
    let Some(alt_len) = base[alt_start..].find('"') else {
        return base.to_string();
    };
    let alt = &base[alt_start..alt_start + alt_len];
    if alt.contains(reading) {
        return base.to_string();
    }
    format!(
        "{}{}（{}）{}",
        &base[..alt_start],
        alt,
        reading,
        &base[alt_start + alt_len..]
    )
}

/// Byte offset within `head` where the ruby base for a `《` at its end begins.
fn ruby_base_start(head: &str) -> Option<usize> {
    // Explicit marker anywhere in the available text wins: `｜base《reading》`.
    if let Some(i) = head.rfind('｜') {
        return Some(i);
    }
    let last = head.chars().next_back()?;
    // An element ends here (`<img …/>`, `</ruby>`). Text-level `<` and `>` are
    // already escaped, so the nearest `<` is unambiguously this tag's start.
    if last == '>' {
        return head.rfind('<');
    }
    // 〔…〕 wraps accent-decomposed Western text and rubies as one unit.
    if last == '〕' {
        return head.rfind('〔');
    }
    let class = ruby_class(last);
    if class == RubyClass::Other {
        return Some(head.len() - last.len_utf8());
    }
    let mut start = head.len();
    for c in head.chars().rev() {
        if ruby_class(c) != class {
            break;
        }
        start -= c.len_utf8();
    }
    Some(start)
}

/// Borrow-first XML escape. Returns the input unchanged when it contains
/// no XML-meta characters — the common case for Japanese body text.
fn escape_xml_lazy(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Wrap `Regex::replace_all` so the result threads through a `Cow<str>`.
fn re_replace_cow<'a>(
    re: &Regex,
    s: Cow<'a, str>,
    mut f: impl FnMut(&Captures<'_>) -> String,
) -> Cow<'a, str> {
    // We can't pass `&s` directly into `replace_all` because the returned
    // `Cow` would borrow from the temporary. Match on the underlying
    // `&str` and decide ownership ourselves.
    let borrowed: &str = &s;
    match re.replace_all(borrowed, |caps: &Captures<'_>| f(caps)) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    }
}

/// Same as [`re_replace_cow`] but for a fixed replacement string (no closure).
fn re_replace_str_cow<'a>(re: &Regex, s: Cow<'a, str>, rep: &str) -> Cow<'a, str> {
    let borrowed: &str = &s;
    match re.replace_all(borrowed, rep) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    }
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
        paired_form(
            "二重取消線",
            r#"<span class="strikethrough-double">"#,
            "</span>",
        ),
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
        // `横組み` deliberately absent — see `block_style_class`. The catch-all
        // drops the markers and the run stays in the vertical flow.
        paired_form("上付き小文字", "<sup>", "</sup>"),
        paired_form("下付き小文字", "<sub>", "</sub>"),
        paired_form("行右小書き", "<sup>", "</sup>"),
        paired_form("行左小書き", "<sub>", "</sub>"),
    ]
});

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
    keys.sort_by_key(|b| std::cmp::Reverse(b.len()));
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
            (m.start(), m.end(), caps[1].to_string(), caps[2].to_string())
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
        let head = &out[..start];
        if let Some(target_idx) = head.rfind(&target) {
            let replacement = format!("<{}>{}</{}>", open_inner, target, close_name);
            out.replace_range(target_idx..target_idx + target.len(), &replacement);
        }
    }
    out
}

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
    fn gaiji_resolves_to_the_character_it_names() {
        // The annotation names the character exactly; `※` is a placeholder for
        // a reader with no table, not the text.
        let mut imgs = Vec::new();
        let out = convert_aozora_line("※［＃感嘆符疑問符、1-8-78］あ", &mut imgs);
        assert_eq!(out, "\u{2049}あ");
        let out = convert_aozora_line("顳※［＃「需＋頁」、第3水準1-94-6］", &mut imgs);
        assert_eq!(out, "顳顬");
        // A glyph described only in prose has nothing to resolve to.
        let out = convert_aozora_line("※［＃「くさかんむり／夷」］", &mut imgs);
        assert_eq!(out, "※");
    }

    #[test]
    fn gaiji_resolves_before_ruby_reads_its_base() {
        // The annotation sits between the character and its reading, so a ruby
        // pass running first saw no base at all and left `《…》` in the text.
        let mut imgs = Vec::new();
        let out = convert_aozora_line(
            "両側の顳※［＃「需＋頁」、第3水準1-94-6］《こめかみ》に",
            &mut imgs,
        );
        assert!(
            out.contains("<ruby>顳顬<rp>（</rp><rt>こめかみ</rt>"),
            "got: {out}"
        );
        assert!(!out.contains('《'), "got: {out}");
    }

    #[test]
    fn ruby_attaches_to_latin_and_iteration_marks() {
        // Aozora only requires the `｜` marker when the base does not start on a
        // character-class boundary. A kanji-only base rule dropped every
        // Western word's reading and every base ending in an iteration mark.
        let mut imgs = Vec::new();
        let out = convert_aozora_line("Quean《クイーン》 locked《ロックト》", &mut imgs);
        assert!(
            out.contains("<ruby>Quean<rp>（</rp><rt>クイーン</rt>"),
            "got: {out}"
        );
        assert!(
            out.contains("<ruby>locked<rp>（</rp><rt>ロックト</rt>"),
            "got: {out}"
        );
        let out = convert_aozora_line("聖鐘の殷々《いんいん》たる", &mut imgs);
        assert!(
            out.contains("<ruby>殷々<rp>（</rp><rt>いんいん</rt>"),
            "got: {out}"
        );
        let out = convert_aozora_line("ＰＡＴＥＲ《パテル》", &mut imgs);
        assert!(
            out.contains("<ruby>ＰＡＴＥＲ<rp>（</rp><rt>パテル</rt>"),
            "got: {out}"
        );
    }

    #[test]
    fn a_reading_on_a_glyph_image_lands_in_its_description() {
        // A glyph with no character to stand for it arrives as an image. Ruby
        // cannot ride on an image through KFX, so the reading is recorded on
        // the image rather than emitted as markup that only one output keeps.
        let mut imgs = Vec::new();
        let out = convert_aozora_line(
            "ヘブライ文字の［＃ヘブライ文字「YOD」（fig24.png、横15×縦23）入る］《ヨッド》まで",
            &mut imgs,
        );
        assert_eq!(
            out,
            r#"ヘブライ文字の<img src="../images/fig24.png" alt="ヘブライ文字「YOD」（ヨッド）"/>まで"#
        );
        // When the description already names the reading, nothing is appended.
        let out = convert_aozora_line(
            "呪語の［＃底本が「ラン」とルビを付した梵字（fig15.png、横18×縦23）入る］《ラン》の字",
            &mut imgs,
        );
        assert_eq!(
            out,
            r#"呪語の<img src="../images/fig15.png" alt="底本が「ラン」とルビを付した梵字"/>の字"#
        );
    }

    #[test]
    fn warichu_keeps_the_source_delimiters() {
        // The marker states that the note is set inline in two lines; the
        // parentheses around it are the 底本's own text and are already there.
        let mut imgs = Vec::new();
        let out = convert_aozora_line(
            "人形（［＃割り注］土耳古の操人形［＃割り注終わり］）を",
            &mut imgs,
        );
        assert_eq!(
            out,
            r#"人形（<span class="warichu">土耳古の操人形</span>）を"#
        );
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
    fn indent_depth_is_carried_not_flattened() {
        // Each depth carries its own margin, and a one-line `［＃N字下げ］`
        // indents that one line.
        let src = "T\nA\n\n-------\n［＃ここから４字下げ］\n深い\n［＃ここで字下げ終わり］\n［＃５字下げ］一行だけ\n［＃天から２字下げ］天から\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(
            body.contains(r#"<p class="indent" style="margin-top:4em">深い</p>"#),
            "body:\n{body}"
        );
        assert!(
            body.contains(r#"<p class="indent" style="margin-top:5em">一行だけ</p>"#),
            "body:\n{body}"
        );
        assert!(
            body.contains(r#"<p class="indent" style="margin-top:2em">天から</p>"#),
            "body:\n{body}"
        );
    }

    #[test]
    fn hanging_indent_is_measured_back_from_the_wrap_depth() {
        let src = "T\nA\n\n-------\n［＃ここから１字下げ、折り返して４字下げ］\n本文\n［＃ここで字下げ終わり］\n［＃ここから改行天付き、折り返して１字下げ］\n天付き\n［＃ここで字下げ終わり］\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(
            body.contains(r#"<p class="indent" style="margin-top:4em;text-indent:-3em">本文</p>"#),
            "body:\n{body}"
        );
        assert!(
            body.contains(
                r#"<p class="indent" style="margin-top:1em;text-indent:-1em">天付き</p>"#
            ),
            "body:\n{body}"
        );
    }

    #[test]
    fn block_end_marker_still_covers_its_own_line() {
        // `［＃ここで…終わり］` closes a range that *includes* the line it sits
        // on. Popping the style before writing that line dropped it from the
        // one paragraph a single-line block consists of.
        let src =
            "T\nA\n\n-------\n［＃ここからゴシック体］\n本文［＃ここでゴシック体終わり］\n後\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(
            body.contains(r#"<p class="gothic">本文</p>"#),
            "body:\n{body}"
        );
        assert!(body.contains("<p>後</p>"), "body:\n{body}");
    }

    #[test]
    fn yokogumi_stays_in_the_vertical_flow() {
        // The marker records that the 底本 set the run horizontally. Carrying
        let src = "T\nA\n\n-------\n式は［＃横組み］犯人＋Ｘ［＃横組み終わり］だ\n［＃ここから横組み］\n段落［＃ここで横組み終わり］\n後\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(!body.contains("yokogumi"), "body:\n{body}");
        assert!(!body.contains("横組み"), "body:\n{body}");
        assert!(body.contains("<p>式は犯人＋Ｘだ</p>"), "body:\n{body}");
        assert!(body.contains("<p>段落</p>"), "body:\n{body}");
        assert!(body.contains("<p>後</p>"), "body:\n{body}");
    }

    #[test]
    fn a_styleless_block_marker_does_not_pop_an_enclosing_style() {
        // 横組み renders as nothing, but its `終わり` must still release its own
        // slot rather than the style opened around it.
        let src = "T\nA\n\n-------\n［＃ここからゴシック体］\n［＃ここから横組み］\n中［＃ここで横組み終わり］\nまだゴシック\n［＃ここでゴシック体終わり］\n後\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(
            body.contains(r#"<p class="gothic">中</p>"#),
            "body:\n{body}"
        );
        assert!(
            body.contains(r#"<p class="gothic">まだゴシック</p>"#),
            "body:\n{body}"
        );
        assert!(body.contains("<p>後</p>"), "body:\n{body}");
    }

    #[test]
    fn keigakomi_block_is_one_paragraph_with_breaks() {
        // A 罫囲み (ruled box) block is ONE `<p>` whose lines are joined by
        let src = "T\nA\n\n-------\n前\n［＃ここから罫囲み］\n一行目\n二行目\n三行目\n［＃ここで罫囲み終わり］\n後\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        // Exactly one box <p>; lines joined by <br/>; no per-line boxes, no div.
        assert!(
            body.contains(r#"<p class="keigakomi">一行目<br/>二行目<br/>三行目</p>"#),
            "body:\n{body}"
        );
        assert_eq!(
            body.matches(r#"class="keigakomi""#).count(),
            1,
            "body:\n{body}"
        );
        assert!(
            !body.contains("<div"),
            "box must not be a div; body:\n{body}"
        );
    }

    #[test]
    fn jizume_markers_leave_no_empty_paragraph() {
        // `字詰め` (chars-per-line) has no reflowable equivalent; the markers
        // must be consumed, not left to emit a stray empty <p>.
        let src =
            "T\nA\n\n-------\n前\n［＃ここから３５字詰め］\n本文\n［＃ここで字詰め終わり］\n後\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(!body.contains("<p></p>"), "no empty <p>; body:\n{body}");
        assert!(
            !body.contains(r#"<p class="indent"></p>"#),
            "no empty indent <p>; body:\n{body}"
        );
        assert!(
            !body.contains("字詰"),
            "marker text stripped; body:\n{body}"
        );
        assert!(body.contains("<p>本文</p>"), "body:\n{body}");
    }

    #[test]
    fn box_end_marker_keeps_its_own_line_in_the_box() {
        // Same shape as `block_end_marker_still_covers_its_own_line`: the last
        // line of a box may carry the `終わり` marker, and it belongs inside.
        let src =
            "T\nA\n\n-------\n［＃ここから罫囲み］\n一行目\n二行目［＃ここで罫囲み終わり］\n後\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        assert!(
            body.contains(r#"<p class="keigakomi">一行目<br/>二行目</p>"#),
            "body:\n{body}"
        );
        assert!(body.contains("<p>後</p>"), "body:\n{body}");
    }

    #[test]
    fn keigakomi_box_closes_at_chapter_heading() {
        // An unterminated box must not leak across a heading reset (which clears
        // block state) — the box `<p>` is force-closed so the XHTML stays
        // balanced and the heading is emitted outside the box.
        let src = "T\nA\n\n-------\n［＃ここから罫囲み］\n箱の中\n［＃中見出し］次章［＃中見出し終わり］\n本文\n";
        let doc = parse_txt(src);
        let body = &doc.body_xhtml;
        let box_open = body.find(r#"<p class="keigakomi">"#).expect("box opened");
        let box_close = body[box_open..]
            .find("</p>")
            .map(|i| box_open + i)
            .expect("box closed");
        let h_start = body.find("<h3").expect("heading present");
        assert!(
            box_close < h_start,
            "heading after box close; body:\n{body}"
        );
        assert!(
            !body[box_open..box_close].contains("<h3"),
            "heading not inside box; body:\n{body}"
        );
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

    #[test]
    fn postfix_heading_with_nested_quotes() {
        // 坂口安吾『不連続殺人事件』ch.22: the heading text itself contains a
        // nested 「…」. The greedy quoted capture must still detect it as a
        // 中見出し (→ <h3>) and emit a TOC entry, not drop it to a plain <p>.
        let src = "T\nA\n\n-------\n［＃５字下げ］二十二　「八月九日　宿命の日」［＃「二十二　「八月九日　宿命の日」」は中見出し］\n本文\n";
        let doc = parse_txt(src);
        assert_eq!(
            doc.toc.len(),
            1,
            "nested-quote heading must produce a TOC entry"
        );
        assert_eq!(doc.toc[0].level, 3);
        assert_eq!(doc.toc[0].text, "二十二　「八月九日　宿命の日」");
        assert!(
            doc.body_xhtml
                .contains(r#"<h3 id="h1">二十二　「八月九日　宿命の日」</h3>"#),
            "expected <h3> heading, got body:\n{}",
            doc.body_xhtml
        );
    }
}
