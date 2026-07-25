//! Font fallback: which face draws which character.
//!
//! The device ships one face per script family and no single one covers the
//! library. The Japanese sans we draw with has no glyph for the PRC-only
//! simplifications (纟 讠 饣 钅 车 马 鸟 见 页 贝 门 …) — they were never
//! adopted in Japan and are in no JIS standard — so a Simplified Chinese
//! title drawn from it alone comes out part text, part `.notdef`. A few
//! simplified forms *are* in JIS X 0208/0212 (楼, 梦, 达), which is why the
//! damage looks random rather than total.
//!
//! A face is chosen per string, not per character. Han unification gives one
//! codepoint to shapes that differ by region (直, 骨, 今, 令 …), so resolving
//! character by character would draw a Chinese title with Japanese shapes
//! wherever the Japanese face happens to have the codepoint and Chinese
//! shapes elsewhere — inconsistent inside one title, and the wrong regional
//! convention for the book. Whole-string selection keeps one convention per
//! title and drops to per-character only when no single face covers the run.
//!
//! Fallback faces are parsed on first miss, never at startup: fontdue
//! outlines every glyph in a face's cmap when it reads the file, and the
//! Chinese faces are ~10 MB against the Japanese face's 3.8 MB. A shelf of
//! Japanese and Latin titles never touches them.
//!
//! The selection policy is a free function over a coverage oracle so it can
//! be exercised on the host, where none of these faces exist.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use fontdue::{Font, FontSettings};

/// Faces to draw with, best first, at their paths on KOA2 firmware 5.16
/// (`/usr/java/lib/fonts`). Absent entries are skipped when the chain loads,
/// so the list is safe to extend for firmware that ships more.
///
/// Japanese leads: it is the bulk of the library, and a Chinese face would
/// hand Japanese text Chinese shapes. `STHeitiMedium` (Simplified) and
/// `STHeitiTC` (Traditional) are Heiti sans at the same *Medium* weight as
/// `TBGothicMed` — weight matters because the renderer thresholds coverage to
/// one bit and a Regular face thins out under the cut. `code2000` is a
/// pan-Unicode catch-all and the last resort before the missing-glyph mark.
pub const CANDIDATES: &[&str] = &[
    "/usr/java/lib/fonts/TBGothicMed_213.ttf",
    "/usr/java/lib/fonts/STHeitiMedium.ttf",
    "/usr/java/lib/fonts/STHeitiTC.ttf",
    "/usr/java/lib/fonts/code2000.ttf",
];

/// Which face draws a run of text. Made once per string and then consulted
/// per character, so the metrics pass and the blit pass can never disagree
/// about who drew what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// This face has every character in the run — draw it all with that one.
    Whole(usize),
    /// No single face covers the run; resolve per character and accept the
    /// mixed shapes, which still beats a hole in the text.
    PerChar,
}

/// First of `faces` that has every visible character of `text`, else
/// [`Selection::PerChar`].
///
/// `has_glyph(face, ch)` answers coverage; it is called in chain order and
/// only until a face misses, so a face that no string needs is never
/// consulted (and, in the renderer, never read from disk).
pub fn select<F>(text: &str, faces: usize, mut has_glyph: F) -> Selection
where
    F: FnMut(usize, char) -> bool,
{
    for face in 0..faces {
        if text
            .chars()
            .filter(|c| !is_invisible(*c))
            .all(|c| has_glyph(face, c))
        {
            return Selection::Whole(face);
        }
    }
    Selection::PerChar
}

/// Face that draws `ch`, or `None` when no face in the chain has it.
///
/// `selection` must be the one [`select`] made for a string containing `ch`:
/// under [`Selection::Whole`] that face is already known to cover the run, so
/// the oracle is not consulted again.
pub fn face_for_char<F>(
    selection: Selection,
    ch: char,
    faces: usize,
    mut has_glyph: F,
) -> Option<usize>
where
    F: FnMut(usize, char) -> bool,
{
    match selection {
        Selection::Whole(face) => Some(face),
        Selection::PerChar => (0..faces).find(|&face| has_glyph(face, ch)),
    }
}

