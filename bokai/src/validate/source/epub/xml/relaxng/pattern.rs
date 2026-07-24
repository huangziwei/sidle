//! The RELAX NG pattern model, hash-consed into an arena.
//!
//! This is the *simplified* syntax of the specification (§4): the form a grammar
//! takes after the transformations that inline `<define>`/`<ref>` cycles, hoist
//! namespaces onto names, and reduce every sugar (`<optional>`, `<zeroOrMore>`,
//! `<mixed>`, `<element name="x">`) to the dozen primitives below. Validation is
//! then a fold over this model and nothing else — [`super::derive`].
//!
//! Patterns are interned. Two reasons, both load-bearing:
//!
//! - **Recursion.** `element foo { element foo { … }? }` is a cycle in the
//!   pattern graph, so a tree of owned values cannot represent it. An arena of
//!   ids can.
//! - **Speed.** Validation derives a new pattern per node, and the derivative of
//!   a large grammar is mostly *the same subpatterns again*. Interning makes
//!   structural equality a `u32` comparison and lets the derivative cache hit.

use std::collections::HashMap;

/// A pattern's index in the [`Arena`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PatternId(pub u32);

/// A name class's index in the [`Arena`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameClassId(pub u32);

/// Which set of element or attribute names a pattern accepts (spec §4, the
/// `nameClass` production).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NameClass {
    /// `<anyName/>`.
    AnyName,
    /// `<anyName><except>…</except></anyName>`.
    AnyNameExcept(NameClassId),
    /// One expanded name. `ns` is `None` for the no-namespace case, which is
    /// what almost every attribute in these grammars is.
    Name { ns: Option<String>, local: String },
    /// `<nsName ns="…"/>` — any local name in one namespace.
    NsName(Option<String>),
    /// `<nsName ns="…"><except>…</except></nsName>`.
    NsNameExcept(Option<String>, NameClassId),
    /// `<choice>` of two name classes.
    Choice(NameClassId, NameClassId),
}

/// A datatype reference: the library it comes from and its name. The grammars
/// here use only the built-in library (`string`/`token`) and XSD.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatatypeName {
    /// The `datatypeLibrary` URI; empty for the built-in library.
    pub library: String,
    pub name: String,
}

/// A RELAX NG pattern in simplified form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Pattern {
    /// Matches the empty sequence.
    Empty,
    /// Matches nothing at all — the value a failed derivative collapses to.
    NotAllowed,
    /// Matches any string.
    Text,
    Choice(PatternId, PatternId),
    Interleave(PatternId, PatternId),
    Group(PatternId, PatternId),
    OneOrMore(PatternId),
    /// `<list>` — the content is split on whitespace and matched as a sequence.
    List(PatternId),
    /// `<data type="…">` with its parameters.
    Data {
        datatype: DatatypeName,
        params: Vec<(String, String)>,
    },
    /// `<data>` with an `<except>` pattern.
    DataExcept {
        datatype: DatatypeName,
        params: Vec<(String, String)>,
        except: PatternId,
    },
    /// `<value>` — one literal, compared in the datatype's value space.
    Value {
        datatype: DatatypeName,
        value: String,
    },
    Attribute(NameClassId, PatternId),
    Element(NameClassId, PatternId),
    /// Not part of the input syntax: produced during derivation to mean "match
    /// the first pattern, then continue with the second" (spec's `after`).
    After(PatternId, PatternId),
    /// An indirection to another pattern, which [`Arena::pattern`] follows
    /// transparently.
    ///
    /// A `define` whose body is just `<ref name="other"/>` — the aliasing that
    /// runs through every modular grammar — cannot be filled by *copying*
    /// `other`'s pattern, because `other` may not be filled yet. Pointing at it
    /// is order-independent and cycle-safe.
    Ref(PatternId),
    /// A [`Arena::reserve`]d slot that has not been [`Arena::fill`]ed yet.
    ///
    /// It has to be its own variant rather than a stand-in like `NotAllowed` or
    /// `Empty`: the smart constructors below rewrite those away, so a recursive
    /// definition built around one would silently lose its own recursion — the
    /// pattern `(div | text)*` would collapse to `text*` while `div` was still a
    /// placeholder. `Hole` is inert, so it survives until it is filled.
    Hole,
}

/// The interning store for one grammar's patterns and name classes.
#[derive(Debug, Default)]
pub struct Arena {
    patterns: Vec<Pattern>,
    pattern_ids: HashMap<Pattern, PatternId>,
    names: Vec<NameClass>,
    name_ids: HashMap<NameClass, NameClassId>,
}

impl Arena {
    pub fn new() -> Self {
        Arena::default()
    }

    /// The pattern behind an id, following [`Pattern::Ref`] indirections. The
    /// hop count is bounded so a grammar defining `x` as `ref x` fails as an
    /// unmatchable pattern instead of hanging.
    pub fn pattern(&self, id: PatternId) -> &Pattern {
        let mut cur = id;
        for _ in 0..64 {
            match &self.patterns[cur.0 as usize] {
                Pattern::Ref(next) => cur = *next,
                p => return p,
            }
        }
        &Pattern::NotAllowed
    }

