//! Books that isolate one layout question each.
//!
//! A [`Probe`] holds deterministic text and at most one declaration. One with
//! no declaration is the control its pair is read against.
//!
//! [`Probe::document`] is XHTML with an inline stylesheet, which
//! [`Probe::book`] hands to bokai's own importer and KFX exporter.

use std::io;
use std::path::{Path, PathBuf};

use bokai::formats::kfx::container_edit::{EntityEdit, edit_container};
use bokai::formats::kfx::ion::IonValue;
use bokai::formats::kfx::symbols::KfxSymbol;
use bokai::import::{AssetInfo, ChapterId, Importer, SpineEntry};
use bokai::model::{AnchorTarget, Book, Format, Landmark, Metadata, TocEntry};

use crate::settings::{Direction, Panel, Script, Settings};

/// One word repeated. Every line holds the same glyphs in the same order.
pub const MEASURED_LATIN: &str = "nnnn nnnn nnnn nnnn nnnn nnnn nnnn nnnn nnnn nnnn \
nnnn nnnn nnnn nnnn nnnn nnnn nnnn nnnn nnnn nnnn";

/// Latin prose: mixed widths, kerning pairs, and words long enough to
/// hyphenate.
pub const PROSE_LATIN: &str = "Typography considered independently of \
communication is unimaginable, and a paragraph long enough to break across \
several lines is the smallest thing that shows how a renderer breaks them.";

/// Prose whose every word is short enough that no line break falls inside
/// one.
pub const SHORT_WORDS_LATIN: &str = "The type on a page is set in a line, and \
a line is set to fit. A word too wide for what is left of a line goes to the \
next one, and the line before it is spaced out to fill.";

/// The same, set in Japanese. No spaces: every break is the line breaker's.
pub const PROSE_JAPANESE: &str = "組版は伝達と切り離しては考えられないものであり、数行にわたって折り返される\
段落こそ、行の分割のしかたを示す最小の単位である。";

/// One probe book.
#[derive(Debug, Clone)]
pub struct Probe {
    /// Names the file and the question. One property at one value.
    pub name: String,
    /// The declaration under test, or `None` for a control.
    pub declared: Option<(String, String)>,
    /// The selector the declaration is set on.
    pub selector: String,
    /// The element each paragraph is written as. `p` takes `margin: 1em 0`
    /// from the default stylesheet; `div` takes nothing.
    pub element: String,
    pub language: String,
    /// Whether the book states that it reads down the page.
    pub vertical: bool,
    /// Rules appended after the declaration under test, for a probe that
    /// needs more than one block styled.
    pub styling: String,
    /// KFX properties written into every style fragment after export. The
    /// style schema maps no CSS to some of the vocabulary, and this reaches
    /// those.
    pub injected: Vec<(u64, IonValue)>,
    /// KFX properties written into every storyline element after export.
    pub injected_elements: Vec<(u64, IonValue)>,
    /// Whether to stretch each element's first style event over the rest of
    /// them, making the ranges contain one another. The exporter's own
    /// flattening emits them side by side.
    pub containing_events: bool,
    /// The paragraphs, in order.
    pub paragraphs: Vec<String>,
    /// Whether each paragraph is markup [`Probe::document`] passes through.
    pub markup: bool,
    /// Pictures the book carries, as `(name, width, height)`. Each is encoded
    /// as a flat grey PNG of that size.
    pub images: Vec<(String, u32, u32)>,
}

