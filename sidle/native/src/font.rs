//! Font fallback: which face draws which character.

use std::path::{Path, PathBuf};

use ab_glyph::{Font as _, FontVec};
use anyhow::{Result, anyhow};

/// The regional convention a run of text should be set in. A face sets one;
/// a book asks for one through its language tag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Script {
    /// No preference — either the text's language is unknown or it doesn't
    /// pick out a CJK convention. Also what a pan-Unicode face sets, so
    /// `Unknown` deliberately promotes nothing.
    #[default]
    Unknown,
    Japanese,
    SimplifiedChinese,
    TraditionalChinese,
}

impl Script {
    /// Read a language tag as a preference. Accepts the BCP-47 shapes that
    /// turn up in ebook metadata, `_` or `-` separated and in any case.
    pub fn of_language(tag: &str) -> Script {
        let mut subtags = tag.split(['-', '_']).map(str::trim);
        let primary = subtags.next().unwrap_or_default().to_ascii_lowercase();
        match primary.as_str() {
            // `jp` is a country code rather than a language, but it is what
            // some imported metadata carries.
            "ja" | "jp" => Script::Japanese,
            // Written Cantonese is set in traditional characters — CLDR
            // resolves `yue` to `yue-Hant-HK`.
            "yue" => Script::TraditionalChinese,
            "zh" => {
                for subtag in subtags {
                    match subtag.to_ascii_lowercase().as_str() {
                        "hant" | "tw" | "hk" | "mo" => return Script::TraditionalChinese,
                        "hans" | "cn" | "sg" => return Script::SimplifiedChinese,
                        _ => {}
                    }
                }
                // Bare `zh` is Simplified: CLDR resolves it to `zh-Hans-CN`.
                Script::SimplifiedChinese
            }
            _ => Script::Unknown,
        }
    }
}

/// One face the device might have, and the convention it sets.
pub struct Candidate {
    pub path: &'static str,
    pub script: Script,
}

/// Faces to draw with, best first, at their paths on KOA2 firmware 5.16
/// (`/usr/java/lib/fonts`). Absent entries are skipped when the chain loads,
/// so the list is safe to extend for firmware that ships more.
pub const CANDIDATES: &[Candidate] = &[
    Candidate {
        path: "/usr/java/lib/fonts/TBGothicMed_213.ttf",
        script: Script::Japanese,
    },
    Candidate {
        path: "/usr/java/lib/fonts/STHeitiMedium.ttf",
        script: Script::SimplifiedChinese,
    },
    Candidate {
        path: "/usr/java/lib/fonts/STHeitiTC.ttf",
        script: Script::TraditionalChinese,
    },
    Candidate {
        path: "/usr/java/lib/fonts/code2000.ttf",
        script: Script::Unknown,
    },
];

/// Which face draws a run of text. Made once per string and then consulted
/// per character, so the metrics pass and the blit pass can never disagree
/// about who drew what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// This face has every character in the run — draw it all with that one.
    Whole(usize),
    /// No single face covers the run; resolve per character and accept the
    /// mixed shapes, which still beats a hole in the text. Carries the run's
    /// preference so each character is still resolved in the wanted order.
    PerChar(Script),
}

/// Order to try faces in: the one that sets the wanted convention first, then
/// the rest of the chain as declared.
pub fn visiting_order(faces: usize, promoted: Option<usize>) -> impl Iterator<Item = usize> {
    promoted
        .into_iter()
        .chain((0..faces).filter(move |face| Some(*face) != promoted))
}

/// First face in `order` that has every visible character of `text`.
pub fn covering_face<I, F>(text: &str, order: I, mut has_glyph: F) -> Option<usize>
where
    I: IntoIterator<Item = usize>,
    F: FnMut(usize, char) -> bool,
{
    order.into_iter().find(|&face| {
        text.chars()
            .filter(|c| !is_invisible(*c))
            .all(|c| has_glyph(face, c))
    })
}