    pub fn name_class(&self, id: NameClassId) -> &NameClass {
        &self.names[id.0 as usize]
    }

    /// Intern a pattern, returning the existing id when it is already present.
    pub fn intern(&mut self, p: Pattern) -> PatternId {
        if let Some(id) = self.pattern_ids.get(&p) {
            return *id;
        }
        let id = PatternId(self.patterns.len() as u32);
        self.patterns.push(p.clone());
        self.pattern_ids.insert(p, id);
        id
    }

    pub fn intern_name(&mut self, n: NameClass) -> NameClassId {
        if let Some(id) = self.name_ids.get(&n) {
            return *id;
        }
        let id = NameClassId(self.names.len() as u32);
        self.names.push(n.clone());
        self.name_ids.insert(n, id);
        id
    }

    /// Reserve an id whose pattern is filled in later — the only way to build
    /// the cycle a recursive `<define>` becomes after inlining. The slot holds
    /// [`Pattern::Hole`] until [`fill`](Self::fill), and is deliberately not
    /// interned so two reservations never alias.
    pub fn reserve(&mut self) -> PatternId {
        let id = PatternId(self.patterns.len() as u32);
        self.patterns.push(Pattern::Hole);
        id
    }

    /// Fill a [`reserve`](Self::reserve)d id. Deliberately *not* interned: a
    /// placeholder that later equals an existing pattern must keep its own id,
    /// or the cycle it stands for would collapse.
    pub fn fill(&mut self, id: PatternId, p: Pattern) {
        self.patterns[id.0 as usize] = p;
    }

    pub fn empty(&mut self) -> PatternId {
        self.intern(Pattern::Empty)
    }

    pub fn not_allowed(&mut self) -> PatternId {
        self.intern(Pattern::NotAllowed)
    }

    pub fn text(&mut self) -> PatternId {
        self.intern(Pattern::Text)
    }

    pub fn is(&self, id: PatternId, p: &Pattern) -> bool {
        self.pattern(id) == p
    }

    // ---- the specification's smart constructors (§ "Simplification of
    // patterns"). Each keeps the algebraic identity that makes the derivative
    // collapse instead of growing without bound.

    pub fn choice(&mut self, a: PatternId, b: PatternId) -> PatternId {
        if self.is(a, &Pattern::NotAllowed) {
            return b;
        }
        if self.is(b, &Pattern::NotAllowed) {
            return a;
        }
        if a == b {
            return a;
        }
        self.intern(Pattern::Choice(a, b))
    }

    pub fn group(&mut self, a: PatternId, b: PatternId) -> PatternId {
        if self.is(a, &Pattern::NotAllowed) || self.is(b, &Pattern::NotAllowed) {
            return self.not_allowed();
        }
        if self.is(a, &Pattern::Empty) {
            return b;
        }
        if self.is(b, &Pattern::Empty) {
            return a;
        }
        self.intern(Pattern::Group(a, b))
    }

    pub fn interleave(&mut self, a: PatternId, b: PatternId) -> PatternId {
        if self.is(a, &Pattern::NotAllowed) || self.is(b, &Pattern::NotAllowed) {
            return self.not_allowed();
        }
        if self.is(a, &Pattern::Empty) {
            return b;
        }
        if self.is(b, &Pattern::Empty) {
            return a;
        }
        self.intern(Pattern::Interleave(a, b))
    }

    pub fn one_or_more(&mut self, a: PatternId) -> PatternId {
        if self.is(a, &Pattern::NotAllowed) {
            return self.not_allowed();
        }
        self.intern(Pattern::OneOrMore(a))
    }

    pub fn after(&mut self, a: PatternId, b: PatternId) -> PatternId {
        if self.is(a, &Pattern::NotAllowed) || self.is(b, &Pattern::NotAllowed) {
            return self.not_allowed();
        }
        self.intern(Pattern::After(a, b))
    }

    /// `optional p` = `choice p empty`.
    pub fn optional(&mut self, a: PatternId) -> PatternId {
        let empty = self.empty();
        self.choice(a, empty)
    }

    /// `zeroOrMore p` = `choice (oneOrMore p) empty`.
    pub fn zero_or_more(&mut self, a: PatternId) -> PatternId {
        let one = self.one_or_more(a);
        self.optional(one)
    }

    /// Whether the pattern matches the empty sequence (spec's `nullable`).
    pub fn nullable(&self, id: PatternId) -> bool {
        match self.pattern(id) {
            Pattern::Empty | Pattern::Text => true,
            // A hole is a grammar still under construction, and `Ref` is
            // already followed by `pattern`; treating either as non-nullable is
            // the conservative reading.
            Pattern::Ref(_)
            | Pattern::Hole
            | Pattern::NotAllowed
            | Pattern::Element(..)
            | Pattern::Attribute(..)
            | Pattern::Data { .. }
            | Pattern::DataExcept { .. }
            | Pattern::Value { .. }
            | Pattern::List(_) => false,
            Pattern::Choice(a, b) => self.nullable(*a) || self.nullable(*b),
            Pattern::Group(a, b) | Pattern::Interleave(a, b) => {
                self.nullable(*a) && self.nullable(*b)
            }
            Pattern::OneOrMore(a) => self.nullable(*a),
            // `after p q` has consumed nothing yet, so it is never nullable.
            Pattern::After(..) => false,
        }
    }

