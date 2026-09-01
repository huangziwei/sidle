//! Writes [`suite`] out as KFX, each book with the script that renders it.
//!
//! `sidle-render-probe <directory> <profile> <uri-prefix>`, where `<profile>`
//! is a [`Panel::parse`] file and `<uri-prefix>` the path each book opens at.

use std::error::Error;
use std::path::PathBuf;

use bokai::formats::kfx::ion::IonValue;
use bokai::formats::kfx::symbols::KfxSymbol;
use sidle_render::probe::{MEASURED_LATIN, PROSE_JAPANESE, PROSE_LATIN, Probe, SHORT_WORDS_LATIN};
use sidle_render::settings::{Panel, Settings};

/// How many pages of each probe to draw.
const PAGES: usize = 3;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(profile), Some(prefix)) = (
        args.next().map(PathBuf::from),
        args.next().map(PathBuf::from),
        args.next(),
    ) else {
        eprintln!("usage: sidle-render-probe <directory> <profile> <uri-prefix>");
        std::process::exit(2);
    };
    let panel = Panel::read(&profile)?;
    let settings = Settings::default_for(&panel);
    let prefix = prefix.trim_end_matches('/').to_string();

    let suite = suite();
    for probe in &suite {
        probe.write_kfx(&dir)?;
        let uri = format!("{prefix}/{}.kfx", probe.file_stem());
        let script = dir.join(format!("{}.script.txt", probe.file_stem()));
        std::fs::write(&script, probe.script(&uri, &panel, &settings, PAGES))?;
    }
    println!("{} books and scripts in {}", suite.len(), dir.display());
    Ok(())
}

/// One `(property, value)` per row, set on the `div` a control uses.
const DECLARED: &[(&str, &str)] = &[
    // Box model.
    ("margin-top", "24px"),
    ("margin-bottom", "24px"),
    ("margin-left", "24px"),
    ("margin-right", "24px"),
    ("margin-left", "10%"),
    ("padding-top", "24px"),
    ("padding-bottom", "24px"),
    ("padding-left", "24px"),
    ("padding-right", "24px"),
    ("width", "50%"),
    ("width", "300px"),
    ("height", "200px"),
    ("max-width", "40%"),
    ("min-width", "80%"),
    ("max-height", "100px"),
    ("min-height", "400px"),
    ("box-sizing", "border-box"),
    ("border-top-width", "8px"),
    ("border-bottom-width", "8px"),
    ("border-left-width", "8px"),
    ("border-right-width", "8px"),
    ("border-top-style", "solid"),
    ("border-top-style", "dashed"),
    ("border-top-style", "double"),
    ("border-top-color", "#808080"),
    ("border-top-left-radius", "12px"),
    ("background-color", "#c0c0c0"),
    ("box-align", "center"),
    ("float", "left"),
    ("clear", "both"),
    ("visibility", "hidden"),
    // Inline and type.
    ("font-size", "20px"),
    ("font-size", "1.5em"),
    ("font-size", "150%"),
    ("font-weight", "bold"),
    ("font-style", "italic"),
    ("font-variant", "small-caps"),
    ("font-family", "Baskerville"),
    ("font-family", "Helvetica"),
    ("letter-spacing", "4px"),
    ("word-spacing", "12px"),
    ("line-height", "2"),
    ("line-height", "40px"),
    ("line-height", "150%"),
    ("text-indent", "2em"),
    ("text-indent", "-2em"),
    ("text-align", "center"),
    ("text-align", "right"),
    ("text-align", "justify"),
    ("text-transform", "uppercase"),
    ("text-decoration", "underline"),
    ("text-decoration-style", "dashed"),
    ("text-decoration-color", "#808080"),
    ("color", "#606060"),
    ("vertical-align", "super"),
    ("vertical-align", "sub"),
    ("white-space", "pre"),
    ("white-space", "nowrap"),
    ("word-break", "break-all"),
    ("overline", "true"),
    // Line breaking.
    ("hyphens", "none"),
    ("hyphens", "auto"),
    ("orphans", "3"),
    ("widows", "3"),
    ("break-before", "always"),
    ("break-after", "always"),
    ("break-inside", "avoid"),
    // Lists.
    ("list-style-type", "square"),
    ("list-style-position", "inside"),
];

