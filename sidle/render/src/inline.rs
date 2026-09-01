//! Inline layout: text and atomic boxes packed into lines. `rustybuzz`
//! shapes; lines break at UAX #14 opportunities, reined by `hyphenate`.

use std::ops::Range;

use ab_glyph::Font as _;
use bokai::model::{NodeId, Role};
use bokai::style::{Color, ComputedStyle, TextAlign, WhiteSpace};
use bokai::text::hyphenation::Hyphenator;
use unicode_linebreak::BreakOpportunity;

use crate::font::{Face, FaceId, Fonts};
use crate::fragment::{Content, Fragment, Glyph, GlyphRun, Node, Orientation};
use crate::geom::{Axis, Rect};
use crate::text;

/// Text style resolved to numbers, as a run needs it.
#[derive(Debug, Clone, Copy)]
pub struct TextStyle<'a> {
    pub computed: &'a ComputedStyle,
    pub font_size: f32,
    pub line_height: f32,
    pub color: Color,
    pub letter_spacing: f32,
    /// Added to every space's advance.
    pub word_spacing: f32,
    /// Added to every glyph's advance.
    pub embolden: f32,
    pub underline: bool,
    pub line_through: bool,
    pub preserve_spaces: bool,
}

/// One paragraph's `trim_table`, and where a stretch sits in it.
/// A junction spans two `Atom`s wherever the line breaker split the pair.
#[derive(Debug, Clone, Copy)]
struct Spacing<'a> {
    /// Leading and trailing trims in ems, by the byte a character starts at.
    trims: &'a [(f32, f32)],
    /// Where this stretch starts in the paragraph.
    base: usize,
}

impl Spacing<'_> {
    /// The trims on the character `at` bytes into its own item.
    fn at(&self, at: usize) -> (f32, f32) {
        self.trims
            .get(self.base.wrapping_add(at))
            .copied()
            .unwrap_or((0.0, 0.0))
    }

    /// A stretch no junction reaches: a ruby reading.
    fn none() -> Self {
        Self {
            trims: &[],
            base: 0,
        }
    }
}

/// One `Item`'s text and what shaping a range of it needs.
#[derive(Clone, Copy)]
struct Stretch<'a, 't> {
    /// Which [`Item`] the text belongs to.
    item: usize,
    text: &'t str,
    style: TextStyle<'a>,
    spacing: Spacing<'a>,
}

/// Each character's leading and trailing trim in ems, by its first byte.
fn trim_table(text: &str) -> Vec<(f32, f32)> {
    let mut table = vec![(0.0, 0.0); text.len() + 1];
    let mut previous: Option<(usize, char)> = None;
    for (at, ch) in text.char_indices() {
        if let Some((before, left)) = previous {
            let (trailing, leading) = text::junction_trim(left, ch);
            table[before].1 = trailing;
            table[at].0 = leading;
        }
        previous = Some((at, ch));
    }
    table
}

/// One piece of inline content, before it is broken into lines.
pub enum Item<'a> {
    /// A stretch of text in one style.
    Text {
        source: NodeId,
        style: TextStyle<'a>,
        text: String,
    },
    /// A base and the annotation set beside it — furigana over kanji, and
    /// the same construct wherever else a book uses it.
    Ruby {
        source: NodeId,
        style: TextStyle<'a>,
        base: String,
        annotation_style: TextStyle<'a>,
        annotation: String,
    },
    /// A box that sits on the line whole — an image.
    Replaced {
        source: NodeId,
        inline_size: f32,
        block_size: f32,
        src: String,
        background: Option<Color>,
    },
    /// An explicit `<br>`.
    Break { source: NodeId },
}

impl Item<'_> {
    fn source(&self) -> NodeId {
        match self {
            Item::Text { source, .. }
            | Item::Ruby { source, .. }
            | Item::Replaced { source, .. }
            | Item::Break { source } => *source,
        }
    }
}

/// Glyphs from one face at one size, positioned along the line.
#[derive(Debug, Clone)]
struct Run {
    face: FaceId,
    size: f32,
    color: Color,
    orientation: Orientation,
    underline: bool,
    line_through: bool,
    glyphs: Vec<Glyph>,
}

/// A shaped, indivisible piece: one line break opportunity to the next.
struct Atom {
    item: usize,
    advance: f32,
    /// What the atom draws on the line. Absent for a break.
    run: Option<Run>,
    /// A ruby annotation, drawn in the strip alongside the line.
    annotation: Option<Run>,
    /// How far the annotation reaches along the line, and how far across it.
    annotation_advance: f32,
    annotation_size: f32,
    ascent: f32,
    descent: f32,
    line_height: f32,
    /// Whitespace, which a line drops at its end.
    is_space: bool,
    /// A line must end here.
    mandatory: bool,
    /// A line that ends here takes a hyphen.
    hyphenated: bool,
}

impl Atom {
    fn empty(item: usize) -> Self {
        Self {
            item,
            advance: 0.0,
            run: None,
            annotation: None,
            annotation_advance: 0.0,
            annotation_size: 0.0,
            ascent: 0.0,
            descent: 0.0,
            line_height: 0.0,
            is_space: false,
            mandatory: false,
            hyphenated: false,
        }
    }
}