    /// Does this name class accept `(ns, local)`?
    pub fn name_matches(&self, id: NameClassId, ns: Option<&str>, local: &str) -> bool {
        match self.name_class(id) {
            NameClass::AnyName => true,
            NameClass::AnyNameExcept(e) => !self.name_matches(*e, ns, local),
            NameClass::Name { ns: n, local: l } => n.as_deref() == ns && l == local,
            NameClass::NsName(n) => n.as_deref() == ns,
            NameClass::NsNameExcept(n, e) => {
                n.as_deref() == ns && !self.name_matches(*e, ns, local)
            }
            NameClass::Choice(a, b) => {
                self.name_matches(*a, ns, local) || self.name_matches(*b, ns, local)
            }
        }
    }

    /// Every concrete name a class names, for a diagnostic that can say what was
    /// expected. Wildcards contribute nothing, so the list is a *subset* — a
    /// message never claims a name is disallowed when it is not.
    pub fn expected_names(&self, id: NameClassId, out: &mut Vec<String>) {
        match self.name_class(id) {
            NameClass::Name { local, .. } => out.push(local.clone()),
            NameClass::Choice(a, b) => {
                self.expected_names(*a, out);
                self.expected_names(*b, out);
            }
            NameClass::AnyName
            | NameClass::AnyNameExcept(_)
            | NameClass::NsName(_)
            | NameClass::NsNameExcept(..) => {}
        }
    }

    /// How many patterns are interned — the grammar's compiled size.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_makes_equal_patterns_identical() {
        let mut a = Arena::new();
        let t1 = a.text();
        let t2 = a.text();
        assert_eq!(t1, t2);
        let g1 = a.group(t1, t1);
        let g2 = a.group(t2, t2);
        assert_eq!(g1, g2);
    }

    #[test]
    fn smart_constructors_keep_the_algebraic_identities() {
        let mut a = Arena::new();
        let (empty, na, text) = (a.empty(), a.not_allowed(), a.text());
        // NotAllowed absorbs in group/interleave and vanishes in choice.
        assert_eq!(a.group(na, text), na);
        assert_eq!(a.interleave(text, na), na);
        assert_eq!(a.choice(na, text), text);
        assert_eq!(a.choice(text, na), text);
        // Empty is the unit of group/interleave.
        assert_eq!(a.group(empty, text), text);
        assert_eq!(a.interleave(text, empty), text);
        // Idempotent choice, so a derivative cannot grow by re-adding a branch.
        assert_eq!(a.choice(text, text), text);
        assert_eq!(a.one_or_more(na), na);
    }

    #[test]
    fn nullability_follows_the_specification() {
        let mut a = Arena::new();
        let (empty, na, text) = (a.empty(), a.not_allowed(), a.text());
        assert!(a.nullable(empty) && a.nullable(text) && !a.nullable(na));

        let name = a.intern_name(NameClass::Name {
            ns: None,
            local: "x".into(),
        });
        let elem = a.intern(Pattern::Element(name, empty));
        assert!(!a.nullable(elem), "an element still has to be matched");
        let maybe = a.optional(elem);
        assert!(a.nullable(maybe), "optional is nullable");

        let group = a.group(elem, text);
        assert!(!a.nullable(group), "group needs both sides nullable");
        let choice = a.choice(elem, text);
        assert!(a.nullable(choice), "choice needs either side nullable");
        let plus = a.one_or_more(elem);
        assert!(!a.nullable(plus));
        let star = a.zero_or_more(elem);
        assert!(a.nullable(star));
    }

    #[test]
    fn name_classes_match_by_namespace_and_local_name() {
        let mut a = Arena::new();
        let html = a.intern_name(NameClass::Name {
            ns: Some("http://www.w3.org/1999/xhtml".into()),
            local: "p".into(),
        });
        assert!(a.name_matches(html, Some("http://www.w3.org/1999/xhtml"), "p"));
        assert!(!a.name_matches(html, None, "p"), "no namespace is distinct");
        assert!(!a.name_matches(html, Some("http://www.w3.org/1999/xhtml"), "div"));

        let any_svg = a.intern_name(NameClass::NsName(Some("urn:svg".into())));
        assert!(a.name_matches(any_svg, Some("urn:svg"), "anything"));
        assert!(!a.name_matches(any_svg, Some("urn:other"), "anything"));

        let except = a.intern_name(NameClass::AnyNameExcept(html));
        assert!(a.name_matches(except, None, "p"));
        assert!(!a.name_matches(except, Some("http://www.w3.org/1999/xhtml"), "p"));

        let either = a.intern_name(NameClass::Choice(html, any_svg));
        assert!(a.name_matches(either, Some("urn:svg"), "rect"));
        let mut names = Vec::new();
        a.expected_names(either, &mut names);
        assert_eq!(names, ["p"], "a wildcard contributes no expected name");
    }
}
