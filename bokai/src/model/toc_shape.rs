//! Format-agnostic rules for the *shape* of a table of contents: how a flat run
//! of entries is re-parented into the tree its labels imply, and how two lists of
//! entries for the same book are merged without losing either.
//!
//! Both rules read only what every format's TOC entry has — a label, a target and
//! children — so a KFX `nav_container` and an EPUB nav doc get identical
//! treatment. Each format implements [`TocNode`] for its own entry type and keeps
//! its own target vocabulary (an element id, an href, a page number); nothing
//! here knows which is which.

use std::collections::HashSet;

/// One entry of a TOC tree, whatever the format's entry type is.
///
/// The rules in this module need a label to read structure from, a target to
/// tell two entries apart by, and children to build the tree in.
pub trait TocNode: Sized {
    /// The entry's display label.
    fn label(&self) -> &str;

    /// Replace the label. [`nest_by_label_indent`] uses this to strip the
    /// indentation once the tree carries what the indentation was saying.
    fn set_label(&mut self, label: String);

    /// The entry's own children.
    fn children(&self) -> &[Self];

    /// Replace the entry's children.
    fn set_children(&mut self, children: Vec<Self>);

    /// The entry's navigation target as a comparable key: two entries with equal
    /// keys point at the same place in the book, and [`merge_by_document_order`]
    /// keeps only one of them.
    fn target_key(&self) -> String;
}

/// True if any entry in `entries` already declares children — the signal that a
/// TOC carries its own structure and the derivation rules must leave it alone.
fn already_nested<T: TocNode>(entries: &[T]) -> bool {
    entries.iter().any(|e| !e.children().is_empty())
}

/// How many levels of leading whitespace a label carries. One IDEOGRAPHIC SPACE
/// (U+3000) per level is the common CJK convention; ASCII spaces and tabs count
/// the same.
pub fn label_indent(label: &str) -> usize {
    label.chars().take_while(|c| c.is_whitespace()).count()
}

/// How many entries must carry a deeper indent than the run's shallowest before
/// the indentation counts as evidence of structure at all. One or two stray
/// leading spaces are a typo; a whole level is not.
///
/// This is a threshold on *evidence*, not on depth — nothing here bounds how
/// many levels are derived.
pub(crate) const MIN_INDENT_EVIDENCE: usize = 3;

/// Re-parent a flat run by the levels its labels keep as leading indentation.
///
/// A publisher whose TOC lost its nesting often still ships it visibly, one
/// IDEOGRAPHIC SPACE per level: a part label flush left, its chapters indented
/// once, their sections twice. Each entry attaches under the nearest preceding
/// entry with a strictly shallower indent, so the depth is whatever the labels
/// encode; the indentation is then trimmed, because the nesting now says what it
/// said.
///
/// A run with no deeper-indented entries (or fewer than [`MIN_INDENT_EVIDENCE`])
/// comes back untouched, as does one that already declares nesting.
pub fn nest_by_label_indent<T: TocNode>(entries: Vec<T>) -> Vec<T> {
    if already_nested(&entries) {
        return entries;
    }
    let indents: Vec<usize> = entries.iter().map(|e| label_indent(e.label())).collect();
    let Some(&base) = indents.iter().min() else {
        return entries;
    };
    if indents.iter().filter(|&&i| i > base).count() < MIN_INDENT_EVIDENCE {
        return entries;
    }
    let mut tree = TocTree::with_capacity(entries.len());
    // The ancestors currently open, outermost first: `(node, its indent)`.
    let mut open: Vec<(usize, usize)> = Vec::new();
    for (mut entry, indent) in entries.into_iter().zip(indents) {
        while open.last().is_some_and(|&(_, d)| d >= indent) {
            open.pop();
        }
        entry.set_label(entry.label().trim_start().to_string());
        let node = tree.push(entry, open.last().map(|&(parent, _)| parent));
        open.push((node, indent));
    }
    tree.build()
}