/// Zero-width / formatting code points that carry no glyph: the BOM and
/// zero-width space family, bidi marks, the word joiner and invisible
/// operators, and the soft hyphen.
///
/// They must never reach the rasterizer. A face has no glyph for most of
/// them, and a font answers "no glyph" by handing back `.notdef` — so a
/// character that is invisible everywhere else in the product turns into a
/// visible box here. They are also collation-ignorable, which is why
/// [`crate::api`] drops them from the fields the picker sorts on.
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}'                  // soft hyphen
        | '\u{200B}'..='\u{200F}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{2060}'..='\u{2064}'   // word joiner + invisible operators
        | '\u{FEFF}'                // BOM / zero-width no-break space
    )
}

/// An ordered set of faces: the first usable candidate, read up front, plus
/// the rest of the chain waiting on disk.
pub struct FontChain {
    primary: Font,
    primary_path: PathBuf,
    rest: Vec<Face>,
}

/// A fallback slot. Candidates this firmware doesn't have are dropped when
/// the chain loads rather than kept here, so the chain is exactly the font
/// files the device offers, in order. The path outlives the read: it is what
/// [`FontChain::paths`] reports.
struct Face {
    path: PathBuf,
    state: State,
}

enum State {
    /// On disk, not parsed yet.
    Pending,
    /// Boxed so an unparsed slot stays a word wide — a parsed face is 200-odd
    /// bytes and most of the chain never loads.
    Loaded(Box<Font>),
    /// Could not be read or parsed after all. Skipped from here on, so a bad
    /// candidate costs one failed attempt per session.
    Absent,
}

impl FontChain {
    /// Take the `candidates` this firmware actually has and keep the first
    /// that parses as the primary; the rest become fallbacks, unread until a
    /// character misses.
    ///
    /// Fails only when none of them loads. A firmware that has moved or
    /// dropped one face is not a reason to refuse to start — the picker draws
    /// with whatever it finds, and only an empty chain has nothing to say.
    pub fn load(candidates: &[&str]) -> Result<Self> {
        // Existence is settled here, parsing is not: a stat per candidate is
        // free, and it keeps `paths` an honest account of this device.
        let mut present = candidates
            .iter()
            .map(Path::new)
            .filter(|path| path.is_file());
        let mut primary = None;
        for path in present.by_ref() {
            if let Some(font) = read_face(path) {
                primary = Some((font, path.to_path_buf()));
                break;
            }
        }
        let Some((primary, primary_path)) = primary else {
            return Err(anyhow!("no usable font among {candidates:?}"));
        };
        let rest = present
            .map(|path| Face {
                path: path.to_path_buf(),
                state: State::Pending,
            })
            .collect();
        Ok(Self {
            primary,
            primary_path,
            rest,
        })
    }

    /// Faces in the chain, read or not. Never zero — [`FontChain::load`]
    /// fails rather than hand back a chain that can't draw.
    pub fn faces(&self) -> usize {
        1 + self.rest.len()
    }

    /// The chain as it stands on this device, primary first. Logged at
    /// startup: a firmware that has moved or dropped a face otherwise shows
    /// up only as glyphs that don't draw.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.primary_path.as_path())
            .chain(self.rest.iter().map(|face| face.path.as_path()))
    }

    /// The face line metrics come from, so every row is the same height
    /// whichever face ends up drawing it.
    pub fn primary(&self) -> &Font {
        &self.primary
    }

    /// Face for the whole of `text` — see [`select`].
    pub fn select(&mut self, text: &str) -> Selection {
        let faces = self.faces();
        select(text, faces, |face, ch| {
            self.ensure(face).is_some_and(|font| font.has_glyph(ch))
        })
    }

    /// The face index and the face itself for `ch` under `selection`, or
    /// `None` when nothing in the chain has the character. The index is the
    /// glyph cache's key: two faces rasterize the same codepoint differently.
    pub fn glyph_source(&mut self, selection: Selection, ch: char) -> Option<(usize, &Font)> {
        let faces = self.faces();
        let face = face_for_char(selection, ch, faces, |face, c| {
            self.ensure(face).is_some_and(|font| font.has_glyph(c))
        })?;
        self.ensure(face).map(|font| (face, font))
    }

    /// Face `index`, reading it from disk on first use.
    fn ensure(&mut self, index: usize) -> Option<&Font> {
        if index == 0 {
            return Some(&self.primary);
        }
        let face = self.rest.get_mut(index - 1)?;
        if matches!(face.state, State::Pending) {
            face.state = match read_face(&face.path) {
                Some(font) => State::Loaded(Box::new(font)),
                None => State::Absent,
            };
        }
        match &face.state {
            State::Loaded(font) => Some(font.as_ref()),
            State::Pending | State::Absent => None,
        }
    }
}