/// What inline layout produced.
pub struct Lines {
    /// How far the lines reach across the block axis.
    pub block_size: f32,
    /// Fragments in logical coordinates, relative to the content box's
    /// inline-start / block-start corner.
    pub fragments: Vec<Fragment>,
}

pub struct Inline<'f> {
    pub fonts: &'f mut Fonts,
    pub axis: Axis,
    pub hyphenator: Option<&'static Hyphenator>,
}

impl Inline<'_> {
    /// Break `items` into lines `available` long, indenting the first by
    /// `indent`, and place them.
    pub fn lay_out(
        &mut self,
        items: &[Item<'_>],
        available: f32,
        indent: f32,
        align: TextAlign,
        fallback_line_height: f32,
    ) -> Lines {
        let atoms = self.atoms(items);
        if atoms.is_empty() {
            return Lines {
                block_size: 0.0,
                fragments: Vec::new(),
            };
        }
        let lines = pack(&atoms, available, indent);
        self.place(
            items,
            &atoms,
            &lines,
            available,
            indent,
            align,
            fallback_line_height,
        )
    }

    /// The widest unbreakable piece of `items`, and all of it on one line.
    pub fn measure(&mut self, items: &[Item<'_>]) -> (f32, f32) {
        let atoms = self.atoms(items);
        let widest = atoms
            .iter()
            .filter(|atom| !atom.is_space)
            .map(|atom| atom.advance)
            .fold(0.0f32, f32::max);
        let whole = atoms.iter().map(|atom| atom.advance).sum();
        (widest, whole)
    }
}

// --- Building atoms -------------------------------------------------------

impl Inline<'_> {
    fn atoms(&mut self, items: &[Item<'_>]) -> Vec<Atom> {
        // Break opportunities are a property of the text as a whole. The
        // items are joined before UAX #14 runs: a word split across two spans
        // gains no break in the middle. A ruby group joins as its base,
        // The text around it breaks as usual; the group
        // itself is one atom and never splits.
        let mut joined = String::new();
        let mut spans: Vec<Range<usize>> = Vec::new();
        for item in items {
            match item {
                Item::Text { text, .. } => {
                    let start = joined.len();
                    joined.push_str(text);
                    spans.push(start..joined.len());
                }
                Item::Ruby { base, .. } => {
                    let start = joined.len();
                    joined.push_str(base);
                    spans.push(start..joined.len());
                }
                _ => {}
            }
        }

        let opportunities: Vec<(usize, BreakOpportunity)> = if joined.is_empty() {
            Vec::new()
        } else {
            unicode_linebreak::linebreaks(&joined).collect()
        };
        let trims = trim_table(&joined);

        let mut atoms = Vec::new();
        let mut span = spans.iter();
        for (index, item) in items.iter().enumerate() {
            match item {
                Item::Text { style, text, .. } => {
                    let Some(range) = span.next() else { continue };
                    self.text_atoms(
                        Stretch {
                            item: index,
                            text,
                            style: *style,
                            spacing: Spacing {
                                trims: &trims,
                                base: range.start,
                            },
                        },
                        range.start,
                        &opportunities,
                        joined.len(),
                        &mut atoms,
                    );
                }
                Item::Ruby {
                    style,
                    base,
                    annotation_style,
                    annotation,
                    ..
                } => {
                    span.next();
                    self.ruby_atom(
                        index,
                        base,
                        *style,
                        annotation,
                        *annotation_style,
                        &mut atoms,
                    );
                }
                Item::Replaced {
                    inline_size,
                    block_size,
                    ..
                } => atoms.push(Atom {
                    advance: *inline_size,
                    ascent: *block_size,
                    line_height: *block_size,
                    ..Atom::empty(index)
                }),
                Item::Break { .. } => atoms.push(Atom {
                    mandatory: true,
                    ..Atom::empty(index)
                }),
            }
        }
        atoms
    }

    /// One ruby group: base and annotation shaped together into a single
    /// atom: a line never breaks between a word and its reading.
    fn ruby_atom(
        &mut self,
        item: usize,
        base: &str,
        style: TextStyle<'_>,
        annotation: &str,
        annotation_style: TextStyle<'_>,
        out: &mut Vec<Atom>,
    ) {
        // A ruby group's text sits outside the paragraph's own run.
        let mut shaped = Vec::new();
        self.shape_into(
            Stretch {
                item,
                text: base,
                style,
                spacing: Spacing::none(),
            },
            0..base.len(),
            &mut shaped,
        );
        let mut atom = merge_atoms(item, shaped);

        let mut marks = Vec::new();
        self.shape_into(
            Stretch {
                item,
                text: annotation,
                style: annotation_style,
                spacing: Spacing::none(),
            },
            0..annotation.len(),
            &mut marks,
        );
        let marks = merge_atoms(item, marks);
        if let Some(run) = marks.run {
            atom.annotation_advance = marks.advance;
            atom.annotation_size = annotation_style.font_size;
            atom.annotation = Some(run);
            // The group is as long as its longer half, and the shorter one
            // centres against it.
            atom.advance = atom.advance.max(marks.advance);
        }
        atom.line_height = style.line_height;
        out.push(atom);
    }

    /// Split one text item at its break opportunities and shape each piece.
    fn text_atoms(
        &mut self,
        stretch: Stretch<'_, '_>,
        offset: usize,
        opportunities: &[(usize, BreakOpportunity)],
        end: usize,
        out: &mut Vec<Atom>,
    ) {
        let text = stretch.text;
        let mut cut = 0usize;
        for &(at, kind) in opportunities {
            // `linebreaks` reports the byte after the break, in the joined
            // text; only the ones inside this item apply to it.
            let Some(local) = at.checked_sub(offset) else {
                continue;
            };
            if local == 0 || local > text.len() {
                continue;
            }
            // The end of the content is where the text stops, not a break in
            // it: taking it as one leaves an empty line after every block.
            let mandatory = kind == BreakOpportunity::Mandatory && at < end;
            self.piece(stretch, cut..local, mandatory, out);
            cut = local;
        }
        if cut < text.len() {
            self.piece(stretch, cut..text.len(), false, out);
        }
    }

    /// One break-to-break piece, which may split further by face or by
    /// hyphenation before it is shaped.
    fn piece(
        &mut self,
        stretch: Stretch<'_, '_>,
        range: Range<usize>,
        mandatory: bool,
        out: &mut Vec<Atom>,
    ) {
        let Stretch {
            item, text, style, ..
        } = stretch;
        let slice = &text[range.clone()];
        if slice.is_empty() {
            return;
        }
        // A trailing newline is the break itself and draws nothing.
        let visible = slice.trim_end_matches('\n');
        if visible.is_empty() {
            match out.last_mut() {
                Some(last) => last.mandatory |= mandatory,
                None => out.push(Atom {
                    is_space: true,
                    mandatory,
                    line_height: style.line_height,
                    ..Atom::empty(item)
                }),
            }
            return;
        }

        let start = out.len();
        let splits = self.hyphenate(visible, &style);
        if splits.is_empty() {
            let visible_range = range.start..range.start + visible.len();
            self.shape_run(stretch, visible_range, out);
        } else {
            let last_split = splits.len() - 1;
            for (number, split) in splits.into_iter().enumerate() {
                let sub = range.start + split.start..range.start + split.end;
                self.shape_run(stretch, sub, out);
                if number != last_split
                    && out.len() > start
                    && let Some(last) = out.last_mut()
                {
                    last.hyphenated = true;
                }
            }
        }
        if let Some(last) = out.last_mut() {
            last.mandatory |= mandatory;
        }
    }

    /// Shape a stretch and merge it into a single atom. A whole word is one
    /// indivisible piece even where it changes face part way through.
    fn shape_run(&mut self, stretch: Stretch<'_, '_>, range: Range<usize>, out: &mut Vec<Atom>) {
        let mut shaped = Vec::new();
        self.shape_into(stretch, range, &mut shaped);
        match shaped.len() {
            0 => {}
            1 => out.push(shaped.pop().expect("checked")),
            // A piece whose faces differ stays several atoms, adjacent on
            // the line: only a UAX #14 opportunity ends one.
            _ => out.extend(shaped),
        }
    }

    /// Where a word may be broken with a hyphen. Empty when it may not be.
    fn hyphenate(&self, word: &str, style: &TextStyle<'_>) -> Vec<Range<usize>> {
        use bokai::style::Hyphens;

        let Some(hyphenator) = self.hyphenator else {
            return Vec::new();
        };
        // The reading settings hyphenate every reflowable book that says
        // nothing; only an explicit `none` turns it off.
        if style.computed.hyphens == Hyphens::None {
            return Vec::new();
        }
        // Only a run of letters hyphenates; punctuation and spaces carry
        // their own break opportunities.
        if !word.chars().all(char::is_alphabetic) {
            return Vec::new();
        }
        let breaks = hyphenator.hyphenate(word);
        if breaks.is_empty() {
            return Vec::new();
        }
        let mut pieces = Vec::new();
        let mut cut = 0;
        for at in breaks {
            pieces.push(cut..at);
            cut = at;
        }
        pieces.push(cut..word.len());
        pieces
    }

    /// Shape one stretch, splitting where the face has to change.
    fn shape_into(&mut self, stretch: Stretch<'_, '_>, range: Range<usize>, out: &mut Vec<Atom>) {
        let Stretch { text, style, .. } = stretch;
        let slice = &text[range.clone()];
        let mut cut = range.start;
        let mut face = None;

        let mut last: Option<(crate::font::Script, Option<FaceId>)> = None;
        // One run is one face standing one way: a star among kana keeps the
        // face that draws it and the orientation it is read at.
        let mut standing = text::is_upright_in_vertical(slice.chars().next().unwrap_or(' '));
        for (index, ch) in slice.char_indices() {
            let at = range.start + index;
            let script = crate::font::Script::of(ch);
            let chosen = match last {
                Some((held, found)) if held == script => found,
                _ => {
                    let found = self.fonts.select(style.computed, ch);
                    last = Some((script, found));
                    found
                }
            };
            let upright = text::is_upright_in_vertical(ch);
            if face.is_none() {
                face = chosen;
                standing = upright;
                continue;
            }
            if (chosen.is_some() && chosen != face) || upright != standing {
                self.shape_one(stretch, cut..at, face, out);
                cut = at;
                face = chosen.or(face);
                standing = upright;
            }
        }
        if cut < range.end {
            self.shape_one(stretch, cut..range.end, face, out);
        }
    }

    /// One character's atom, taking its shape from [`Fonts::glyph`].
    #[allow(clippy::too_many_arguments)]
    fn one_glyph(
        &mut self,
        stretch: Stretch<'_, '_>,
        at: usize,
        ch: char,
        face: FaceId,
        orientation: Orientation,
        scale: f32,
        out: &mut Vec<Atom>,
    ) {
        let Stretch {
            item,
            style,
            spacing,
            ..
        } = stretch;
        let vertical = orientation == Orientation::Upright;
        let Some(shaped) = self.fonts.glyph(face, vertical, ch) else {
            return;
        };
        let (leading, trailing) = spacing.at(at);
        let cell = text::em_advance(ch, self.axis.is_vertical())
            .map_or(shaped.advance * scale, |ems| ems * style.font_size);
        let step = cell - (leading + trailing) * style.font_size;

        let (along, across) = if vertical {
            (-shaped.y_offset * scale, shaped.x_offset * scale)
        } else {
            (shaped.x_offset * scale, -shaped.y_offset * scale)
        };
        let (ascent, descent) = reach(self.fonts.face(face), style.font_size, orientation);
        let gap = if is_collapsible(ch) {
            style.word_spacing
        } else {
            0.0
        };

        out.push(Atom {
            advance: step + style.letter_spacing + style.embolden + gap,
            run: Some(Run {
                face,
                size: style.font_size,
                color: style.color,
                orientation,
                underline: style.underline,
                line_through: style.line_through,
                glyphs: vec![Glyph {
                    id: shaped.glyph,
                    offset: at as u32,
                    along: along - leading * style.font_size,
                    across,
                }],
            }),
            ascent,
            descent,
            line_height: style.line_height,
            is_space: is_collapsible(ch),
            ..Atom::empty(item)
        });
    }
    /// Shape one stretch that uses a single face.
    fn shape_one(
        &mut self,
        stretch: Stretch<'_, '_>,
        range: Range<usize>,
        face: Option<FaceId>,
        out: &mut Vec<Atom>,
    ) {
        let Stretch {
            item,
            text,
            style,
            spacing,
        } = stretch;
        let range_start = range.start;
        let slice = &text[range];
        if slice.is_empty() {
            return;
        }
        let is_space = slice.chars().all(is_collapsible);
        let orientation = if !self.axis.is_vertical() {
            Orientation::Horizontal
        } else if slice
            .chars()
            .next()
            .is_some_and(text::is_upright_in_vertical)
        {
            Orientation::Upright
        } else {
            Orientation::Sideways
        };

        let vertical = self.axis.is_vertical();
        let Some(face_id) = face else {
            // With no face the run holds `em_advance` of space.
            out.push(Atom {
                advance: slice
                    .char_indices()
                    .map(|(index, ch)| {
                        let (leading, trailing) = spacing.at(range_start + index);
                        let cell = text::em_advance(ch, vertical).unwrap_or(0.5);
                        (cell - leading - trailing) * style.font_size + style.embolden
                    })
                    .sum(),
                ascent: style.font_size * 0.8,
                descent: style.font_size * 0.2,
                line_height: style.line_height,
                is_space,
                ..Atom::empty(item)
            });
            return;
        };

        let loaded = self.fonts.face(face_id);
        let scale = style.font_size / loaded.units_per_em();

        // A CJK line breaks between every pair, leaving one character an atom.
        let mut only = slice.chars();
        if let (Some(ch), None) = (only.next(), only.next()) {
            self.one_glyph(stretch, range_start, ch, face_id, orientation, scale, out);
            return;
        }

        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(slice);
        buffer.guess_segment_properties();
        if orientation == Orientation::Upright {
            buffer.set_direction(rustybuzz::Direction::TopToBottom);
        }
        let shaped = rustybuzz::shape(loaded.shaper(), &[], buffer);

        let mut glyphs = Vec::with_capacity(shaped.len());
        let mut pen = 0.0f32;
        for (info, position) in shaped
            .glyph_infos()
            .iter()
            .zip(shaped.glyph_positions().iter())
        {
            let x_offset = position.x_offset as f32 * scale;
            let y_offset = position.y_offset as f32 * scale;
            let (along, across, shaped_step) = if orientation == Orientation::Upright {
                // Shaping a vertical run puts the pen at each glyph's
                // vertical origin, the top centre of its cell. The offsets
                // carry the shift back to the outline's own origin.
                let advance = (position.y_advance as f32).abs() * scale;
                let step = if advance > 0.0 {
                    advance
                } else {
                    style.font_size
                };
                (pen - y_offset, x_offset, step)
            } else {
                (pen + x_offset, -y_offset, position.x_advance as f32 * scale)
            };

            // `em_advance` overrides the face; a leading trim moves the ink
            // back as well as shortening the cell.
            let at = range_start + info.cluster as usize;
            let ch = text[at..].chars().next();
            let (leading, trailing) = ch.map_or((0.0, 0.0), |_| spacing.at(at));
            let cell = ch
                .and_then(|ch| text::em_advance(ch, vertical))
                .map_or(shaped_step, |ems| ems * style.font_size);
            let step = cell - (leading + trailing) * style.font_size;

            glyphs.push(Glyph {
                id: info.glyph_id as u16,
                offset: at as u32,
                along: along - leading * style.font_size,
                across,
            });
            let gap = if ch.is_some_and(is_collapsible) {
                style.word_spacing
            } else {
                0.0
            };
            pen += step + style.letter_spacing + style.embolden + gap;
        }

        let (ascent, descent) = reach(loaded, style.font_size, orientation);

        out.push(Atom {
            advance: pen,
            run: Some(Run {
                face: face_id,
                size: style.font_size,
                color: style.color,
                orientation,
                underline: style.underline,
                line_through: style.line_through,
                glyphs,
            }),
            ascent,
            descent,
            line_height: style.line_height,
            is_space,
            ..Atom::empty(item)
        });
    }
}

/// How far a run of `face` at `size` reaches either side of the line's
/// baseline. A horizontal run hangs from it by the face's own ascent and
/// descent. An upright glyph on a vertical line sits centred, half an em each
/// way. A quarter-turned one sits centred too, so it reaches half its own
/// height each way and its alphabetic baseline moves off the line's —
/// see [`Inline::run_baseline`].
fn reach(face: &Face, size: f32, orientation: Orientation) -> (f32, f32) {
    match orientation {
        Orientation::Horizontal => (face.ascent(size), face.descent(size)),
        Orientation::Upright => (size / 2.0, size / 2.0),
        Orientation::Sideways => {
            let half = (face.ascent(size) + face.descent(size)) / 2.0;
            (half, half)
        }
    }
}

/// Join consecutive shaped pieces into one atom, keeping the first face.
/// Used where the caller needs a single indivisible piece.
fn merge_atoms(item: usize, atoms: Vec<Atom>) -> Atom {
    let mut merged = Atom::empty(item);
    for atom in atoms {
        if let Some(run) = atom.run {
            match &mut merged.run {
                Some(held) if held.face == run.face && held.size == run.size => {
                    held.glyphs
                        .extend(run.glyphs.into_iter().map(|glyph| Glyph {
                            along: glyph.along + merged.advance,
                            ..glyph
                        }));
                }
                Some(_) => {}
                slot @ None => {
                    *slot = Some(Run {
                        glyphs: run
                            .glyphs
                            .into_iter()
                            .map(|glyph| Glyph {
                                along: glyph.along + merged.advance,
                                ..glyph
                            })
                            .collect(),
                        ..run
                    })
                }
            }
        }
        merged.advance += atom.advance;
        merged.ascent = merged.ascent.max(atom.ascent);
        merged.descent = merged.descent.max(atom.descent);
        merged.line_height = merged.line_height.max(atom.line_height);
    }
    merged
}

// --- Packing atoms into lines --------------------------------------------

/// Atom indices per line.
fn pack(atoms: &[Atom], available: f32, indent: f32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut used = indent;

    for (index, atom) in atoms.iter().enumerate() {
        let first_on_line = index == start;
        // A line always takes at least one atom. An over-long word overflows.
        if !first_on_line && !atom.is_space && used + atom.advance > available + 0.01 {
            lines.push(start..index);
            start = index;
            used = 0.0;
        }
        used += atom.advance;
        if atom.mandatory {
            lines.push(start..index + 1);
            start = index + 1;
            used = 0.0;
        }
    }
    if start < atoms.len() {
        lines.push(start..atoms.len());
    }
    lines
}

// --- Placing lines --------------------------------------------------------

impl Inline<'_> {
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        items: &[Item<'_>],
        atoms: &[Atom],
        lines: &[Range<usize>],
        available: f32,
        indent: f32,
        align: TextAlign,
        fallback_line_height: f32,
    ) -> Lines {
        let mut fragments = Vec::new();
        let mut block = 0.0f32;

        for (number, line) in lines.iter().enumerate() {
            let on_line = &atoms[line.clone()];
            // Trailing whitespace hangs past the line end, off the centring.
            let visible = trim_trailing_spaces(on_line);
            let content: f32 = visible.iter().map(|atom| atom.advance).sum();
            let hyphen = visible.last().is_some_and(|atom| atom.hyphenated);

            let ascent = visible
                .iter()
                .map(|atom| atom.ascent)
                .fold(0.0f32, f32::max);
            let descent = visible
                .iter()
                .map(|atom| atom.descent)
                .fold(0.0f32, f32::max);
            let declared = visible
                .iter()
                .map(|atom| atom.line_height)
                .fold(0.0f32, f32::max)
                .max(fallback_line_height);
            // An annotation claims none of the line's advance.
            let body = declared.max(ascent + descent);
            let block_size = body;
            // Measured from the top of the body, which starts where the ruby
            // strip ends. The text's own box never covers the strip, and no
            // two boxes in the tree claim the same ground.
            let baseline = (body - (ascent + descent)) / 2.0 + ascent;

            let indent = if number == 0 { indent } else { 0.0 };
            let last = number + 1 == lines.len();
            let hyphen_width = if hyphen {
                visible.last().map_or(0.0, |atom| atom.ascent * 0.3)
            } else {
                0.0
            };
            let slack = (available - indent - content - hyphen_width).max(0.0);
            let (start, spread) = match align {
                TextAlign::Center => (indent + slack / 2.0, 0.0),
                TextAlign::Right | TextAlign::End => (indent + slack, 0.0),
                TextAlign::Justify if !last && visible.len() > 1 => {
                    (indent, slack / (visible.len() - 1) as f32)
                }
                _ => (indent, 0.0),
            };

            let mut runs = Vec::new();
            self.emit_line(
                Line {
                    block,
                    body,
                    baseline,
                    ascent,
                    start,
                    spread,
                    hyphen,
                },
                items,
                visible,
                &mut runs,
            );

            // The line spans the column's whole inline size.
            let source = visible
                .first()
                .map_or(NodeId(0), |atom| items[atom.item].source());
            let mut fragment = Fragment::new(
                source,
                Role::Container,
                Rect::new(0.0, block, available, block_size),
            )
            .as_kind(Node::Line);
            fragment.children = runs;
            fragments.push(fragment);

            block += block_size;
        }

        Lines {
            block_size: block,
            fragments,
        }
    }

    /// Draw one line's atoms, joining neighbours that share a face and style
    /// into a single run.
    fn emit_line(&self, line: Line, items: &[Item<'_>], atoms: &[Atom], out: &mut Vec<Fragment>) {
        let body_block = line.block;
        let mut inline = line.start;
        let mut pending: Option<Pending> = None;

        for atom in atoms {
            if let Some(annotation) = &atom.annotation {
                out.push(self.annotate(&line, items, atom, annotation, inline));
            }

            match &atom.run {
                Some(run)
                    if atom.annotation.is_none()
                        && pending
                            .as_ref()
                            .is_some_and(|held| held.continues(atom.item, run)) =>
                {
                    let held = pending.as_mut().expect("checked");
                    let shift = inline - held.inline;
                    held.run.glyphs.extend(run.glyphs.iter().map(|glyph| Glyph {
                        along: glyph.along + shift,
                        ..*glyph
                    }));
                    held.end = inline + atom.advance;
                }
                Some(run) => {
                    self.flush(items, pending.take(), &line, body_block, out);
                    pending = Some(Pending {
                        item: atom.item,
                        inline,
                        end: inline + atom.advance,
                        run: run.clone(),
                    });
                }
                None => {
                    self.flush(items, pending.take(), &line, body_block, out);
                    if let Some(fragment) =
                        self.replaced(items, atom, inline, body_block, line.baseline)
                    {
                        out.push(fragment);
                    }
                }
            }
            inline += atom.advance + line.spread;
        }
        self.flush(items, pending.take(), &line, body_block, out);

        if line.hyphen
            && let Some(atom) = atoms.last()
        {
            self.emit_hyphen(
                items,
                atom,
                inline - line.spread,
                body_block,
                line.body,
                line.baseline,
                out,
            );
        }
    }

    /// A ruby annotation, centred along its base and set against the edge of
    /// its em box.
    fn annotate(
        &self,
        line: &Line,
        items: &[Item<'_>],
        atom: &Atom,
        annotation: &Run,
        inline: f32,
    ) -> Fragment {
        // The strip sits outside the line, at its em boxes' start edge.
        let em_start = line.block + line.baseline - line.ascent;
        let block = em_start - atom.annotation_size;
        // Across its own box: an upright glyph centres on its em box, where a
        // horizontal one hangs from a baseline near the foot of the strip.
        let baseline = if annotation.orientation.is_vertical() {
            atom.annotation_size / 2.0
        } else {
            atom.annotation_size * 0.8
        };
        self.run_fragment(
            items,
            atom.item,
            inline + (atom.advance - atom.annotation_advance) / 2.0,
            block,
            atom.annotation_advance,
            atom.annotation_size,
            baseline,
            annotation.clone(),
        )
    }

    fn flush(
        &self,
        items: &[Item<'_>],
        pending: Option<Pending>,
        line: &Line,
        body_block: f32,
        out: &mut Vec<Fragment>,
    ) {
        let Some(held) = pending else { return };
        out.push(self.run_fragment(
            items,
            held.item,
            held.inline,
            body_block,
            held.end - held.inline,
            line.body,
            line.baseline,
            held.run,
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn run_fragment(
        &self,
        items: &[Item<'_>],
        item: usize,
        inline: f32,
        block: f32,
        advance: f32,
        block_size: f32,
        baseline: f32,
        run: Run,
    ) -> Fragment {
        let source = items[item].source();
        let role = match &items[item] {
            Item::Ruby { .. } => Role::Ruby,
            _ => Role::Text,
        };
        let mut fragment = Fragment::new(
            source,
            role,
            Rect::new(inline, block, advance.max(0.0), block_size),
        );
        fragment.content = Content::Glyphs(GlyphRun {
            face: run.face,
            size: run.size,
            color: run.color,
            orientation: run.orientation,
            baseline: self.run_baseline(&run, baseline),
            glyphs: run.glyphs,
            underline: run.underline,
            line_through: run.line_through,
        });
        fragment.as_kind(Node::Run)
    }

    /// Where `run`'s own baseline sits, `line` being the line's. They are the
    /// same but for a quarter-turned run: it stands centred on the column,
    /// while the letters hang from a baseline half the gap between its ascent
    /// and its descent away. The tops of the letters face the page's right
    /// edge, which is where the line's blocks start in [`Axis::VerticalRl`]
    /// and where they end in [`Axis::VerticalLr`].
    fn run_baseline(&self, run: &Run, line: f32) -> f32 {
        if run.orientation != Orientation::Sideways {
            return line;
        }
        let face = self.fonts.face(run.face);
        let shift = (face.ascent(run.size) - face.descent(run.size)) / 2.0;
        match self.axis {
            Axis::VerticalRl => line + shift,
            _ => line - shift,
        }
    }

    /// The hyphen a broken word takes at the end of its line.
    #[allow(clippy::too_many_arguments)]
    fn emit_hyphen(
        &self,
        items: &[Item<'_>],
        atom: &Atom,
        inline: f32,
        block: f32,
        block_size: f32,
        baseline: f32,
        out: &mut Vec<Fragment>,
    ) {
        let Some(run) = &atom.run else { return };
        let loaded = self.fonts.face(run.face);
        let id = loaded.outline().glyph_id('-');
        if id.0 == 0 {
            return;
        }
        let mut fragment = Fragment::new(
            items[atom.item].source(),
            Role::Text,
            Rect::new(inline, block, atom.ascent * 0.3, block_size),
        );
        fragment.content = Content::Glyphs(GlyphRun {
            face: run.face,
            size: run.size,
            color: run.color,
            orientation: run.orientation,
            glyphs: vec![Glyph {
                id: id.0,
                offset: Glyph::NO_SOURCE,
                along: 0.0,
                across: 0.0,
            }],
            baseline: self.run_baseline(run, baseline),
            underline: false,
            line_through: false,
        });
        out.push(fragment);
    }
}

/// One line's measurements, as placing its atoms needs them.
struct Line {
    /// Where the line starts along the block axis.
    block: f32,
    /// The text's own extent across the line.
    body: f32,
    /// Baseline, measured from the start of the body.
    baseline: f32,
    /// Tallest ascent on the line, which is where its em boxes begin.
    ascent: f32,
    /// Where the first atom starts along the line, and how far justification
    /// pushes each one after it.
    start: f32,
    spread: f32,
    /// Whether the line ends in a broken word.
    hyphen: bool,
}

/// Neighbouring atoms being gathered into one run.
struct Pending {
    item: usize,
    /// Where the run starts along the line, and where its last atom ends.
    inline: f32,
    end: f32,
    run: Run,
}

impl Pending {
    /// Whether an atom can join this run: same source, face, size, colour
    /// and orientation.
    fn continues(&self, item: usize, run: &Run) -> bool {
        self.item == item
            && self.run.face == run.face
            && self.run.size == run.size
            && self.run.color == run.color
            && self.run.orientation == run.orientation
            && self.run.underline == run.underline
            && self.run.line_through == run.line_through
    }
}

impl Inline<'_> {
    fn replaced(
        &self,
        items: &[Item<'_>],
        atom: &Atom,
        inline: f32,
        block: f32,
        baseline: f32,
    ) -> Option<Fragment> {
        let Item::Replaced {
            src,
            background,
            source,
            ..
        } = &items[atom.item]
        else {
            return None;
        };
        let mut fragment = Fragment::new(
            *source,
            Role::Image,
            Rect::new(
                inline,
                block + baseline - atom.ascent,
                atom.advance,
                atom.ascent,
            ),
        );
        fragment.content = Content::Image(src.clone());
        fragment.background = *background;
        Some(fragment)
    }
}

/// A line's atoms with any whitespace at its end removed.
fn trim_trailing_spaces(line: &[Atom]) -> &[Atom] {
    let mut end = line.len();
    while end > 0 && line[end - 1].is_space {
        end -= 1;
    }
    &line[..end]
}

/// Whether a text node's whitespace survives as written.
pub fn preserves_spaces(white_space: WhiteSpace) -> bool {
    matches!(white_space, WhiteSpace::Pre | WhiteSpace::PreWrap)
}

/// Whether a character is whitespace that collapses. U+3000 is Japanese
/// paragraph indentation and U+00A0 a non-breaking space; both are content.
fn is_collapsible(ch: char) -> bool {
    ch.is_whitespace() && ch != '\u{3000}' && ch != '\u{00a0}'
}

/// Collapse a run of whitespace to a single space, as CSS does for
/// `white-space: normal`. A newline survives as a UAX #14 mandatory break.
pub fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch == '\n' {
            out.push('\n');
            in_space = true;
            continue;
        }
        if is_collapsible(ch) {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
            continue;
        }
        in_space = false;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsing_leaves_one_space_and_keeps_newlines() {
        assert_eq!(collapse("a   b"), "a b");
        // A newline survives as the break it is, and the space that follows
        // it collapses into it — a line never begins with one.
        assert_eq!(collapse("a \n b"), "a \nb");
    }

    #[test]
    fn ideographic_space_is_content_not_whitespace() {
        // U+3000 is Japanese paragraph indentation, and is content.
        assert_eq!(collapse("\u{3000}あ"), "\u{3000}あ");
        assert_eq!(collapse("\u{00a0}a"), "\u{00a0}a");
    }

    /// One em.
    const EM: f32 = 44.0;

    /// The inline extent one paragraph of `text` takes with no face loaded.
    fn line_extent(text: &str) -> f32 {
        let computed = ComputedStyle::default();
        let style = TextStyle {
            computed: &computed,
            font_size: EM,
            line_height: EM * 1.2,
            color: Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            letter_spacing: 0.0,
            word_spacing: 0.0,
            embolden: 0.0,
            underline: false,
            line_through: false,
            preserve_spaces: false,
        };
        let items = vec![Item::Text {
            source: NodeId(0),
            style,
            text: text.to_string(),
        }];
        let mut fonts = Fonts::empty();
        let atoms = Inline {
            fonts: &mut fonts,
            axis: Axis::VerticalRl,
            hyphenator: None,
        }
        .atoms(&items);

        atoms.iter().map(|atom| atom.advance).sum()
    }

    #[test]
    fn a_mark_between_ideographs_takes_a_whole_em() {
        assert_eq!(line_extent("亜、亜"), 3.0 * EM);
        assert_eq!(line_extent("亜「亜"), 3.0 * EM);
    }

    #[test]
    fn two_marks_side_by_side_come_to_one_and_a_half_ems() {
        // `、` gives up its trailing blank, `。` keeps its own.
        assert_eq!(line_extent("、。"), 1.5 * EM);
        // Two blanks meet and the following mark gives one up.
        assert_eq!(line_extent("。「"), 1.5 * EM);
        assert_eq!(line_extent("】【"), 1.5 * EM);
    }

    #[test]
    fn a_bracket_pair_facing_ink_to_ink_keeps_both_ems() {
        assert_eq!(line_extent("「」"), 2.0 * EM);
        assert_eq!(line_extent("（）"), 2.0 * EM);
    }

    #[test]
    fn a_mark_beside_a_kana_keeps_its_whole_em() {
        assert_eq!(line_extent("。あ"), 2.0 * EM);
        assert_eq!(line_extent("あ「"), 2.0 * EM);
    }

    #[test]
    fn the_vertical_colon_takes_one_and_a_half_ems() {
        assert_eq!(line_extent("亜：亜"), 3.5 * EM);
    }

    /// A quarter-turned run stands centred on the column, like the upright
    /// glyphs beside it: its own baseline sits half the gap between its
    /// ascent and its descent off the line's, and the line stays centred in
    /// the box it claims.
    #[test]
    fn a_quarter_turned_run_stands_centred_on_the_column() {
        let mut fonts = Fonts::new();
        let computed = ComputedStyle::default();
        if fonts.select(&computed, 'A').is_none() || fonts.select(&computed, '亜').is_none() {
            return; // the host carries no face for one of them
        }
        let style = TextStyle {
            computed: &computed,
            font_size: EM,
            line_height: EM * 1.75,
            color: Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            letter_spacing: 0.0,
            word_spacing: 0.0,
            embolden: 0.0,
            underline: false,
            line_through: false,
            preserve_spaces: false,
        };
        let items = vec![Item::Text {
            source: NodeId(0),
            style,
            text: "亜A亜".to_string(),
        }];
        let lines = Inline {
            fonts: &mut fonts,
            axis: Axis::VerticalRl,
            hyphenator: None,
        }
        .lay_out(&items, 20.0 * EM, 0.0, TextAlign::Start, EM * 1.75);

        let line = lines.fragments.first().expect("one line");
        let runs: Vec<&GlyphRun> = line
            .children
            .iter()
            .filter_map(|child| match &child.content {
                Content::Glyphs(run) => Some(run),
                _ => None,
            })
            .collect();
        let upright = runs
            .iter()
            .find(|run| run.orientation == Orientation::Upright)
            .expect("the ideographs stand upright");
        let sideways = runs
            .iter()
            .find(|run| run.orientation == Orientation::Sideways)
            .expect("the letter lies on its side");

        let face = fonts.face(sideways.face);
        let off_axis = (face.ascent(EM) - face.descent(EM)) / 2.0;
        assert!(
            (sideways.baseline - upright.baseline - off_axis).abs() < 0.01,
            "sideways {} upright {} off_axis {off_axis}",
            sideways.baseline,
            upright.baseline
        );
        assert!(
            (upright.baseline - line.rect.height / 2.0).abs() < 0.01,
            "line baseline {} in a body of {}",
            upright.baseline,
            line.rect.height
        );
    }

    #[test]
    fn a_junction_still_trims_where_the_line_breaker_split_the_pair() {
        // A CJK line breaks between every pair: `、` and `。` are separate
        // atoms, and the trim is read from the paragraph.
        let split: Vec<f32> = {
            let computed = ComputedStyle::default();
            let style = TextStyle {
                computed: &computed,
                font_size: EM,
                line_height: EM * 1.2,
                color: Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                },
                letter_spacing: 0.0,
                word_spacing: 0.0,
                embolden: 0.0,
                underline: false,
                line_through: false,
                preserve_spaces: false,
            };
            let items = vec![
                Item::Text {
                    source: NodeId(0),
                    style,
                    text: "、".to_string(),
                },
                Item::Text {
                    source: NodeId(1),
                    style,
                    text: "。".to_string(),
                },
            ];
            let mut fonts = Fonts::empty();
            Inline {
                fonts: &mut fonts,
                axis: Axis::VerticalRl,
                hyphenator: None,
            }
            .atoms(&items)
            .iter()
            .map(|atom| atom.advance)
            .collect()
        };

        assert_eq!(split, [0.5 * EM, EM]);
    }
}
