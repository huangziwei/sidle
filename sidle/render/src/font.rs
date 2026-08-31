//! [`Fonts::select`] picks a [`Face`] for a run: the names a stack states,
//! then [`Fonts::reading_family`], then coverage. A stack headed by
//! [`DEFERRING_HEAD`] states none. No family name is compiled in.

use std::collections::HashMap;
use std::path::Path;

use ab_glyph::{Font as _, FontRef};
use bokai::style::{ComputedStyle, FontStyle, is_generic_font_keyword};

/// A loaded face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FaceId(pub u32);

/// Which fallback `ch` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Script {
    Latin,
    Cjk,
}

impl Script {
    /// The `Script` `ch` is written in.
    pub fn of(ch: char) -> Script {
        if crate::text::is_cjk(ch) {
            Script::Cjk
        } else {
            Script::Latin
        }
    }
}

/// The head a stack carries where `reading_family` decides.
pub const DEFERRING_HEAD: &str = "default";

pub struct Face {
    pub(crate) shaper: rustybuzz::Face<'static>,
    outline: FontRef<'static>,
}

impl Face {
    /// The shaping view.
    pub fn shaper(&self) -> &rustybuzz::Face<'static> {
        &self.shaper
    }

    /// The outline view, for rasterization.
    pub fn outline(&self) -> &FontRef<'static> {
        &self.outline
    }

    /// Design units per em, the divisor that turns shaped advances into
    /// fractions of the font size.
    pub fn units_per_em(&self) -> f32 {
        self.outline.units_per_em().unwrap_or(1000.0)
    }

    /// Distance from the baseline to the top of the em box, at `size`.
    pub fn ascent(&self, size: f32) -> f32 {
        self.outline.ascent_unscaled() / self.units_per_em() * size
    }

    /// Distance from the baseline to the bottom, positive downward.
    pub fn descent(&self, size: f32) -> f32 {
        -self.outline.descent_unscaled() / self.units_per_em() * size
    }

    /// Whether the face has a glyph for `ch`.
    pub fn covers(&self, ch: char) -> bool {
        self.outline.glyph_id(ch).0 != 0
    }
}

/// What `select` caches a choice under.
type Request = (String, u16, bool, Script);

/// What `select` caches a lone character's face under.
type OddRequest = (String, u16, bool, char);

/// Loaded [`Face`]s and the catalogue `select` searches.
pub struct Fonts {
    db: fontdb::Database,
    faces: Vec<Face>,
    /// Loaded faces, by the catalogue id each came from.
    loaded: HashMap<fontdb::ID, FaceId>,
    /// Past selections, by what was asked for.
    chosen: HashMap<Request, Option<FaceId>>,
    /// Faces found for a character the script's own face misses.
    covering: HashMap<OddRequest, Option<FaceId>>,
    /// Single characters shaped once each.
    shaped: HashMap<(FaceId, bool, char), Option<Shaped>>,
    /// The families the reading settings chose, per script.
    reading: HashMap<Script, Vec<String>>,
    /// Families no name and no script may reach — only a character nothing
    /// else covers.
    coverage_only: Vec<String>,
}

impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

impl Fonts {
    /// An empty catalogue. Faces must be added before anything can be shaped.
    pub fn empty() -> Self {
        Self {
            db: fontdb::Database::new(),
            faces: Vec::new(),
            loaded: HashMap::new(),
            chosen: HashMap::new(),
            covering: HashMap::new(),
            shaped: HashMap::new(),
            reading: HashMap::new(),
            coverage_only: Vec::new(),
        }
    }

    /// The host's installed faces.
    pub fn new() -> Self {
        let mut fonts = Self::empty();
        fonts.db.load_system_fonts();
        fonts
    }

    /// Add every face under `path`, searched ahead of the host's own.
    pub fn add_directory(&mut self, path: impl AsRef<Path>) {
        self.db.load_fonts_dir(path);
        self.chosen.clear();
        self.covering.clear();
    }

    /// Add a face a book carries, ahead of the host's.
    pub fn add_embedded(&mut self, data: Vec<u8>) {
        self.db.load_font_data(data);
        self.chosen.clear();
        self.covering.clear();
    }

    /// The families `select` falls to for `script`, best first.
    pub fn reading_family(&mut self, script: Script, families: &[&str]) {
        self.reading.insert(
            script,
            families.iter().map(|name| (*name).to_string()).collect(),
        );
        self.chosen.clear();
        self.covering.clear();
    }

    /// Bar `family` from every route but `any_covering`.
    pub fn only_by_coverage(&mut self, family: &str) {
        self.coverage_only.push(family.to_string());
        self.chosen.clear();
        self.covering.clear();
    }

    pub fn face(&self, id: FaceId) -> &Face {
        &self.faces[id.0 as usize]
    }