/// Every book [`main`] writes.
fn suite() -> Vec<Probe> {
    let mut probes = vec![
        // A `div` declaring nothing exports a style fragment holding only
        // its name and the book's language.
        Probe::new("control-latin", &[MEASURED_LATIN, PROSE_LATIN]).as_element("div"),
        Probe::new("control-japanese", &[PROSE_JAPANESE])
            .as_element("div")
            .in_language("ja")
            .vertical(),
        // A `p` carries `margin: 1em 0` from the default stylesheet.
        Probe::new("control-latin-paragraph", &[MEASURED_LATIN, PROSE_LATIN]),
        // Every word in [`SHORT_WORDS_LATIN`] fits on a line.
        Probe::new(
            "control-short-words",
            &[SHORT_WORDS_LATIN, SHORT_WORDS_LATIN],
        )
        .as_element("div"),
    ];

    let mut seen: Vec<String> = Vec::new();
    for (property, value) in DECLARED {
        let mut name = format!("{property}-{}", slug(value));
        while seen.contains(&name) {
            name.push('x');
        }
        seen.push(name.clone());
        probes.push(
            Probe::new(name, &[MEASURED_LATIN, PROSE_LATIN])
                .as_element("div")
                .declaring(*property, *value),
        );
    }
    probes.extend(paired());
    probes.extend(shaped());
    probes.extend(structured());
    probes.extend(pictures());
    probes.extend(remaining());
    probes.extend(on_elements());
    probes.extend(narrow_pictures());
    probes.extend(inline_structure());
    probes.extend(cjk_metrics());
    probes.extend(pinned_faces());
    probes.extend(word_spacing());
    probes.extend(cjk_junctions());
    probes
}

