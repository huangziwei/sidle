//! A book's credits — the names it credits, each with its roles — and the author
//! line drawn from them, with the name normalization every entry path shares; and
//! the people those names belong to, where several names are linked as one.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The MARC relator code for an author.
pub const AUTHOR: &str = "aut";

/// One credited name and its MARC relator roles (`aut`, `ill`, `edt`, `trl`, …).
/// A credit stating no role counts as an author and makes no claim on a file's
/// roles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    pub name: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Credit {
    /// A credit stating no role.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            roles: Vec::new(),
        }
    }

    /// A credit stating `roles`.
    pub fn with_roles(name: impl Into<String>, roles: &[&str]) -> Self {
        Self {
            name: name.into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
        }
    }

    /// Whether it counts as an author: `aut` among its roles, or none stated.
    pub fn is_author(&self) -> bool {
        self.roles.is_empty() || self.roles.iter().any(|r| r.eq_ignore_ascii_case(AUTHOR))
    }
}

/// Generational / honorific suffixes that follow a comma in `"Name, Suffix"`
/// (e.g. `"Davis, Jr."`) — that's NOT a `Surname, Given` to flip. Compared
/// case-insensitively with any trailing `.` stripped.
const NAME_SUFFIXES: &[&str] = &[
    "jr", "sr", "ii", "iii", "iv", "v", "phd", "md", "do", "esq", "dds", "jd",
];

/// Whether `name` holds a CJK, kana or Hangul character.
fn is_cjk_name(name: &str) -> bool {
    name.chars().any(bokai::text::is_cjk)
}

/// Whether `name` holds a Han ideograph or a Hangul syllable.
fn is_han_or_hangul_name(name: &str) -> bool {
    name.chars().any(|c| {
        matches!(c as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F |
            0xAC00..=0xD7AF
        )
    })
}

/// The names `packed` holds: split on `、`, and on an ASCII or full-width comma when
/// every part holds Han or Hangul (`春野一郎, 秋山次郎` splits, `スミス，ジョン` stays one).
fn unpack(packed: &str) -> Vec<&str> {
    packed
        .split('、')
        .flat_map(|part| {
            let names: Vec<&str> = part
                .split([',', '，'])
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .collect();
            if names.iter().all(|n| is_han_or_hangul_name(n)) {
                names
            } else {
                vec![part]
            }
        })
        .collect()
}

/// Flip a Western `"Surname, Given"` display name to natural `"Given Surname"`.
/// Left unchanged when the name has anything other than exactly one ASCII comma,
/// contains a CJK character, or the post-comma part is a generational suffix.
pub fn normalize_display(name: &str) -> String {
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.matches(',').count() != 1 || is_cjk_name(&collapsed) {
        return collapsed;
    }
    let (surname, given) = collapsed.split_once(',').unwrap();
    let (surname, given) = (surname.trim(), given.trim());
    if surname.is_empty() || given.is_empty() {
        return collapsed;
    }
    let tail = given.trim_end_matches('.').to_ascii_lowercase();
    if NAME_SUFFIXES.contains(&tail.as_str()) {
        return collapsed;
    }
    format!("{given} {surname}")
}

/// Trim, flip Western names, drop empties — the shared tail of every entry path.
fn canonicalize<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<String> {
    parts
        .map(normalize_display)
        .filter(|s| !s.is_empty())
        .collect()
}

/// The credits of bokai's parsed metadata, one per creator in order: Western
/// names flipped, and a creator packing several names (see [`unpack`]) unpacked
/// into one credit each, every one keeping the creator's roles.
pub fn from_metadata(meta: &bokai::Metadata) -> Vec<Credit> {
    meta.creators
        .iter()
        .flat_map(|c| {
            canonicalize(unpack(&c.name).into_iter())
                .into_iter()
                .map(|name| Credit {
                    name,
                    roles: c.roles.clone(),
                })
        })
        .collect()
}

/// The credits a stored author line names, stating no roles.
pub fn from_author_line(author: &str) -> Vec<Credit> {
    split_display(author)
        .into_iter()
        .map(Credit::named)
        .collect()
}

/// Canonical author list from a typed author line: names split on `&` or `「、」`,
/// and on a comma between Han or Hangul names (see [`unpack`]); a comma in a Latin
/// name is the `Surname, Given` separator.
pub fn parse_input(s: &str) -> Vec<String> {
    canonicalize(s.split('&').flat_map(unpack))
}

/// `credits` with every credit whose name packs several (see [`unpack`]) unpacked
/// into one credit per name in its place, each keeping its roles; every other
/// credit as it is.
pub fn unpacked(credits: &[Credit]) -> Vec<Credit> {
    credits
        .iter()
        .flat_map(|c| {
            let names = unpack(&c.name);
            if names.len() < 2 {
                return vec![c.clone()];
            }
            canonicalize(names.into_iter())
                .into_iter()
                .map(|name| Credit {
                    name,
                    roles: c.roles.clone(),
                })
                .collect()
        })
        .collect()
}