/// Read and parse one candidate. `None` covers both a path this firmware
/// doesn't have and a file fontdue can't parse: either way the answer is
/// "skip this face", not "fail" — the chain only has to keep one.
fn read_face(path: &Path) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Coverage oracle over face repertoires spelled as strings: face 0
    /// stands in for the Japanese face, face 1 for the Simplified one, and
    /// so on down the chain.
    fn repertoires<'a>(faces: &'a [&'a str]) -> impl FnMut(usize, char) -> bool + 'a {
        move |face, ch| faces[face].contains(ch)
    }

    #[test]
    fn a_run_one_face_covers_is_drawn_entirely_by_it() {
        // Every character is in face 0, so the fallback is never consulted —
        // a Japanese title keeps Japanese shapes throughout.
        let faces = ["あいう漢字", "汉字"];
        let sel = select("漢字あい", faces.len(), repertoires(&faces));
        assert_eq!(sel, Selection::Whole(0));
    }

    #[test]
    fn a_run_the_first_face_misses_moves_whole_to_the_next() {
        // 楼 is in both faces, 红 only in face 1. Selection is per string, so
        // 楼 is drawn by face 1 too rather than picking up face 0's shapes.
        let faces = ["楼梦", "红楼梦魇"];
        let sel = select("红楼梦魇", faces.len(), repertoires(&faces));
        assert_eq!(sel, Selection::Whole(1));
        assert_eq!(
            face_for_char(sel, '楼', faces.len(), repertoires(&faces)),
            Some(1)
        );
    }

    #[test]
    fn a_run_no_single_face_covers_resolves_per_character() {
        let faces = ["あ", "汉"];
        let sel = select("あ汉", faces.len(), repertoires(&faces));
        assert_eq!(sel, Selection::PerChar);
        assert_eq!(
            face_for_char(sel, 'あ', faces.len(), repertoires(&faces)),
            Some(0)
        );
        assert_eq!(
            face_for_char(sel, '汉', faces.len(), repertoires(&faces)),
            Some(1)
        );
    }

    #[test]
    fn a_character_no_face_has_resolves_to_nothing() {
        let faces = ["あ", "汉"];
        let sel = select("あ𐀀", faces.len(), repertoires(&faces));
        assert_eq!(sel, Selection::PerChar);
        assert_eq!(
            face_for_char(sel, '𐀀', faces.len(), repertoires(&faces)),
            None
        );
    }

    #[test]
    fn invisible_characters_do_not_decide_the_face() {
        // A title carrying a stray BOM must not be pushed off the face that
        // covers its visible text — no face has a glyph for U+FEFF.
        let faces = ["あいう", "汉字"];
        let sel = select("あ\u{FEFF}い", faces.len(), repertoires(&faces));
        assert_eq!(sel, Selection::Whole(0));
        assert!(is_invisible('\u{FEFF}'));
        assert!(!is_invisible('あ'));
    }

    #[test]
    fn an_empty_run_selects_the_primary() {
        let faces = ["あ"];
        assert_eq!(
            select("", faces.len(), repertoires(&faces)),
            Selection::Whole(0)
        );
    }

    #[test]
    fn a_chain_with_no_readable_candidate_fails_to_load() {
        // The one case that is fatal. Individually missing candidates are not
        // (they are simply skipped), but that path needs a real font file and
        // so is only exercised on the device.
        let Err(err) = FontChain::load(&["/nonexistent/font.ttf"]) else {
            panic!("a chain over a path that isn't there has nothing to draw with");
        };
        assert!(err.to_string().contains("no usable font"));
        assert!(FontChain::load(&[]).is_err());
    }
}