/// Merge the TOC a book **declares** with one **derived** from its content, into
/// a single list in document order.
///
/// The invariant is that nothing is lost: every declared entry survives, keeping
/// its own label, and a derived entry joins it only when it targets somewhere the
/// declared TOC does not already reach. A proposal built this way can add
/// chapters and add structure but can never take away an entry the reader has
/// today — which is what makes it safe to offer as the starting point for an
/// edit.
///
/// `position` places an entry in the book (a spine index, an element's ordinal,
/// a page); entries it can't place inherit their predecessor's position, so they
/// stay with the neighbours they arrived next to. Both inputs are assumed to be
/// in document order already, and equal positions keep declared before derived.
///
/// A declared TOC that already carries nesting is returned untouched: it is the
/// publisher's own structure, and there is no non-guessing way to say where a
/// flat derived list's additions belong inside it.
pub fn merge_by_document_order<T: TocNode>(
    declared: Vec<T>,
    derived: Vec<T>,
    position: impl Fn(&T) -> Option<usize>,
) -> Vec<T> {
    if declared.is_empty() {
        return derived;
    }
    if already_nested(&declared) {
        return declared;
    }
    let known: HashSet<String> = declared.iter().map(T::target_key).collect();

    // `rank` keeps a declared entry ahead of a derived one that landed on the
    // same position, and the sort below stable, so each input's own order
    // survives wherever positions can't separate them.
    let mut placed: Vec<(usize, usize, T)> = Vec::with_capacity(declared.len() + derived.len());
    let mut at = 0usize;
    for entry in declared {
        at = position(&entry).unwrap_or(at);
        placed.push((at, 0, entry));
    }
    at = 0;
    for entry in derived {
        at = position(&entry).unwrap_or(at);
        if !known.contains(&entry.target_key()) {
            placed.push((at, 1, entry));
        }
    }
    placed.sort_by_key(|&(at, rank, _)| (at, rank));
    placed.into_iter().map(|(_, _, entry)| entry).collect()
}

/// A TOC tree under construction, built in document order by naming each entry's
/// parent. Flat while it is built (an entry's children arrive after it),
/// materialized into the nested entries at the end.
pub struct TocTree<T> {
    nodes: Vec<(T, Vec<usize>)>,
    roots: Vec<usize>,
}