/// First face in `order` that has `ch`, or `None` when none does.
pub fn face_with<I, F>(ch: char, order: I, mut has_glyph: F) -> Option<usize>
where
    I: IntoIterator<Item = usize>,
    F: FnMut(usize, char) -> bool,
{
    order.into_iter().find(|&face| has_glyph(face, ch))
}

/// Code points that carry no glyph: the C0/C1 controls, plus the zero-width
/// and formatting family — the BOM and zero-width spaces, bidi marks, the word
/// joiner and invisible operators, and the soft hyphen.
pub fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}'                  // soft hyphen
            | '\u{200B}'..='\u{200F}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
            | '\u{2060}'..='\u{2064}'   // word joiner + invisible operators
            | '\u{FEFF}'                // BOM / zero-width no-break space
        )
}

/// An ordered set of faces: the first usable candidate, read up front, plus
/// the rest of the chain waiting on disk.
pub struct FontChain {
    primary: FontVec,
    primary_path: PathBuf,
    primary_script: Script,
    rest: Vec<Face>,
}

/// A fallback slot. Candidates this firmware doesn't have are dropped when
struct Face {
    path: PathBuf,
    script: Script,
    state: State,
}

enum State {
    /// On disk, not parsed yet.
    Pending,
    Loaded(FontVec),
    /// Could not be read or parsed after all. Skipped from here on, so a bad
    /// candidate costs one failed attempt per session.
    Absent,
}