    /// Whether any of `families` names a face of the host's carrying
    /// `sample`.
    pub fn carries(&mut self, families: &[String], sample: char) -> bool {
        for name in families {
            let Some(id) = self.query(name, 400, false) else {
                continue;
            };
            let Some(face) = self.load(id) else { continue };
            if self.faces[face.0 as usize].covers(sample) {
                return true;
            }
        }
        false
    }

    /// A face for `style` and `ch`, `None` on an empty catalogue.
    pub fn select(&mut self, style: &ComputedStyle, ch: char) -> Option<FaceId> {
        let script = Script::of(ch);
        let families = style.font_family.clone().unwrap_or_default();
        let weight = style.font_weight.0;
        let italic = style.font_style != FontStyle::Normal;

        let key = (families.clone(), weight, italic, script);
        if let Some(&cached) = self.chosen.get(&key)
            && cached.is_none_or(|id| self.faces[id.0 as usize].covers(ch))
        {
            return cached;
        }
        // A character the script's own face has no glyph for is searched for
        // by itself: a star among Latin letters, a symbol among kana.
        let odd = (families.clone(), weight, italic, ch);
        if let Some(&cached) = self.covering.get(&odd) {
            return cached;
        }
        let chosen = self.resolve(&families, weight, italic, script, ch);
        match self.chosen.contains_key(&key) {
            true => self.covering.insert(odd, chosen),
            false => self.chosen.insert(key, chosen),
        };
        chosen
    }

    /// The first candidate face with a glyph for `ch`.
    fn resolve(
        &mut self,
        families: &str,
        weight: u16,
        italic: bool,
        script: Script,
        ch: char,
    ) -> Option<FaceId> {
        let declared = stack(families);
        let reading = self.reading.get(&script).cloned().unwrap_or_default();

        // A deferring stack contributes no name of its own.
        let mut candidates: Vec<String> = Vec::new();
        if !defers(&declared) {
            candidates.extend(declared);
        }
        candidates.extend(reading);

        let mut best = None;
        for name in candidates {
            if self.coverage_only.contains(&name) {
                continue;
            }
            let Some(id) = self.query(&name, weight, italic) else {
                continue;
            };
            let Some(face) = self.load(id) else { continue };
            if self.faces[face.0 as usize].covers(ch) {
                return Some(face);
            }
            best.get_or_insert(face);
        }

        // Past the names: the generic family, then every face there is. A
        // character none of them covers keeps the run's own face.
        self.by_generic(script, weight, italic, ch)
            .or_else(|| self.any_covering(ch))
            .or(best)
            .or_else(|| self.any_face(weight, italic))
    }

    /// The generic family's face for `ch`.
    fn by_generic(
        &mut self,
        script: Script,
        weight: u16,
        italic: bool,
        ch: char,
    ) -> Option<FaceId> {
        let generics: &[fontdb::Family] = match script {
            Script::Latin => &[fontdb::Family::Serif, fontdb::Family::SansSerif],
            Script::Cjk => &[fontdb::Family::Serif, fontdb::Family::SansSerif],
        };
        for family in generics {
            let Some(id) = self.db.query(&fontdb::Query {
                families: &[*family],
                weight: fontdb::Weight(weight),
                stretch: fontdb::Stretch::Normal,
                style: slant(italic),
            }) else {
                continue;
            };
            let Some(face) = self.load(id) else { continue };
            if self.faces[face.0 as usize].covers(ch) {
                return Some(face);
            }
        }
        None
    }

    /// Any loadable face covering `ch`.
    fn any_covering(&mut self, ch: char) -> Option<FaceId> {
        let ids: Vec<fontdb::ID> = self.db.faces().map(|info| info.id).collect();
        for id in ids {
            let Some(face) = self.load(id) else { continue };
            if self.faces[face.0 as usize].covers(ch) {
                return Some(face);
            }
        }
        None
    }

    /// A face for a character no face has a glyph for, which draws a visible
    /// `.notdef`.
    fn any_face(&mut self, weight: u16, italic: bool) -> Option<FaceId> {
        self.db
            .query(&fontdb::Query {
                families: &[fontdb::Family::Serif, fontdb::Family::SansSerif],
                weight: fontdb::Weight(weight),
                stretch: fontdb::Stretch::Normal,
                style: slant(italic),
            })
            .and_then(|id| self.load(id))
    }

    fn query(&self, family: &str, weight: u16, italic: bool) -> Option<fontdb::ID> {
        let named;
        let family = if is_generic_font_keyword(family) {
            generic(family)
        } else {
            named = family;
            fontdb::Family::Name(named)
        };
        self.db.query(&fontdb::Query {
            families: &[family],
            weight: fontdb::Weight(weight),
            stretch: fontdb::Stretch::Normal,
            style: slant(italic),
        })
    }