/// Probes that need two blocks styled differently, or two properties set
/// together.
fn paired() -> Vec<Probe> {
    let three = [MEASURED_LATIN, PROSE_LATIN, MEASURED_LATIN];
    vec![
        // A cap in ems on a bordered box, and the same box padded: the
        // border draws the box the cap left.
        Probe::new("max-width-22em-bordered", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .styled("div { max-width: 22em; border-top: 8px solid #000; }\n"),
        Probe::new("max-width-22em-padded", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .styled(
                "div { max-width: 22em; padding: 0 4em 0 1.5em; \
                 border-top: 8px solid #000; }\n",
            ),
        // A capped, padded box holding a bordered child: the child's border
        // draws what the cap and the two boxes' insets left.
        Probe::new("max-width-22em-nested", &["<div class=\"rule\">組版</div>"])
            .as_element("div")
            .as_markup()
            .in_language("ja")
            .styled(
                "div.p0 { max-width: 22em; padding: 0 4em 0 1.5em; }\n\
             div.rule { margin-left: 3em; margin-right: 1.5em; \
             padding-right: 1.5em; font-size: 0.85em; \
             border-top: 8px solid #000; }\n",
            ),
        // The nested pair split: the outer box's padding alone, then the
        // inner box's margins alone at a font size of its own.
        Probe::new(
            "max-width-22em-outer-pad",
            &["<div class=\"rule\">組版</div>"],
        )
        .as_element("div")
        .as_markup()
        .in_language("ja")
        .styled(
            "div.p0 { max-width: 22em; padding: 0 4em 0 1.5em; }\n\
             div.rule { border-top: 8px solid #000; }\n",
        ),
        // The outer box's padding, in a book that reads in English.
        Probe::new(
            "max-width-22em-outer-pad-latin",
            &["<div class=\"rule\">Typography</div>"],
        )
        .as_element("div")
        .as_markup()
        .styled(
            "div.p0 { max-width: 22em; padding: 0 4em 0 1.5em; }\n\
             div.rule { border-top: 8px solid #000; }\n",
        ),
        // Half an em of padding either side, in a book on the em grid.
        Probe::new(
            "max-width-22em-half-pad",
            &["<div class=\"rule\">組版</div>"],
        )
        .as_element("div")
        .as_markup()
        .in_language("ja")
        .styled(
            "div.p0 { max-width: 22em; padding: 0 0.5em; }\n\
             div.rule { border-top: 8px solid #000; }\n",
        ),
        Probe::new(
            "max-width-22em-inner-margin",
            &["<div class=\"rule\">組版</div>"],
        )
        .as_element("div")
        .as_markup()
        .in_language("ja")
        .styled(
            "div.p0 { max-width: 22em; }\n\
             div.rule { margin-left: 3em; margin-right: 1.5em; \
             font-size: 0.85em; border-top: 8px solid #000; }\n",
        ),
        // The same cap on a section written across a book that reads down
        // the page.
        Probe::new("max-width-22em-across", &[PROSE_JAPANESE])
            .as_element("div")
            .in_language("ja")
            .vertical()
            .styled(
                "div { max-width: 22em; writing-mode: horizontal-tb; \
                 border-top: 8px solid #000; }\n",
            ),
        // A border weight and a border style, declared together.
        Probe::new("border-top-8px-solid", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .styled("div { border-top: 8px solid #000; }\n"),
        Probe::new("border-all-8px-solid", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .styled("div { border: 8px solid #000; }\n"),
        // Two line heights in one book, which exports as two `lh` ratios.
        Probe::new("line-height-1em-and-2em", &three)
            .as_element("div")
            .styled(
                ".p0 { line-height: 1em; } .p1 { line-height: 2em; } .p2 { line-height: 1em; }\n",
            ),
        Probe::new("line-height-1em-and-1d5em", &three)
            .as_element("div")
            .styled(
                ".p0 { line-height: 1em; } .p1 { line-height: 1.5em; } .p2 { line-height: 1em; }\n",
            ),
        // Space between two blocks, from each side and from both.
        Probe::new("margin-bottom-between", &three)
            .as_element("div")
            .styled(".p0 { margin-bottom: 24px; }\n"),
        Probe::new("margin-collapse", &three)
            .as_element("div")
            .styled(".p0 { margin-bottom: 24px; } .p1 { margin-top: 24px; }\n"),
        Probe::new("padding-bottom-between", &three)
            .as_element("div")
            .styled(".p0 { padding-bottom: 24px; }\n"),
        // A font stack of several families.
        Probe::new("font-family-pinned", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .styled("div { font-family: Baskerville, Palatino, serif; }\n"),
        Probe::new(
            "font-family-helvetica-pinned",
            &[MEASURED_LATIN, PROSE_LATIN],
        )
        .as_element("div")
        .styled("div { font-family: Helvetica, Futura, sans-serif; }\n"),
        // Letter and word spacing over [`MEASURED_LATIN`] alone.
        Probe::new(
            "letter-spacing-on-measured",
            &[MEASURED_LATIN, MEASURED_LATIN],
        )
        .as_element("div")
        .styled("div { letter-spacing: 4px; }\n"),
        Probe::new(
            "word-spacing-on-measured",
            &[MEASURED_LATIN, MEASURED_LATIN],
        )
        .as_element("div")
        .styled("div { word-spacing: 12px; }\n"),
        // One indented block between two that are not.
        Probe::new("text-indent-paired", &three)
            .as_element("div")
            .styled(".p1 { text-indent: 2em; }\n"),
    ]
}

/// A value as a file name: letters, digits and dashes.
fn slug(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => c,
            '%' => 'p',
            '.' => 'd',
            '-' => 'n',
            _ => '-',
        })
        .collect()
}

/// Twelve numbered paragraphs, which run past one page.
fn long_text() -> Vec<String> {
    (0..12)
        .map(|n| format!("{n}. {PROSE_LATIN}"))
        .collect::<Vec<String>>()
}

/// Books needing markup of their own, or an injected KFX property.
fn shaped() -> Vec<Probe> {
    let long: Vec<String> = long_text();
    let long: Vec<&str> = long.iter().map(String::as_str).collect();
    let ruby = "<ruby>東京<rt>とうきょう</rt></ruby>にある図書館で、\
<ruby>組版<rt>くみはん</rt></ruby>の本を読んだ。";
    let table = "one|two|three";

    vec![
        // A book of [`long_text`], and the properties a page cut acts on.
        Probe::new("pages-long", &long).as_element("div"),
        Probe::new("pages-orphans-3", &long)
            .as_element("div")
            .declaring("orphans", "3"),
        Probe::new("pages-widows-3", &long)
            .as_element("div")
            .declaring("widows", "3"),
        Probe::new("pages-break-inside-avoid", &long)
            .as_element("div")
            .declaring("break-inside", "avoid"),
        // Ruby, down the page and across it.
        Probe::new("ruby-vertical", &[ruby, ruby])
            .as_element("div")
            .in_language("ja")
            .vertical(),
        Probe::new("ruby-horizontal", &[ruby, ruby])
            .as_element("div")
            .in_language("ja"),
        Probe::new(
            "tate-chu-yoko",
            &["平成30年に、100人が来た。", "平成30年に、100人が来た。"],
        )
        .as_element("div")
        .in_language("ja")
        .vertical()
        .styled("div { text-combine-upright: all; }\n"),
        // A table of three columns.
        Probe::new("table-three-columns", &[table])
            .as_element("div")
            .styled("div { display: table; }\n"),
        // KFX properties [`StyleSchema`] maps no CSS to, written in by
        // [`Probe::injecting`].
        Probe::new("kfx-ligatures-off", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::Ligatures, IonValue::Bool(false)),
        Probe::new("kfx-ligatures-on", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::Ligatures, IonValue::Bool(true)),
        Probe::new("kfx-kerning-off", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::Kerning, IonValue::Bool(false)),
        Probe::new("kfx-kerning-on", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::Kerning, IonValue::Bool(true)),
        Probe::new("kfx-min-hyphen-word-length-12", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::MinHyphenWordLength, IonValue::Int(12)),
        Probe::new("kfx-min-hyphen-word-length-3", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::MinHyphenWordLength, IonValue::Int(3)),
        Probe::new("kfx-hyphen-dictionary-de", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(
                KfxSymbol::HyphenDictionary,
                IonValue::String("de".to_string()),
            ),
        Probe::new("kfx-ot-features-liga", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::OtFeatures, IonValue::String("liga".to_string())),
    ]
}

/// Books whose paragraphs are markup [`Probe::as_markup`] passes through, and
/// second values for the injected properties [`shaped`] left unmoved.
fn structured() -> Vec<Probe> {
    let ruby = "<ruby>東京<rt>とうきょう</rt></ruby>にある図書館で、\
<ruby>組版<rt>くみはん</rt></ruby>の本を読んだ。";
    let tcy = "平成<span style=\"text-combine-upright: all\">30</span>年に、\
<span style=\"text-combine-upright: all\">100</span>人が来た。";
    let table = "<table><tr><td>one</td><td>two</td><td>three</td></tr>\
<tr><td>four</td><td>five</td><td>six</td></tr></table>";
    let spans = "<table><tr><td colspan=\"2\">wide</td><td>one</td></tr>\
<tr><td>a</td><td>b</td><td>c</td></tr></table>";
    let ligatures = "office affluent waffle fluffy finding difficult \
efficient sufficient offline baffling";

    vec![
        Probe::new("markup-ruby-vertical", &[ruby, ruby])
            .as_element("div")
            .as_markup()
            .in_language("ja")
            .vertical(),
        Probe::new("markup-ruby-horizontal", &[ruby, ruby])
            .as_element("div")
            .as_markup()
            .in_language("ja"),
        Probe::new("markup-tate-chu-yoko", &[tcy, tcy])
            .as_element("div")
            .as_markup()
            .in_language("ja")
            .vertical(),
        Probe::new("markup-table", &[table])
            .as_element("div")
            .as_markup(),
        Probe::new("markup-table-colspan", &[spans])
            .as_element("div")
            .as_markup(),
        // Text carrying the pairs a ligature is made of.
        Probe::new("ligature-text-control", &[ligatures, ligatures]).as_element("div"),
        Probe::new("ligature-text-off", &[ligatures, ligatures])
            .as_element("div")
            .injecting(KfxSymbol::Ligatures, IonValue::Bool(false)),
        // Other grammars for the three properties a string and an int left
        // unmoved.
        Probe::new("kfx-min-hyphen-length-string", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(
                KfxSymbol::MinHyphenWordLength,
                IonValue::String("12".to_string()),
            ),
        Probe::new("kfx-hyphen-dictionary-symbol", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::HyphenDictionary, IonValue::Symbol(0)),
        Probe::new("kfx-ot-features-list", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(
                KfxSymbol::OtFeatures,
                IonValue::List(vec![IonValue::String("liga".to_string())]),
            ),
        Probe::new("kfx-ot-features-bool", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::OtFeatures, IonValue::Bool(false)),
    ]
}

/// Books carrying a picture, for the properties that act on a replaced box.
fn pictures() -> Vec<Probe> {
    let wide = "<img src=\"wide.png\" alt=\"\"/>";
    let tall = "<img src=\"tall.png\" alt=\"\"/>";
    let small = "<img src=\"small.png\" alt=\"\"/>";
    let image = |name: &str, markup: &str, w: u32, h: u32| {
        Probe::new(name, &[markup, PROSE_LATIN])
            .as_element("div")
            .as_markup()
            .with_image("wide.png", 1600, 400)
            .with_image("tall.png", 400, 1600)
            .with_image("small.png", 200, 100)
            .with_image(name, w, h)
    };

    vec![
        image("image-wide", wide, 1, 1),
        image("image-tall", tall, 1, 1),
        image("image-small", small, 1, 1),
        image("image-box-align-center", small, 1, 1).styled("img { box-align: center; }\n"),
        image("image-width-50p", small, 1, 1).styled("img { width: 50%; }\n"),
        image("image-max-width-40p", wide, 1, 1).styled("img { max-width: 40%; }\n"),
        image("image-width-100p", small, 1, 1).styled("img { width: 100%; }\n"),
    ]
}

/// Books for the properties reachable only as a KFX symbol, and the markup
/// that exercises a table's own vocabulary.
fn remaining() -> Vec<Probe> {
    let table = "<table><tr><td>one</td><td>two</td><td>three</td></tr>\
<tr><td>four</td><td>five</td><td>six</td></tr></table>";
    let wide = "<img src=\"wide.png\" alt=\"\"/>";
    let picture = |name: &str| {
        Probe::new(name, &[wide, PROSE_LATIN])
            .as_element("div")
            .as_markup()
            .with_image("wide.png", 1600, 400)
    };
    let cjk = "、。「」あア亜、。「」あア亜、。「」あア亜、。「」あア亜、。「」あア亜";

    vec![
        // How a picture is fitted to its box.
        picture("fit-width-true").injecting(KfxSymbol::FitWidth, IonValue::Bool(true)),
        picture("fit-width-false").injecting(KfxSymbol::FitWidth, IonValue::Bool(false)),
        picture("fit-tight-true").injecting(KfxSymbol::FitTight, IonValue::Bool(true)),
        // Where a block sits on the page.
        Probe::new("position-footer", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(
                KfxSymbol::Position,
                IonValue::Symbol(KfxSymbol::Footer as u64),
            ),
        Probe::new("position-fixed", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(
                KfxSymbol::Position,
                IonValue::Symbol(KfxSymbol::Fixed as u64),
            ),
        // How a run sits on its baseline, and what a shadow costs.
        Probe::new("baseline-style-2", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::BaselineStyle, IonValue::Int(2)),
        Probe::new("text-shadows-on", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::TextShadows, IonValue::Bool(true)),
        // A table's own vocabulary.
        Probe::new("table-important-cells", &[table])
            .as_element("div")
            .as_markup()
            .injecting(KfxSymbol::ImportantCells, IonValue::Int(1)),
        Probe::new("table-pan-zoom", &[table])
            .as_element("div")
            .as_markup()
            .injecting(KfxSymbol::PanZoom, IonValue::Bool(true)),
        // Column widths: the same cells with one word and with several.
        Probe::new(
            "table-uneven",
            &["<table><tr><td>a</td>\
<td>a much longer cell than its neighbours</td><td>b</td></tr></table>"],
        )
        .as_element("div")
        .as_markup(),
        // CJK punctuation, whose compression is what a vertical line squeezes.
        Probe::new("cjk-punctuation-vertical", &[cjk, cjk])
            .as_element("div")
            .in_language("ja")
            .vertical(),
        Probe::new("cjk-punctuation-horizontal", &[cjk, cjk])
            .as_element("div")
            .in_language("ja"),
        // Tate-chu-yoko: a two-digit run set horizontally inside the vertical
        // line, against the same digits in a run too long to combine.
        Probe::new("tcy-plain", &["平成30年に、10人が来た。"])
            .as_element("div")
            .in_language("ja")
            .vertical(),
        Probe::new("tcy-long-run", &["平成100年に、1000人が来た。"])
            .as_element("div")
            .in_language("ja")
            .vertical(),
        Probe::new(
            "tcy-span",
            &["平成<span style=\"writing-mode: horizontal-tb; \
text-combine-upright: all\">30</span>年に、10人が来た。"],
        )
        .as_element("div")
        .as_markup()
        .in_language("ja")
        .vertical(),
        // A combine inside a styled run: the nested element lands inside the
        // style event's range.
        Probe::new(
            "tcy-in-styled-run",
            &["平成<span style=\"letter-spacing: 0.5em\">30年に、10人</span>が来た。"],
        )
        .as_element("div")
        .as_markup()
        .in_language("ja")
        .vertical(),
        // The control the others read against: the same sentence horizontal.
        Probe::new("tcy-horizontal-control", &["平成30年に、10人が来た。"])
            .as_element("div")
            .in_language("ja"),
    ]
}

/// The same properties as [`remaining`], written onto the storyline element.
fn on_elements() -> Vec<Probe> {
    let table = "<table><tr><td>one</td><td>two</td><td>three</td></tr>\
<tr><td>four</td><td>five</td><td>six</td></tr></table>";
    let wide = "<img src=\"wide.png\" alt=\"\"/>";
    let picture = |name: &str| {
        Probe::new(name, &[wide, PROSE_LATIN])
            .as_element("div")
            .as_markup()
            .with_image("wide.png", 1600, 400)
    };
    let framed = |name: &str| Probe::new(name, &[table]).as_element("div").as_markup();

    vec![
        picture("elem-fit-width-true").injecting_element(KfxSymbol::FitWidth, IonValue::Bool(true)),
        picture("elem-fit-width-false")
            .injecting_element(KfxSymbol::FitWidth, IonValue::Bool(false)),
        picture("elem-fit-tight-true").injecting_element(KfxSymbol::FitTight, IonValue::Bool(true)),
        picture("elem-box-align-center").injecting_element(
            KfxSymbol::BoxAlign,
            IonValue::Symbol(KfxSymbol::Center as u64),
        ),
        framed("elem-important-cells")
            .injecting_element(KfxSymbol::ImportantCells, IonValue::Int(1)),
        framed("elem-pan-zoom").injecting_element(KfxSymbol::PanZoom, IonValue::Bool(true)),
        Probe::new("elem-baseline-style-2", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting_element(KfxSymbol::BaselineStyle, IonValue::Int(2)),
        Probe::new("elem-text-shadows", &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting_element(KfxSymbol::TextShadows, IonValue::Bool(true)),
    ]
}

/// Drop caps, and two style events where one range contains the other.
fn inline_structure() -> Vec<Probe> {
    let spans = "<span class=\"wide\">AAAA</span><span class=\"heavy\">BB</span>AAAA";
    let css = ".wide { letter-spacing: 4px; } .heavy { font-weight: bold; }\n";
    let paired = |name: &str| {
        Probe::new(name, &[spans, PROSE_LATIN])
            .as_element("div")
            .as_markup()
            .styled(css)
    };

    vec![
        // The exporter's own shape: the two ranges side by side.
        paired("events-side-by-side"),
        // The same two styles, the first range covering the second.
        paired("events-containing").containing_events(),
        // `dropcap_lines` and `dropcap_chars`, which no CSS maps to.
        Probe::new("dropcap-lines-3", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::DropcapLines, IonValue::Int(3)),
        Probe::new("dropcap-chars-1", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::DropcapChars, IonValue::Int(1)),
        Probe::new("dropcap-lines-3-chars-1", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::DropcapLines, IonValue::Int(3))
            .injecting(KfxSymbol::DropcapChars, IonValue::Int(1)),
        Probe::new("elem-dropcap-lines-3", &[PROSE_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting_element(KfxSymbol::DropcapLines, IonValue::Int(3))
            .injecting_element(KfxSymbol::DropcapChars, IonValue::Int(1)),
    ]
}

/// The fit and alignment properties over a picture narrower than its box,
/// which is the case that can move.
fn narrow_pictures() -> Vec<Probe> {
    let small = "<img src=\"small.png\" alt=\"\"/>";
    let picture = |name: &str| {
        Probe::new(name, &[small, PROSE_LATIN])
            .as_element("div")
            .as_markup()
            .with_image("small.png", 200, 100)
    };

    vec![
        picture("narrow-control"),
        picture("narrow-fit-width").injecting_element(KfxSymbol::FitWidth, IonValue::Bool(true)),
        picture("narrow-fit-tight").injecting_element(KfxSymbol::FitTight, IonValue::Bool(true)),
        picture("narrow-align-center").injecting_element(
            KfxSymbol::BoxAlign,
            IonValue::Symbol(KfxSymbol::Center as u64),
        ),
        picture("narrow-align-right").injecting_element(
            KfxSymbol::BoxAlign,
            IonValue::Symbol(KfxSymbol::Right as u64),
        ),
    ]
}

/// The CJK punctuation repertoire `cjk_metrics` sets one paragraph per.
const CJK_MARKS: &[char] = &[
    '、', '。', '，', '．', '・', '：', '；', '！', '？', '…', '‥', '―', 'ー', '〜', '～', '「',
    '」', '『', '』', '（', '）', '［', '］', '｛', '｝', '〈', '〉', '《', '》', '【', '】', '〔',
    '〕', '々', '\u{3000}',
];

/// The ideograph each mark is set between.
const REFERENCE_IDEOGRAPH: char = '亜';

/// A line alternating `REFERENCE_IDEOGRAPH` with `mark`.
fn mark_line(mark: char) -> String {
    let mut line = String::new();
    for _ in 0..8 {
        line.push(REFERENCE_IDEOGRAPH);
        line.push(mark);
    }
    line.push(REFERENCE_IDEOGRAPH);
    line
}

/// One paragraph per mark in `CJK_MARKS`, on both axes.
fn cjk_metrics() -> Vec<Probe> {
    let mut lines: Vec<String> = vec![mark_line(REFERENCE_IDEOGRAPH)];
    lines.extend(CJK_MARKS.iter().copied().map(mark_line));
    let paragraphs: Vec<&str> = lines.iter().map(String::as_str).collect();

    vec![
        Probe::new("cjk-advances-horizontal", &paragraphs)
            .as_element("div")
            .in_language("ja"),
        Probe::new("cjk-advances-vertical", &paragraphs)
            .as_element("div")
            .in_language("ja")
            .vertical(),
    ]
}

/// A `font_family` written past the `default` head every export carries.
fn pinned_faces() -> Vec<Probe> {
    let pinned = |name: &str, family: &str| {
        Probe::new(name, &[MEASURED_LATIN, PROSE_LATIN])
            .as_element("div")
            .injecting(KfxSymbol::FontFamily, IonValue::String(family.to_string()))
    };

    vec![
        pinned("face-baskerville", "Baskerville"),
        pinned("face-futura", "Futura"),
        pinned("face-caecilia", "Caecilia"),
        // A family the search path has no file for.
        pinned("face-absent", "NoSuchFamily"),
        // The deferring form, spelled out.
        pinned("face-deferred", "default"),
    ]
}

/// Word spacing over [`MEASURED_LATIN`], whose spaces can be counted.
fn word_spacing() -> Vec<Probe> {
    vec![
        Probe::new("word-spacing-control", &[MEASURED_LATIN, MEASURED_LATIN]).as_element("div"),
        Probe::new("word-spacing-8", &[MEASURED_LATIN, MEASURED_LATIN])
            .as_element("div")
            .declaring("word-spacing", "8px"),
        Probe::new("word-spacing-16", &[MEASURED_LATIN, MEASURED_LATIN])
            .as_element("div")
            .declaring("word-spacing", "16px"),
    ]
}

/// Punctuation pairs `cjk_junctions` sets side by side.
const CJK_JUNCTIONS: &[[char; 2]] = &[
    ['、', '。'],
    ['。', '「'],
    ['」', '「'],
    ['「', '」'],
    ['）', '（'],
    ['（', '）'],
    ['、', '、'],
    ['「', '「'],
    ['。', 'あ'],
    ['あ', '「'],
    ['、', '」'],
    ['】', '【'],
];

/// One line per pair in `CJK_JUNCTIONS`, plus a line opening on a bracket
/// and one closing on a stop.
fn cjk_junctions() -> Vec<Probe> {
    let mut lines: Vec<String> = Vec::new();
    for pair in CJK_JUNCTIONS {
        let mut line = String::from(REFERENCE_IDEOGRAPH);
        for _ in 0..6 {
            line.push(pair[0]);
            line.push(pair[1]);
            line.push(REFERENCE_IDEOGRAPH);
        }
        lines.push(line);
    }
    lines.push(format!("「{0}{0}{0}{0}{0}{0}」", REFERENCE_IDEOGRAPH));
    lines.push(format!("{0}{0}{0}{0}{0}{0}。", REFERENCE_IDEOGRAPH));
    let paragraphs: Vec<&str> = lines.iter().map(String::as_str).collect();

    vec![
        Probe::new("cjk-junctions-horizontal", &paragraphs)
            .as_element("div")
            .in_language("ja"),
        Probe::new("cjk-junctions-vertical", &paragraphs)
            .as_element("div")
            .in_language("ja")
            .vertical(),
    ]
}