impl FontChain {
    /// Take the `candidates` this firmware actually has and keep the first
    /// that parses as the primary; the rest become fallbacks, unread until a
    /// character misses.
    pub fn load(candidates: &[Candidate]) -> Result<Self> {
        // Existence is settled here, parsing is not: a stat per candidate is
        // free, and it keeps `paths` an honest account of this device.
        let mut present = candidates
            .iter()
            .filter(|candidate| Path::new(candidate.path).is_file());
        let mut primary = None;
        for candidate in present.by_ref() {
            if let Some(font) = read_face(Path::new(candidate.path)) {
                primary = Some((font, candidate));
                break;
            }
        }
        let Some((primary, first)) = primary else {
            let tried: Vec<&str> = candidates.iter().map(|c| c.path).collect();
            return Err(anyhow!("no usable font among {tried:?}"));
        };
        let rest = present
            .map(|candidate| Face {
                path: PathBuf::from(candidate.path),
                script: candidate.script,
                state: State::Pending,
            })
            .collect();
        Ok(Self {
            primary,
            primary_path: PathBuf::from(first.path),
            primary_script: first.script,
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
    pub fn primary(&self) -> &FontVec {
        &self.primary
    }

    /// Face for the whole of `text`, preferring the one that sets `script` —
    /// see [`covering_face`].
    pub fn select(&mut self, text: &str, script: Script) -> Selection {
        let order = visiting_order(self.faces(), self.promoted(script));
        match covering_face(text, order, |face, ch| {
            self.ensure(face).is_some_and(|font| has_glyph(font, ch))
        }) {
            Some(face) => Selection::Whole(face),
            None => Selection::PerChar(script),
        }
    }

    /// The face index and the face itself for `ch` under `selection`, or
    /// `None` when nothing in the chain has the character. The index is the
    /// glyph cache's key: two faces rasterize the same codepoint differently.
    pub fn glyph_source(&mut self, selection: Selection, ch: char) -> Option<(usize, &FontVec)> {
        let face = match selection {
            Selection::Whole(face) => face,
            Selection::PerChar(script) => {
                let order = visiting_order(self.faces(), self.promoted(script));
                face_with(ch, order, |face, c| {
                    self.ensure(face).is_some_and(|font| has_glyph(font, c))
                })?
            }
        };
        self.ensure(face).map(|font| (face, font))
    }

    /// Chain position of the face that sets `script`, if this device has it.
    fn promoted(&self, script: Script) -> Option<usize> {
        if script == Script::Unknown {
            return None;
        }
        std::iter::once(self.primary_script)
            .chain(self.rest.iter().map(|face| face.script))
            .position(|candidate| candidate == script)
    }

    /// Face `index`, reading it from disk on first use.
    fn ensure(&mut self, index: usize) -> Option<&FontVec> {
        if index == 0 {
            return Some(&self.primary);
        }
        let face = self.rest.get_mut(index - 1)?;
        if matches!(face.state, State::Pending) {
            face.state = match read_face(&face.path) {
                Some(font) => State::Loaded(font),
                None => State::Absent,
            };
        }
        match &face.state {
            State::Loaded(font) => Some(font),
            State::Pending | State::Absent => None,
        }
    }
}

/// Whether `font` can draw `ch` at all. Glyph 0 is `.notdef`, which is what a
/// face hands back for a character it doesn't have — asking for it is how the
/// tofu got drawn in the first place.
pub fn has_glyph(font: &FontVec, ch: char) -> bool {
    font.glyph_id(ch).0 != 0
}

/// Read and parse one candidate. `None` covers both a path this firmware
/// doesn't have and a file that won't parse: either way the answer is "skip
/// this face", not "fail" — the chain only has to keep one.
fn read_face(path: &Path) -> Option<FontVec> {
    let bytes = std::fs::read(path).ok()?;
    FontVec::try_from_vec(bytes).ok()
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

    /// The chain tried in declared order — what a caller that knows nothing
    /// about the text's language gets.
    fn unhinted(faces: &[&str]) -> impl Iterator<Item = usize> {
        visiting_order(faces.len(), None)
    }

    #[test]
    fn a_run_one_face_covers_is_drawn_entirely_by_it() {
        // Every character is in face 0, so the fallback is never consulted —
        // a Japanese title keeps Japanese shapes throughout.
        let faces = ["あいう漢字", "汉字"];
        let face = covering_face("漢字あい", unhinted(&faces), repertoires(&faces));
        assert_eq!(face, Some(0));
    }

    #[test]
    fn a_run_the_first_face_misses_moves_whole_to_the_next() {
        // 楼 is in both faces, 红 only in face 1. Selection is per string, so
        // 楼 is drawn by face 1 too rather than picking up face 0's shapes.
        let faces = ["楼梦", "红楼梦魇"];
        assert_eq!(
            covering_face("红楼梦魇", unhinted(&faces), repertoires(&faces)),
            Some(1)
        );
    }

    #[test]
    fn a_run_no_single_face_covers_resolves_per_character() {
        let faces = ["あ", "汉"];
        assert_eq!(
            covering_face("あ汉", unhinted(&faces), repertoires(&faces)),
            None
        );
        assert_eq!(
            face_with('あ', unhinted(&faces), repertoires(&faces)),
            Some(0)
        );
        assert_eq!(
            face_with('汉', unhinted(&faces), repertoires(&faces)),
            Some(1)
        );
    }

    #[test]
    fn a_character_no_face_has_resolves_to_nothing() {
        let faces = ["あ", "汉"];
        assert_eq!(face_with('𐀀', unhinted(&faces), repertoires(&faces)), None);
    }

    #[test]
    fn invisible_characters_do_not_decide_the_face() {
        // A title carrying a stray BOM must not be pushed off the face that
        // covers its visible text — no face has a glyph for U+FEFF.
        let faces = ["あいう", "汉字"];
        assert_eq!(
            covering_face("あ\u{FEFF}い", unhinted(&faces), repertoires(&faces)),
            Some(0)
        );
        assert!(is_invisible('\u{FEFF}'));
        assert!(!is_invisible('あ'));
    }

    #[test]
    fn control_characters_never_reach_the_rasterizer() {
        // A banner message joins its clauses with `\n`. No face has U+000A, so
        for c in ['\n', '\r', '\t', '\u{0}', '\u{7F}', '\u{85}'] {
            assert!(is_invisible(c), "{c:?} would draw as a box");
        }
        // Nothing in the chain has one, which is the whole damage: the box got
        // drawn, and the miss also pushed the rest of the run off its face.
        let faces = ["Synced 3", "汉字"];
        assert_eq!(face_with('\n', unhinted(&faces), repertoires(&faces)), None);
        assert_eq!(
            covering_face("Synced 3\nSynced 3", unhinted(&faces), repertoires(&faces)),
            Some(0)
        );
    }

    #[test]
    fn an_empty_run_selects_the_first_face() {
        let faces = ["あ"];
        assert_eq!(
            covering_face("", unhinted(&faces), repertoires(&faces)),
            Some(0)
        );
    }

    #[test]
    fn a_hint_moves_its_face_to_the_front_and_keeps_the_rest_in_order() {
        assert_eq!(visiting_order(4, Some(2)).collect::<Vec<_>>(), [2, 0, 1, 3]);
        assert_eq!(visiting_order(4, None).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(visiting_order(4, Some(0)).collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[test]
    fn a_hint_wins_over_a_face_that_would_also_have_covered_the_run() {
        // The whole point of the hint: face 0 (Japanese) has every character
        // of this Traditional title, so coverage alone would leave it there
        // in Japanese shapes. The Traditional face is face 2.
        let faces = ["粵語語法講義", "粤语语法讲义", "粵語語法講義"];
        assert_eq!(
            covering_face("粵語語法講義", visiting_order(3, None), repertoires(&faces)),
            Some(0)
        );
        assert_eq!(
            covering_face(
                "粵語語法講義",
                visiting_order(3, Some(2)),
                repertoires(&faces)
            ),
            Some(2)
        );
    }

    #[test]
    fn a_hinted_face_that_misses_still_loses_to_coverage() {
        // A wrong language tag costs regional shapes, never glyphs.
        let faces = ["紅樓夢魘", "红楼梦魇"];
        assert_eq!(
            covering_face("红楼梦魇", visiting_order(2, Some(0)), repertoires(&faces)),
            Some(1)
        );
    }

    #[test]
    fn language_tags_name_the_convention_they_are_set_in() {
        assert_eq!(Script::of_language("ja"), Script::Japanese);
        // A country code where a language belongs — real imported metadata.
        assert_eq!(Script::of_language("jp"), Script::Japanese);
        assert_eq!(Script::of_language("zh-Hant"), Script::TraditionalChinese);
        assert_eq!(Script::of_language("zh_TW"), Script::TraditionalChinese);
        assert_eq!(Script::of_language("ZH-HK"), Script::TraditionalChinese);
        assert_eq!(Script::of_language("yue"), Script::TraditionalChinese);
        assert_eq!(Script::of_language("zh-Hans"), Script::SimplifiedChinese);
        assert_eq!(Script::of_language("zh-CN"), Script::SimplifiedChinese);
        // Bare `zh` is Simplified, per CLDR's likely subtags.
        assert_eq!(Script::of_language("zh"), Script::SimplifiedChinese);
        // Nothing in the chain sets these, so they express no preference.
        assert_eq!(Script::of_language("en"), Script::Unknown);
        assert_eq!(Script::of_language("ko"), Script::Unknown);
        assert_eq!(Script::of_language(""), Script::Unknown);
    }

    #[test]
    fn every_script_a_tag_can_name_is_set_by_some_candidate() {
        // A hint nothing in the chain can honour is a silent no-op, so the
        // two tables have to stay in step.
        for script in [
            Script::Japanese,
            Script::SimplifiedChinese,
            Script::TraditionalChinese,
        ] {
            assert!(
                CANDIDATES.iter().any(|c| c.script == script),
                "no candidate sets {script:?}"
            );
        }
    }

    #[test]
    fn a_chain_with_no_readable_candidate_fails_to_load() {
        // The one case that is fatal. Individually missing candidates are not
        // (they are simply skipped), but that path needs a real font file and
        // so is only exercised on the device.
        let nowhere = [Candidate {
            path: "/nonexistent/font.ttf",
            script: Script::Japanese,
        }];
        let Err(err) = FontChain::load(&nowhere) else {
            panic!("a chain over a path that isn't there has nothing to draw with");
        };
        assert!(err.to_string().contains("no usable font"));
        assert!(FontChain::load(&[]).is_err());
    }
}