    /// The face for catalogue entry `id`, reading its bytes once.
    fn load(&mut self, id: fontdb::ID) -> Option<FaceId> {
        if let Some(&existing) = self.loaded.get(&id) {
            return Some(existing);
        }
        let (data, index) = self
            .db
            .with_face_data(id, |data, index| (data.to_vec(), index))?;
        // `shaper` and `outline` both borrow these bytes for the process.
        let data: &'static [u8] = Vec::leak(data);
        let shaper = rustybuzz::Face::from_slice(data, index)?;
        let outline = FontRef::try_from_slice_and_index(data, index).ok()?;

        let face = FaceId(self.faces.len() as u32);
        self.faces.push(Face { shaper, outline });
        self.loaded.insert(id, face);
        Some(face)
    }
}

/// A `font-family` value as the names it asks for.
fn stack(families: &str) -> Vec<String> {
    families
        .split(',')
        .map(|name| name.trim().trim_matches(['"', '\'']).to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether `stack` hands the choice to `reading_family`.
fn defers(stack: &[String]) -> bool {
    stack
        .first()
        .is_none_or(|head| head.eq_ignore_ascii_case(DEFERRING_HEAD))
}

/// The catalogue generic `keyword` names.
fn generic(keyword: &str) -> fontdb::Family<'static> {
    match keyword.trim().to_ascii_lowercase().as_str() {
        name if name.starts_with("sans-serif") => fontdb::Family::SansSerif,
        name if name.starts_with("monospace") => fontdb::Family::Monospace,
        name if name.starts_with("cursive") => fontdb::Family::Cursive,
        name if name.starts_with("fantasy") => fontdb::Family::Fantasy,
        _ => fontdb::Family::Serif,
    }
}

fn slant(italic: bool) -> fontdb::Style {
    if italic {
        fontdb::Style::Italic
    } else {
        fontdb::Style::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_headed_by_default_defers() {
        assert!(defers(&stack("default")));
        assert!(defers(&stack("default,Baskerville,serif")));
        assert!(defers(&stack("DEFAULT, serif")));
        assert!(defers(&stack("")));
    }

    #[test]
    fn a_stack_naming_a_family_first_does_not_defer() {
        assert!(!defers(&stack("Baskerville,default")));
        assert!(!defers(&stack("\"Hiragino Mincho ProN\", serif")));
    }

    #[test]
    fn a_stack_keeps_the_spaces_inside_a_name() {
        assert_eq!(
            stack("\"Times New Roman\", serif"),
            ["Times New Roman", "serif"]
        );
    }

    #[test]
    fn a_generic_keyword_reads_through_its_locale_cut() {
        assert_eq!(generic("sans-serif-ja"), fontdb::Family::SansSerif);
        assert_eq!(generic("serif"), fontdb::Family::Serif);
        assert_eq!(generic("monospace"), fontdb::Family::Monospace);
    }

    #[test]
    fn an_empty_catalogue_selects_nothing() {
        let mut fonts = Fonts::empty();
        let style = ComputedStyle::default();

        assert_eq!(fonts.select(&style, 'a'), None);
    }

    #[test]
    fn a_character_the_reading_face_misses_takes_one_that_covers_it() {
        // `★` between kana: no Latin reading face carries it, and the run it
        // sits in has settled on one from the letters around it.
        let mut fonts = Fonts::new();
        let style = ComputedStyle::default();
        fonts.select(&style, 'a');

        let Some(face) = fonts.select(&style, '★') else {
            return;
        };

        assert!(fonts.face(face).covers('★'));
    }

    #[test]
    fn a_barred_family_is_not_reached_by_name() {
        let mut fonts = Fonts::empty();
        fonts.only_by_coverage("Code2000");

        assert!(fonts.coverage_only.contains(&"Code2000".to_string()));
    }
}

/// One character shaped on its own, in the face's own design units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shaped {
    pub glyph: u16,
    pub advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
}

impl Fonts {
    /// `ch` shaped alone in `face`, kept for every later occurrence.
    ///
    /// A CJK line breaks between every pair; the same few thousand characters
    /// recur all book.
    pub fn glyph(&mut self, face: FaceId, vertical: bool, ch: char) -> Option<Shaped> {
        if let Some(&held) = self.shaped.get(&(face, vertical, ch)) {
            return held;
        }
        let loaded = &self.faces[face.0 as usize];
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(ch.encode_utf8(&mut [0u8; 4]));
        buffer.guess_segment_properties();
        if vertical {
            buffer.set_direction(rustybuzz::Direction::TopToBottom);
        }
        let run = rustybuzz::shape(&loaded.shaper, &[], buffer);
        let shaped = run
            .glyph_infos()
            .first()
            .zip(run.glyph_positions().first())
            .map(|(info, position)| Shaped {
                glyph: info.glyph_id as u16,
                advance: if vertical {
                    (position.y_advance as f32).abs()
                } else {
                    position.x_advance as f32
                },
                x_offset: position.x_offset as f32,
                y_offset: position.y_offset as f32,
            });
        self.shaped.insert((face, vertical, ch), shaped);
        shaped
    }
}
