//! Faces, and choosing one for a run of text.
//!
//! A [`Face`] holds two views of one font: `rustybuzz` shapes with it and
//! `ab_glyph` draws its outlines. Both borrow bytes leaked once on load.
//!
//! [`Fonts::select`] chooses one face per run, from the family the style asks
//! for and the script the text is written in. A character the face has no
//! glyph for moves the whole run to the next candidate.

use std::collections::HashMap;

use ab_glyph::{Font as _, FontRef};
use bokai::style::{ComputedStyle, FontStyle};

/// A loaded face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FaceId(pub u32);

/// Which of the renderer's fallback lists a character belongs to. Latin and
/// CJK need genuinely different faces, and the one the style names is often
/// only right for one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Script {
    Latin,
    Cjk,
}

impl Script {
    /// The script a character is written in, as far as face choice cares.
    pub fn of(ch: char) -> Script {
        if crate::text::is_cjk(ch) {
            Script::Cjk
        } else {
            Script::Latin
        }
    }
}

/// Families tried when the document names none, or names one the host does
/// not have. A Kindle sets body text in a serif, and these lead with one.
const LATIN_FALLBACKS: &[&str] = &[
    "Georgia",
    "Times New Roman",
    "Times",
    "Palatino",
    "Helvetica",
    "Arial",
];

const CJK_FALLBACKS: &[&str] = &[
    "Hiragino Mincho ProN",
    "Hiragino Mincho Pro",
    "YuMincho",
    "Hiragino Sans",
    "Hiragino Sans GB",
    "Songti SC",
    "PingFang SC",
    "Apple SD Gothic Neo",
    "Noto Serif CJK JP",
    "Noto Sans CJK JP",
];

pub struct Face {
    shaper: rustybuzz::Face<'static>,
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

    /// Distance from the baseline to the top of the em box, in CSS pixels at
    /// `size`.
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

/// Every face the renderer has loaded, and the host's font catalogue.
pub struct Fonts {
    db: fontdb::Database,
    faces: Vec<Face>,
    /// Loaded faces, by the catalogue id each came from.
    loaded: HashMap<fontdb::ID, FaceId>,
    /// Past selections, by what was asked for.
    chosen: HashMap<(String, u16, bool, Script), Option<FaceId>>,
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
        }
    }

    /// The host's installed faces.
    pub fn new() -> Self {
        let mut fonts = Self::empty();
        fonts.db.load_system_fonts();
        fonts
    }

    /// Add a face the book carries itself. Embedded faces take precedence
    /// over the host's, being what the publisher asked for.
    pub fn add_embedded(&mut self, data: Vec<u8>) {
        self.db.load_font_data(data);
        self.chosen.clear();
    }

    pub fn face(&self, id: FaceId) -> &Face {
        &self.faces[id.0 as usize]
    }

    /// A face for text in `style`, written in the script `ch` belongs to.
    /// `None` only when the host has no usable face at all.
    pub fn select(&mut self, style: &ComputedStyle, ch: char) -> Option<FaceId> {
        let script = Script::of(ch);
        let families = style.font_family.clone().unwrap_or_default();
        let weight = style.font_weight.0;
        let italic = style.font_style != FontStyle::Normal;

        let key = (families.clone(), weight, italic, script);
        if let Some(&cached) = self.chosen.get(&key) {
            return cached;
        }
        let chosen = self.resolve(&families, weight, italic, script, ch);
        self.chosen.insert(key, chosen);
        chosen
    }

    /// Walk the candidate families in order and take the first face that has
    /// a glyph for `ch`.
    fn resolve(
        &mut self,
        families: &str,
        weight: u16,
        italic: bool,
        script: Script,
        ch: char,
    ) -> Option<FaceId> {
        let declared: Vec<String> = families
            .split(',')
            .map(|name| name.trim().trim_matches(['"', '\'']).to_string())
            .filter(|name| !name.is_empty())
            .collect();
        let fallbacks = match script {
            Script::Latin => LATIN_FALLBACKS,
            Script::Cjk => CJK_FALLBACKS,
        };

        let mut best = None;
        for name in declared
            .iter()
            .map(String::as_str)
            .chain(fallbacks.iter().copied())
        {
            let Some(id) = self.query(name, weight, italic) else {
                continue;
            };
            let Some(face) = self.load(id) else { continue };
            if self.faces[face.0 as usize].covers(ch) {
                return Some(face);
            }
            best.get_or_insert(face);
        }

        // Nothing covers the character: any real face beats drawing nothing,
        // and a missing glyph is visible as such.
        best.or_else(|| self.any(weight, italic).and_then(|id| self.load(id)))
    }

    fn query(&self, family: &str, weight: u16, italic: bool) -> Option<fontdb::ID> {
        self.db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight: fontdb::Weight(weight),
            stretch: fontdb::Stretch::Normal,
            style: if italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
        })
    }

    fn any(&self, weight: u16, italic: bool) -> Option<fontdb::ID> {
        self.db.query(&fontdb::Query {
            families: &[fontdb::Family::Serif, fontdb::Family::SansSerif],
            weight: fontdb::Weight(weight),
            stretch: fontdb::Stretch::Normal,
            style: if italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
        })
    }

    /// The face made from catalogue entry `id`, loading its bytes on the
    /// first call.
    fn load(&mut self, id: fontdb::ID) -> Option<FaceId> {
        if let Some(&existing) = self.loaded.get(&id) {
            return Some(existing);
        }
        let (data, index) = self
            .db
            .with_face_data(id, |data, index| (data.to_vec(), index))?;
        // The renderer holds its faces for as long as it runs, and both
        // views borrow the bytes.
        let data: &'static [u8] = Vec::leak(data);
        let shaper = rustybuzz::Face::from_slice(data, index)?;
        let outline = FontRef::try_from_slice_and_index(data, index).ok()?;

        let face = FaceId(self.faces.len() as u32);
        self.faces.push(Face { shaper, outline });
        self.loaded.insert(id, face);
        Some(face)
    }
}