/// Split a stored author line back into its names, on `[&、]` only and with no flip.
pub fn split_display(author: &str) -> Vec<String> {
    author
        .split(['&', '、'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Join a canonical author list into the stored display string, as
/// [`bokai::model::join_names`] joins a one-string author field.
pub fn join_display(authors: &[String]) -> String {
    bokai::model::join_names(authors)
}

/// Every credited name, in order: what a file stating no roles carries.
pub fn names(credits: &[Credit]) -> Vec<String> {
    credits.iter().map(|c| c.name.clone()).collect()
}

/// The names of the credits that count as authors, in order.
pub fn author_names(credits: &[Credit]) -> Vec<String> {
    credits
        .iter()
        .filter(|c| c.is_author())
        .map(|c| c.name.clone())
        .collect()
}

/// The author line: the authors' names joined for display.
pub fn author_line(credits: &[Credit]) -> String {
    join_display(&author_names(credits))
}

/// `credits` with its authors replaced by `authors`, the other credits after them in
/// order; a name credited as an author keeps its roles, one credited only in another
/// role moves up and gains `aut`.
pub fn with_authors(credits: &[Credit], authors: &[String]) -> Vec<Credit> {
    let mut others: Vec<Credit> = credits.iter().filter(|c| !c.is_author()).cloned().collect();
    let mut out: Vec<Credit> = authors
        .iter()
        .map(|name| {
            if let Some(kept) = credits.iter().find(|c| c.is_author() && c.name == *name) {
                return kept.clone();
            }
            if let Some(at) = others.iter().position(|c| c.name == *name) {
                let mut moved = others.remove(at);
                moved.roles.push(AUTHOR.to_string());
                return moved;
            }
            Credit::named(name.clone())
        })
        .collect();
    out.extend(others);
    out
}

/// `credits` with each of `assigned` given its roles: a credited name takes them in
/// place, a new one is appended.
pub fn assign(credits: &[Credit], assigned: &[Credit]) -> Vec<Credit> {
    let mut out = credits.to_vec();
    for a in assigned {
        match out.iter_mut().find(|c| c.name == a.name) {
            Some(c) => c.roles = a.roles.clone(),
            None => out.push(a.clone()),
        }
    }
    out
}

/// Names linked as one person, each keying the person's names in order; the first
/// is the name the person sorts under. A name in none stands for a person of its own.
#[derive(Debug, Clone, Default)]
pub struct People {
    by_name: HashMap<String, Arc<[String]>>,
}

impl People {
    /// The registry of `groups`, each one person's names in order.
    pub fn new(groups: Vec<Vec<String>>) -> Self {
        let mut by_name = HashMap::new();
        for group in groups {
            let names: Arc<[String]> = group.into();
            for name in names.iter() {
                by_name.insert(name.clone(), names.clone());
            }
        }
        Self { by_name }
    }

    /// The names of the person credited as `name`, in order: `name` alone when it
    /// is linked to none.
    pub fn names_of(&self, name: &str) -> Vec<String> {
        match self.by_name.get(name) {
            Some(names) => names.to_vec(),
            None => vec![name.to_string()],
        }
    }
}

/// Each person `credits` names as an author, as that person's names, in credit
/// order and once each: the people a book is by.
pub fn author_people(credits: &[Credit], people: &People) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for c in credits.iter().filter(|c| c.is_author()) {
        let names = people.names_of(&c.name);
        if !out.contains(&names) {
            out.push(names);
        }
    }
    out
}

/// Every name of every person `credits` names, in credit order and once each.
pub fn every_name(credits: &[Credit], people: &People) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in credits {
        for name in people.names_of(&c.name) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// A user-supplied credit list made canonical: names trimmed and flipped, roles
/// trimmed, lowercased and deduplicated, credits without a name dropped, and a
/// name listed twice merged into its first place with the roles of both.
pub fn normalize(credits: Vec<Credit>) -> Vec<Credit> {
    let mut out: Vec<Credit> = Vec::with_capacity(credits.len());
    for c in credits {
        let name = normalize_display(&c.name);
        if name.is_empty() {
            continue;
        }
        let roles: Vec<String> = c
            .roles
            .iter()
            .map(|r| r.trim().to_ascii_lowercase())
            .filter(|r| !r.is_empty())
            .collect();
        let at = match out.iter().position(|o| o.name == name) {
            Some(at) => at,
            None => {
                out.push(Credit::named(name));
                out.len() - 1
            }
        };
        for r in roles {
            if !out[at].roles.contains(&r) {
                out[at].roles.push(r);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_western_surname_first() {
        assert_eq!(normalize_display("Writer, Ann"), "Ann Writer");
        assert_eq!(normalize_display("Johnson, Allan"), "Allan Johnson");
        // Multi-word given / initials and multi-word surnames.
        assert_eq!(normalize_display("Quill, J.R.R."), "J.R.R. Quill");
        assert_eq!(normalize_display("Le Pen, Ursula K."), "Ursula K. Le Pen");
    }

    #[test]
    fn leaves_natural_order_untouched() {
        assert_eq!(normalize_display("Ann Writer"), "Ann Writer");
        assert_eq!(normalize_display("J. R. R. Quill"), "J. R. R. Quill");
    }

    #[test]
    fn never_flips_cjk_or_suffixes() {
        // A CJK name is never flipped.
        assert_eq!(normalize_display("春野一郎"), "春野一郎");
        // A surname-first romaji name is Latin, and flips.
        assert_eq!(normalize_display("Haruno, Ichiro"), "Ichiro Haruno");
        // Generational suffix is "Name, Suffix", not "Surname, Given".
        assert_eq!(normalize_display("Davis, Jr."), "Davis, Jr.");
        assert_eq!(normalize_display("King, III"), "King, III");
        // Two commas: ambiguous, leave alone.
        assert_eq!(normalize_display("Davis, Sammy, Jr."), "Davis, Sammy, Jr.");
    }

    fn meta(creators: Vec<bokai::Contributor>) -> bokai::Metadata {
        bokai::Metadata {
            creators,
            ..Default::default()
        }
    }

    #[test]
    fn from_metadata_flips_unpacks_and_keeps_roles() {
        // KFX lone surname-first author.
        assert_eq!(
            from_metadata(&meta(vec![bokai::Contributor::named("Writer, Ann")])),
            [Credit::named("Ann Writer")]
        );
        // EPUB: one entry per <dc:creator>, each with the roles it states.
        assert_eq!(
            from_metadata(&meta(vec![
                bokai::Contributor::named("Doe, John").with_role("aut"),
                bokai::Contributor::named("Roe, Jane").with_role("ill"),
            ])),
            [
                Credit::with_roles("John Doe", &["aut"]),
                Credit::with_roles("Jane Roe", &["ill"])
            ]
        );
        // CJK packed in one creator → unpacked on 「、」, not flipped.
        assert_eq!(
            from_metadata(&meta(vec![
                bokai::Contributor::named("春野一郎、秋山次郎").with_role("edt")
            ])),
            [
                Credit::with_roles("春野一郎", &["edt"]),
                Credit::with_roles("秋山次郎", &["edt"])
            ]
        );
        // A comma between Han names separates them, ASCII or full-width.
        for packed in ["春野一郎, 秋山次郎", "春野一郎，秋山次郎"] {
            assert_eq!(
                from_metadata(&meta(vec![bokai::Contributor::named(packed)])),
                [Credit::named("春野一郎"), Credit::named("秋山次郎")],
                "{packed}"
            );
        }
        // A comma beside a Latin name is not a list, nor between the halves of a
        // Western name written in katakana.
        for one in ["Ann Writer, 春野一郎", "スミス，ジョン", "スミス, ジョン"] {
            assert_eq!(
                from_metadata(&meta(vec![bokai::Contributor::named(one)])),
                [Credit::named(one)],
                "{one}"
            );
        }
    }

    #[test]
    fn a_stored_packed_credit_unpacks_in_place() {
        let stored = [
            Credit::named("Writer, Ann"),
            Credit::with_roles("春野一郎, 秋山次郎", &["edt"]),
            Credit::with_roles("Ivy Drawer", &["ill"]),
        ];
        assert_eq!(
            unpacked(&stored),
            [
                Credit::named("Writer, Ann"),
                Credit::with_roles("春野一郎", &["edt"]),
                Credit::with_roles("秋山次郎", &["edt"]),
                Credit::with_roles("Ivy Drawer", &["ill"]),
            ],
            "only the packed credit changes"
        );
        let canonical = [
            Credit::named("春野一郎"),
            Credit::named("春野一郎"),
            Credit::named("スミス，ジョン"),
        ];
        assert_eq!(unpacked(&canonical), canonical);
    }

    #[test]
    fn parse_input_uses_ampersand_not_comma() {
        // A lone comma is intra-name → one (flipped) author, never two.
        assert_eq!(parse_input("Quill, J.R.R."), vec!["J.R.R. Quill"]);
        // Ampersand and ideographic comma separate authors.
        assert_eq!(
            parse_input("Ann Writer & Jane Doe"),
            vec!["Ann Writer", "Jane Doe"]
        );
        assert_eq!(
            parse_input("春野一郎、秋山次郎"),
            vec!["春野一郎", "秋山次郎"]
        );
        assert_eq!(
            parse_input("春野一郎, 秋山次郎 & Ann Writer"),
            vec!["春野一郎", "秋山次郎", "Ann Writer"]
        );
        assert!(parse_input("   ").is_empty());
    }

    #[test]
    fn join_display_picks_separator_by_script() {
        assert_eq!(join_display(&["Ann Writer".into()]), "Ann Writer");
        assert_eq!(
            join_display(&["Ann Writer".into(), "Jane Doe".into()]),
            "Ann Writer & Jane Doe"
        );
        assert_eq!(
            join_display(&["春野一郎".into(), "秋山次郎".into()]),
            "春野一郎、秋山次郎"
        );
        // Round-trips through split_display.
        let stored = join_display(&["John Doe".into(), "Jane Roe".into()]);
        assert_eq!(split_display(&stored), vec!["John Doe", "Jane Roe"]);
    }

    #[test]
    fn the_author_line_names_the_authors_alone() {
        let credits = [
            Credit::with_roles("山田 太郎", &["aut"]),
            Credit::with_roles("絵師 花子", &["ill"]),
            Credit::named("共著 次郎"),
            Credit::with_roles("Ann Writer", &["aut", "dto"]),
        ];
        assert_eq!(author_line(&credits), "山田 太郎 & 共著 次郎 & Ann Writer");
        assert_eq!(
            author_line(&[Credit::with_roles("編者 一", &["edt"])]),
            "",
            "a book may credit no author at all"
        );
        assert_eq!(names(&credits).len(), 4, "a file without roles names all");
    }

    #[test]
    fn replacing_the_authors_keeps_every_other_credit() {
        let credits = vec![
            Credit::with_roles("Ann Writer", &["aut", "dto"]),
            Credit::with_roles("Ivy Drawer", &["ill"]),
            Credit::named("Old Coauthor"),
        ];
        let out = with_authors(&credits, &["Ann Writer".into(), "New Coauthor".into()]);
        assert_eq!(
            out,
            [
                Credit::with_roles("Ann Writer", &["aut", "dto"]),
                Credit::named("New Coauthor"),
                Credit::with_roles("Ivy Drawer", &["ill"]),
            ]
        );
        let promoted = with_authors(&credits, &["Ivy Drawer".into()]);
        assert_eq!(
            promoted,
            [Credit::with_roles("Ivy Drawer", &["ill", "aut"])],
            "a name typed as an author is not credited twice"
        );
    }

    #[test]
    fn an_assignment_sets_roles_in_place_or_appends() {
        let credits = vec![Credit::named("Ann Writer"), Credit::named("Ivy Drawer")];
        let out = assign(
            &credits,
            &[
                Credit::with_roles("Ivy Drawer", &["ill"]),
                Credit::with_roles("Ed Itor", &["edt"]),
            ],
        );
        assert_eq!(
            out,
            [
                Credit::named("Ann Writer"),
                Credit::with_roles("Ivy Drawer", &["ill"]),
                Credit::with_roles("Ed Itor", &["edt"]),
            ]
        );
    }

    #[test]
    fn a_user_list_is_canonicalized_and_merged_by_name() {
        let out = normalize(vec![
            Credit::with_roles(" Writer, Ann ", &["AUT"]),
            Credit::with_roles("  ", &["ill"]),
            Credit::with_roles("Ivy Drawer", &[" ill ", ""]),
            Credit::with_roles("Ann Writer", &["dto", "aut"]),
        ]);
        assert_eq!(
            out,
            [
                Credit::with_roles("Ann Writer", &["aut", "dto"]),
                Credit::with_roles("Ivy Drawer", &["ill"]),
            ]
        );
    }

    #[test]
    fn a_linked_name_stands_for_its_whole_person() {
        let people = People::new(vec![vec!["Ann Writer".into(), "アン・ライター".into()]]);
        let translated = [
            Credit::with_roles("アン・ライター", &["aut"]),
            Credit::with_roles("春野一郎", &["trl"]),
        ];
        assert_eq!(
            author_people(&translated, &people),
            [vec!["Ann Writer".to_string(), "アン・ライター".to_string()]]
        );
        assert_eq!(
            every_name(&translated, &people),
            ["Ann Writer", "アン・ライター", "春野一郎"]
        );
        let bilingual = [Credit::named("Ann Writer"), Credit::named("アン・ライター")];
        assert_eq!(
            author_people(&bilingual, &people).len(),
            1,
            "one person, however many of its names a book credits"
        );
        assert_eq!(
            author_people(&[Credit::named("Bo Cowriter")], &People::default()),
            [vec!["Bo Cowriter".to_string()]]
        );
    }

    #[test]
    fn a_stored_author_line_reads_back_as_credits_stating_no_role() {
        assert_eq!(
            from_author_line("春野一郎、冬木歩"),
            [Credit::named("春野一郎"), Credit::named("冬木歩")]
        );
        assert!(from_author_line("").is_empty());
    }
}