impl Probe {
    /// A control: paragraphs and nothing else declared.
    pub fn new(name: impl Into<String>, paragraphs: &[&str]) -> Self {
        Self {
            name: name.into(),
            declared: None,
            selector: "p".to_string(),
            element: "p".to_string(),
            language: "en".to_string(),
            vertical: false,
            styling: String::new(),
            injected: Vec::new(),
            injected_elements: Vec::new(),
            containing_events: false,
            markup: false,
            images: Vec::new(),
            paragraphs: paragraphs.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    /// The same book with one property declared on it.
    pub fn declaring(mut self, property: impl Into<String>, value: impl Into<String>) -> Self {
        self.declared = Some((property.into(), value.into()));
        self
    }

    /// Which elements the declaration applies to.
    pub fn on(mut self, selector: impl Into<String>) -> Self {
        self.selector = selector.into();
        self
    }

    /// Write each paragraph as `element`, and set the declaration on it.
    pub fn as_element(mut self, element: impl Into<String>) -> Self {
        let element = element.into();
        self.selector.clone_from(&element);
        self.element = element;
        self
    }

    pub fn in_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    /// Write a KFX property into every style fragment after export.
    pub fn injecting(mut self, symbol: KfxSymbol, value: IonValue) -> Self {
        self.injected.push((symbol as u64, value));
        self
    }

    /// Write a KFX property into every storyline element after export.
    pub fn injecting_element(mut self, symbol: KfxSymbol, value: IonValue) -> Self {
        self.injected_elements.push((symbol as u64, value));
        self
    }

    /// Stretch each element's first style event over the rest of them.
    pub fn containing_events(mut self) -> Self {
        self.containing_events = true;
        self
    }

    /// Append rules of the caller's own, on top of the declaration.
    pub fn styled(mut self, css: impl Into<String>) -> Self {
        self.styling = css.into();
        self
    }

    /// Carry a picture of `width` by `height` pixels, named `name.png`.
    pub fn with_image(mut self, name: impl Into<String>, width: u32, height: u32) -> Self {
        self.images.push((name.into(), width, height));
        self
    }

    /// Pass each paragraph through as markup.
    pub fn as_markup(mut self) -> Self {
        self.markup = true;
        self
    }

    /// State that the book reads down the page.
    pub fn vertical(mut self) -> Self {
        self.vertical = true;
        self
    }

    /// The name the file takes, and the book's title.
    pub fn file_stem(&self) -> String {
        self.name.replace([' ', '/', ':'], "-")
    }

    /// The document, as the importer will be handed it.
    ///
    /// Each paragraph carries a `p0`, `p1`, … class, which a rule in
    /// [`Probe::styling`] selects to give one block a different declaration
    /// from its neighbour.
    pub fn document(&self) -> String {
        let mut css = String::new();
        if self.vertical {
            css.push_str("html { writing-mode: vertical-rl; }\n");
        }
        if let Some((property, value)) = &self.declared {
            css.push_str(&format!("{} {{ {property}: {value}; }}\n", self.selector));
        }
        css.push_str(&self.styling);
        let body: String = self
            .paragraphs
            .iter()
            .enumerate()
            .map(|(n, text)| {
                let body = if self.markup {
                    text.clone()
                } else {
                    escape(text)
                };
                format!("<{e} class=\"p{n}\">{body}</{e}>\n", e = self.element)
            })
            .collect();

        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"{lang}\">\n\
             <head><title>{title}</title><style>\n{css}</style></head>\n\
             <body>\n{body}</body>\n</html>\n",
            lang = self.language,
            title = escape(&self.name),
        )
    }

    /// The book this probe is, ready to export.
    pub fn book(&self) -> Book {
        Book::from_importer(Box::new(Source::new(self)))
    }

    /// The probe as a KFX container, with [`Probe::injected`] added to every
    /// style fragment.
    pub fn kfx(&self) -> io::Result<Vec<u8>> {
        let mut out = io::Cursor::new(Vec::new());
        self.book().export(Format::Kfx, &mut out)?;
        let bytes = out.into_inner();
        if self.injected.is_empty() && self.injected_elements.is_empty() && !self.containing_events
        {
            return Ok(bytes);
        }
        edit_container(&bytes, |entity| {
            if entity.is_type(KfxSymbol::Style) && !self.injected.is_empty() {
                let IonValue::Struct(mut fields) = entity.parse_ion()? else {
                    return Ok(EntityEdit::Keep);
                };
                set_all(&mut fields, &self.injected);
                return Ok(EntityEdit::Ion(IonValue::Struct(fields)));
            }
            if entity.is_type(KfxSymbol::Storyline)
                && (!self.injected_elements.is_empty() || self.containing_events)
            {
                let mut ion = entity.parse_ion()?;
                if !self.injected_elements.is_empty() {
                    stamp_elements(&mut ion, &self.injected_elements);
                }
                if self.containing_events {
                    stretch_first_event(&mut ion);
                }
                return Ok(EntityEdit::Ion(ion));
            }
            Ok(EntityEdit::Keep)
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }

    /// Write the probe as KFX into `dir`, returning the path written.
    pub fn write_kfx(&self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.kfx", self.file_stem()));
        std::fs::write(&path, self.kfx()?)?;
        Ok(path)
    }

    /// A request script: open `uri` at these settings, draw `pages` pages,
    /// close it. `uri` is the path the renderer opens, which the caller that
    /// runs it decides.
    ///
    /// Every parameter `open_uri` accepts is sent: an absent key ends the
    /// process, an empty value does not. `font_family` empty selects the face
    /// for the book's own script.
    pub fn script(&self, uri: &str, panel: &Panel, settings: &Settings, pages: usize) -> String {
        let direction = if self.vertical {
            Direction::Vertical
        } else {
            Direction::Horizontal
        };
        let margins = settings.margins(panel, direction);
        let mut script = format!(
            "/command/open_uri?uri={uri}\
             &pid=&guid=&position=&containers=&vouchers=&skip_rendering=0\
             &width={width}&height={height}\
             &top_margin={top}&right_margin={right}&bottom_margin={bottom}&left_margin={left}\
             &font_family=&font_size={size:.6}&line_spacing={spacing:.6}\
             &justification={justification}&embolden_weight={weight}\
             &enable_automatic_columns={columns}\
             &reserve_ruby_text_width=0&reserve_ruby_text_gap=0\n",
            width = panel.size.width as u32,
            height = panel.size.height as u32,
            top = margins.top as i32,
            right = margins.right as i32,
            bottom = margins.bottom as i32,
            left = margins.left as i32,
            size = settings.font_size_pt(panel, Script::of(&self.language)),
            spacing = settings.line_spacing(panel, &self.language),
            justification = u8::from(settings.justified),
            weight = settings.embolden_weight(panel) as u32,
            columns = u8::from(settings.columns(panel) > 1),
        );
        script.push_str("/command/draw_page\n");
        for _ in 1..pages.max(1) {
            script.push_str("/command/next_page\n/command/draw_page\n");
        }
        script.push_str("/command/close_uri\n");
        script
    }
}

/// A document held in memory, served to bokai's own HTML pipeline.
struct Source {
    metadata: Metadata,
    spine: Vec<SpineEntry>,
    toc: Vec<TocEntry>,
    landmarks: Vec<Landmark>,
    assets: Vec<PathBuf>,
    document: String,
    href: String,
    images: Vec<(String, u32, u32)>,
}

impl Source {
    fn new(probe: &Probe) -> Self {
        let document = probe.document();
        let href = format!("{}.xhtml", probe.file_stem());
        let metadata = Metadata {
            title: probe.name.clone(),
            language: probe.language.clone(),
            identifier: format!("probe:{}", probe.file_stem()),
            page_progression_direction: probe.vertical.then(|| "rtl".to_string()),
            primary_writing_mode: probe.vertical.then(|| "vertical-rl".to_string()),
            ..Metadata::default()
        };
        Self {
            metadata,
            spine: vec![SpineEntry {
                id: ChapterId(0),
                size_estimate: document.len(),
                page_spread: None,
                viewport: None,
                panels: Vec::new(),
            }],
            toc: vec![TocEntry {
                title: probe.name.clone(),
                href: href.clone(),
                children: Vec::new(),
                play_order: Some(1),
                target: None,
            }],
            landmarks: Vec::new(),
            assets: probe
                .images
                .iter()
                .map(|(name, _, _)| PathBuf::from(name))
                .collect(),
            document,
            href,
            images: probe.images.clone(),
        }
    }
}

/// Set each `(symbol, value)` on `fields`, replacing one of the same symbol.
fn set_all(fields: &mut Vec<(u64, IonValue)>, properties: &[(u64, IonValue)]) {
    for (symbol, value) in properties {
        fields.retain(|(s, _)| s != symbol);
        fields.push((*symbol, value.clone()));
    }
}

/// Set `properties` on every struct under `ion` that carries a `type`, which
/// is what a storyline's content elements do.
fn stamp_elements(ion: &mut IonValue, properties: &[(u64, IonValue)]) {
    match ion {
        IonValue::Struct(fields) => {
            let is_element = fields.iter().any(|(s, _)| *s == KfxSymbol::Type as u64);
            for (_, value) in fields.iter_mut() {
                stamp_elements(value, properties);
            }
            if is_element {
                set_all(fields, properties);
            }
        }
        IonValue::List(items) => {
            for item in items {
                stamp_elements(item, properties);
            }
        }
        _ => {}
    }
}

/// Stretch the first entry of every `style_events` list to the end of the last
/// entry, so the first range contains the others.
fn stretch_first_event(ion: &mut IonValue) {
    match ion {
        IonValue::Struct(fields) => {
            for (symbol, value) in fields.iter_mut() {
                if *symbol == KfxSymbol::StyleEvents as u64
                    && let IonValue::List(events) = value
                {
                    let end = events.iter().filter_map(event_end).max();
                    if let Some(end) = end
                        && let Some(IonValue::Struct(first)) = events.first_mut()
                    {
                        let start = field_int(first, KfxSymbol::Offset).unwrap_or(0);
                        set_int(first, KfxSymbol::Length, end - start);
                    }
                }
                stretch_first_event(value);
            }
        }
        IonValue::List(items) => {
            for item in items {
                stretch_first_event(item);
            }
        }
        _ => {}
    }
}

/// One past the last character a style event covers.
fn event_end(event: &IonValue) -> Option<i64> {
    let IonValue::Struct(fields) = event else {
        return None;
    };
    Some(field_int(fields, KfxSymbol::Offset)? + field_int(fields, KfxSymbol::Length)?)
}

/// The integer `symbol` holds on `fields`.
fn field_int(fields: &[(u64, IonValue)], symbol: KfxSymbol) -> Option<i64> {
    fields.iter().find_map(|(s, v)| match v {
        IonValue::Int(n) if *s == symbol as u64 => Some(*n),
        _ => None,
    })
}

/// Set `symbol` on `fields` to `value`, replacing one of the same symbol.
fn set_int(fields: &mut Vec<(u64, IonValue)>, symbol: KfxSymbol, value: i64) {
    fields.retain(|(s, _)| *s != symbol as u64);
    fields.push((symbol as u64, IonValue::Int(value)));
}

/// A flat mid-grey PNG of `width` by `height` pixels.
fn grey_png(width: u32, height: u32) -> io::Result<Vec<u8>> {
    let pixels = image::GrayImage::from_pixel(width.max(1), height.max(1), image::Luma([128u8]));
    let mut out = io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(pixels)
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(out.into_inner())
}

impl Importer for Source {
    fn open(_path: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a probe is authored, not opened",
        ))
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn toc(&self) -> &[TocEntry] {
        &self.toc
    }

    fn toc_mut(&mut self) -> &mut [TocEntry] {
        &mut self.toc
    }

    fn landmarks(&self) -> &[Landmark] {
        &self.landmarks
    }

    fn spine(&self) -> &[SpineEntry] {
        &self.spine
    }

    fn source_id(&self, _id: ChapterId) -> Option<&str> {
        Some(&self.href)
    }

    fn load_raw(&mut self, _id: ChapterId) -> io::Result<Vec<u8>> {
        Ok(self.document.clone().into_bytes())
    }

    fn list_assets(&self) -> &[PathBuf] {
        &self.assets
    }

    fn load_asset(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let name = path.file_name().unwrap_or(path.as_os_str());
        self.images
            .iter()
            .find(|(held, _, _)| std::path::Path::new(held).file_name() == Some(name))
            .map(|(_, width, height)| grey_png(*width, *height))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such picture: {}", path.display()),
                )
            })?
    }

    fn asset_manifest(&mut self) -> Option<Vec<AssetInfo>> {
        Some(
            self.images
                .iter()
                .map(|(name, width, height)| AssetInfo {
                    path: PathBuf::from(name),
                    media_type: "image/png".to_string(),
                    width: Some(*width),
                    height: Some(*height),
                })
                .collect(),
        )
    }

    /// One document: every href into the book lands on [`ChapterId`] 0.
    fn resolve_href(&self, _from: ChapterId, href: &str) -> Option<AnchorTarget> {
        let href = href.trim();
        if href.starts_with("http://") || href.starts_with("https://") {
            return Some(AnchorTarget::External(href.to_string()));
        }
        Some(AnchorTarget::Chapter(ChapterId(0)))
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_declares_nothing() {
        let probe = Probe::new("control", &[MEASURED_LATIN]);
        let document = probe.document();

        assert!(document.contains("<style>\n</style>"));
        assert!(document.contains("nnnn nnnn"));
    }

    #[test]
    fn a_probe_declares_one_property_on_one_selector() {
        let document = Probe::new("indent", &[PROSE_LATIN])
            .declaring("text-indent", "2em")
            .document();

        assert!(document.contains("p { text-indent: 2em; }"));
    }

    #[test]
    fn a_vertical_probe_states_its_axis_three_ways() {
        let probe = Probe::new("vertical", &[PROSE_JAPANESE])
            .in_language("ja")
            .vertical();
        let book = probe.book();

        assert!(probe.document().contains("writing-mode: vertical-rl"));
        assert_eq!(
            book.metadata().primary_writing_mode.as_deref(),
            Some("vertical-rl")
        );
        assert_eq!(
            book.metadata().page_progression_direction.as_deref(),
            Some("rtl")
        );
    }

    #[test]
    fn a_probe_imports_as_a_chapter_of_paragraphs() {
        use bokai::model::Role;

        let mut book = Probe::new("two", &["first", "second"]).book();
        let id = book.spine()[0].id;
        let chapter = book.load_chapter(id).expect("the document parses");

        let paragraphs = (0..chapter.node_count())
            .filter_map(|i| chapter.node(bokai::model::NodeId(i as u32)))
            .filter(|node| node.role == Role::Paragraph)
            .count();
        assert_eq!(paragraphs, 2);
    }

    #[test]
    fn a_probe_exports_to_a_kfx_that_reads_back() {
        let probe = Probe::new("roundtrip", &[PROSE_LATIN]).declaring("text-indent", "2em");
        let bytes = probe.kfx().expect("the probe exports");

        let mut reopened = Book::from_vec(bytes, Format::Kfx).expect("the container opens");
        assert_eq!(reopened.metadata().title, "roundtrip");
        let id = reopened.spine()[0].id;
        let chapter = reopened.load_chapter(id).expect("the chapter loads");

        let mut text = String::new();
        for i in 0..chapter.node_count() {
            let node = bokai::model::NodeId(i as u32);
            if let Some(entry) = chapter.node(node)
                && entry.role == bokai::model::Role::Text
            {
                text.push_str(chapter.text(entry.text));
            }
        }
        assert!(text.contains("Typography considered"), "got: {text}");
    }

    #[test]
    fn a_vertical_probe_exports_a_book_that_states_its_axis() {
        let probe = Probe::new("vertical-roundtrip", &[PROSE_JAPANESE])
            .in_language("ja")
            .vertical();
        let bytes = probe.kfx().expect("the probe exports");

        let mut reopened = Book::from_vec(bytes, Format::Kfx).expect("the container opens");
        assert_eq!(
            reopened.writing_mode(),
            bokai::style::WritingMode::VerticalRl
        );
    }

    #[test]
    fn a_paragraph_carries_the_default_sheets_margins_and_a_div_carries_nothing() {
        use bokai::model::Role;
        use bokai::style::Length;

        // The block holding the text, whatever role its element took.
        let margin_of_the_text_block = |probe: Probe| {
            let mut book = probe.book();
            let id = book.spine()[0].id;
            let chapter = book.load_chapter(id).expect("the document parses");
            (0..chapter.node_count())
                .map(|i| bokai::model::NodeId(i as u32))
                .filter(|node| {
                    chapter
                        .children(*node)
                        .any(|child| chapter.node(child).map(|n| n.role) == Some(Role::Text))
                })
                .filter_map(|node| chapter.node(node))
                .filter_map(|node| chapter.styles.get(node.style))
                .map(|style| style.margin_top)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            margin_of_the_text_block(Probe::new("p", &["text"])),
            vec![Length::Em(1.0)],
            "a paragraph takes the default sheet's margin"
        );
        assert_eq!(
            margin_of_the_text_block(Probe::new("div", &["text"]).as_element("div")),
            vec![Length::Auto],
            "a div declares none"
        );
    }

    /// Round numbers in the shape a profile takes, each ladder's stops far
    /// enough apart to tell which one a parameter came from.
    const PROFILE: &str = "\
panel 1000 2000 300
color 0
columns 0
font_size_default 10 20
font_size_cjk 11 21
font_size_indic 12 22
default_font_size 1
line_spacing 1.0 1.5 2.0
line_spacing_wide 1.1 1.6 2.1
boldness 0 20
default_boldness 0
margins_horizontal 1 2 3 4  11 12 13 14  21 22 23 24
margins_vertical 5 6 7 8  15 16 17 18  25 26 27 28
";

    #[test]
    fn a_script_opens_at_the_panels_own_defaults_and_draws_each_page() {
        let panel = Panel::parse(PROFILE).expect("the profile parses");
        let probe = Probe::new("ja", &[PROSE_JAPANESE])
            .in_language("ja")
            .vertical();

        let script = probe.script("/x/ja.kfx", &panel, &Settings::default_for(&panel), 3);
        let lines: Vec<&str> = script.lines().collect();

        // Every request is one line, and no parameter carries a stray space.
        assert!(lines.iter().all(|line| line.starts_with("/command/")));
        assert!(!lines[0].contains(' '), "{}", lines[0]);
        assert_eq!(
            lines[1..],
            [
                "/command/draw_page",
                "/command/next_page",
                "/command/draw_page",
                "/command/next_page",
                "/command/draw_page",
                "/command/close_uri",
            ]
        );

        // A vertical book takes the vertical margin ladder, and Japanese the
        // CJK font-size ladder and the wide line-spacing one.
        assert!(lines[0].contains("&width=1000&height=2000"));
        assert!(lines[0].contains("&top_margin=5&right_margin=6&bottom_margin=7&left_margin=8"));
        assert!(lines[0].contains("&font_size=21.000000&line_spacing=1.600000"));
        assert!(lines[0].contains("&embolden_weight=0"));
    }

    #[test]
    fn markup_in_the_text_is_escaped_rather_than_parsed() {
        let document = Probe::new("angles", &["a < b & c > d"]).document();

        assert!(document.contains("a &lt; b &amp; c &gt; d"));
    }
}