impl<T: TocNode> TocTree<T> {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(n),
            roots: Vec::new(),
        }
    }

    /// Add `entry` under `parent` (or at the top level), returning its node.
    pub fn push(&mut self, entry: T, parent: Option<usize>) -> usize {
        let node = self.nodes.len();
        self.nodes.push((entry, Vec::new()));
        match parent {
            Some(parent) => self.nodes[parent].1.push(node),
            None => self.roots.push(node),
        }
        node
    }

    pub fn build(self) -> Vec<T> {
        // A node's children always come after it, so filling in reverse order
        // means every child is finished before its parent asks for it.
        let mut arena: Vec<Option<(T, Vec<usize>)>> = self.nodes.into_iter().map(Some).collect();
        let mut done: Vec<Option<T>> = Vec::with_capacity(arena.len());
        done.resize_with(arena.len(), || None);
        for i in (0..arena.len()).rev() {
            let (mut entry, children) = arena[i].take().expect("each node is built once");
            entry.set_children(
                children
                    .into_iter()
                    .map(|c| done[c].take().expect("a child is built before its parent"))
                    .collect(),
            );
            done[i] = Some(entry);
        }
        self.roots
            .into_iter()
            .map(|r| done[r].take().expect("each root is built once"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal node, so these tests exercise the rules and not a format.
    #[derive(Debug, Clone, PartialEq)]
    struct Node {
        label: String,
        target: String,
        children: Vec<Node>,
    }

    impl Node {
        fn new(label: &str, target: &str) -> Self {
            Self {
                label: label.into(),
                target: target.into(),
                children: Vec::new(),
            }
        }
    }

    impl TocNode for Node {
        fn label(&self) -> &str {
            &self.label
        }
        fn set_label(&mut self, label: String) {
            self.label = label;
        }
        fn children(&self) -> &[Self] {
            &self.children
        }
        fn set_children(&mut self, children: Vec<Self>) {
            self.children = children;
        }
        fn target_key(&self) -> String {
            self.target.clone()
        }
    }

    /// `label/target/label/target/…` for a whole tree, so a test can assert the
    /// shape in one line.
    fn shape(entries: &[Node]) -> Vec<String> {
        fn walk(entries: &[Node], depth: usize, out: &mut Vec<String>) {
            for e in entries {
                out.push(format!("{}{}", "  ".repeat(depth), e.label));
                walk(&e.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(entries, 0, &mut out);
        out
    }

    #[test]
    fn indentation_nests_and_is_then_trimmed() {
        let flat = vec![
            Node::new("Part", "a"),
            Node::new(" One", "b"),
            Node::new("  i", "c"),
            Node::new("  ii", "d"),
            Node::new(" Two", "e"),
        ];
        assert_eq!(
            shape(&nest_by_label_indent(flat)),
            ["Part", "  One", "    i", "    ii", "  Two"]
        );
    }

    #[test]
    fn indentation_nests_to_any_depth() {
        // Nothing in the rule counts levels, so an arbitrarily deep run round
        // trips. Six is past any depth a format's own writer bounds.
        const DEPTH: usize = 6;
        let flat: Vec<Node> = (0..DEPTH)
            .map(|d| Node::new(&format!("{}L{d}", " ".repeat(d)), &format!("t{d}")))
            .collect();
        let nested = nest_by_label_indent(flat);

        let mut level = &nested;
        for d in 0..DEPTH {
            assert_eq!(level.len(), 1, "one entry per level at depth {d}");
            assert_eq!(level[0].label, format!("L{d}"), "indentation trimmed");
            level = &level[0].children;
        }
        assert!(level.is_empty(), "the deepest entry has no children");
    }

    #[test]
    fn a_run_with_too_little_indent_evidence_is_left_flat() {
        // Two indented entries is a typo, not a level.
        let flat = vec![
            Node::new("One", "a"),
            Node::new(" stray", "b"),
            Node::new("Two", "c"),
            Node::new(" stray", "d"),
        ];
        let out = nest_by_label_indent(flat.clone());
        assert_eq!(out, flat, "left untouched, indentation included");
    }

    #[test]
    fn an_already_nested_run_is_left_alone() {
        let mut parent = Node::new("Part", "a");
        parent.children = vec![Node::new(" One", "b")];
        let nested = vec![parent, Node::new(" Two", "c")];
        assert_eq!(nest_by_label_indent(nested.clone()), nested);
    }

    #[test]
    fn a_merge_keeps_every_declared_entry_and_adds_what_is_missing() {
        // The declared TOC knows the front and back matter; the derived one knows
        // the chapters. Neither is a superset, and the merge loses nothing.
        let declared = vec![
            Node::new("Cover", "p0"),
            Node::new("Contents", "p1"),
            Node::new("Colophon", "p9"),
        ];
        let derived = vec![Node::new("One", "p2"), Node::new("Two", "p5")];
        let pos = |n: &Node| n.target[1..].parse::<usize>().ok();

        let merged = merge_by_document_order(declared, derived, pos);
        assert_eq!(
            shape(&merged),
            ["Cover", "Contents", "One", "Two", "Colophon"],
            "declared entries survive and derived ones land in document order"
        );
    }

    #[test]
    fn a_merge_never_duplicates_a_target_and_keeps_the_declared_label() {
        let declared = vec![Node::new("Chapter One", "p1")];
        let derived = vec![Node::new("1. One", "p1"), Node::new("2. Two", "p2")];
        let pos = |n: &Node| n.target[1..].parse::<usize>().ok();

        let merged = merge_by_document_order(declared, derived, pos);
        assert_eq!(shape(&merged), ["Chapter One", "2. Two"]);
    }

    #[test]
    fn a_declared_toc_that_repeats_a_target_keeps_both_copies() {
        // Two entries pointing at one place is the publisher's own business; only
        // the derived side is held to what the declaration already reached.
        let declared = vec![Node::new("Prologue", "p1"), Node::new("Chapter One", "p1")];
        let derived = vec![Node::new("1", "p1")];
        let pos = |n: &Node| n.target[1..].parse::<usize>().ok();

        let merged = merge_by_document_order(declared, derived, pos);
        assert_eq!(shape(&merged), ["Prologue", "Chapter One"]);
    }

    #[test]
    fn unplaceable_entries_stay_with_the_neighbour_they_arrived_next_to() {
        let declared = vec![
            Node::new("One", "p1"),
            Node::new("One's note", "?"), // no position
            Node::new("Three", "p3"),
        ];
        let derived = vec![Node::new("Two", "p2")];
        let pos = |n: &Node| n.target.strip_prefix('p')?.parse::<usize>().ok();

        let merged = merge_by_document_order(declared, derived, pos);
        assert_eq!(
            shape(&merged),
            ["One", "One's note", "Two", "Three"],
            "the unplaceable entry inherits its predecessor's position"
        );
    }

    #[test]
    fn a_nested_declared_toc_is_returned_untouched() {
        let mut parent = Node::new("Part", "p1");
        parent.children = vec![Node::new("One", "p2")];
        let declared = vec![parent];
        let derived = vec![Node::new("Two", "p3")];

        let merged = merge_by_document_order(declared, derived, |_| None);
        assert_eq!(
            shape(&merged),
            ["Part", "  One"],
            "the publisher's own structure wins; nothing is guessed into it"
        );
    }

    #[test]
    fn merging_into_an_empty_declared_toc_yields_the_derivation() {
        let merged = merge_by_document_order(Vec::new(), vec![Node::new("One", "p1")], |_| None);
        assert_eq!(shape(&merged), ["One"]);
    }
}
