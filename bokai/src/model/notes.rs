//! `NoteRole` for a source that states no `epub:type`, from reciprocal links:
//! a marker links into a note body and that body links back into the marker's
//! block.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::model::{AnchorTarget, Chapter, ChapterId, GlobalNodeId, NodeId, ResolvedLinks, Role};

/// A node's part in a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteRole {
    /// The link that opens the note.
    Reference,
    /// The block holding the note text.
    Body,
}

impl NoteRole {
    /// The `epub:type` token for this role.
    pub fn epub_type(self) -> &'static str {
        match self {
            NoteRole::Reference => "noteref",
            NoteRole::Body => "footnote",
        }
    }
}

/// Spine index, then document-order rank within that chapter.
type ReadingOrder = (usize, usize);

/// True for a `role` that carries no block of its own.
fn is_inline(role: Role) -> bool {
    matches!(
        role,
        Role::Text
            | Role::Inline
            | Role::Link
            | Role::Image
            | Role::Break
            | Role::Ruby
            | Role::RubyText
    )
}

/// The nearest block enclosing `node`, or `node` when its own role is a block.
/// `None` when the walk reaches `NodeId::ROOT`.
fn enclosing_block(chapter: &Chapter, node: NodeId) -> Option<NodeId> {
    let mut current = node;
    loop {
        let n = chapter.node(current)?;
        if !is_inline(n.role) {
            return (current != NodeId::ROOT).then_some(current);
        }
        current = n.parent?;
    }
}

/// One internal link, reduced to the two blocks it joins.
struct BlockLink {
    /// The link node itself.
    link: GlobalNodeId,
    /// The block the link sits in.
    from: GlobalNodeId,
    /// The block the link lands in.
    to: GlobalNodeId,
}

/// `to` is a `Body` when `from` precedes it, `to`'s subtree links back to
/// `from`, and `to` is outside `nav_targets` and not a `Role::Heading`. Every
/// `link` landing in a `Body` from an earlier block is a `Reference`.
pub(crate) fn detect_notes(
    chapters: &[(ChapterId, Arc<Chapter>)],
    resolved: &ResolvedLinks,
    nav_targets: &HashSet<GlobalNodeId>,
) -> HashMap<GlobalNodeId, NoteRole> {
    let by_id: HashMap<ChapterId, &Arc<Chapter>> =
        chapters.iter().map(|(id, c)| (*id, c)).collect();

    // One `BlockLink` per internal link in `resolved`.
    let mut links: Vec<BlockLink> = Vec::new();
    for (source, target) in resolved.iter() {
        let AnchorTarget::Internal(target) = target else {
            continue;
        };
        let (Some(from_chapter), Some(to_chapter)) =
            (by_id.get(&source.chapter), by_id.get(&target.chapter))
        else {
            continue;
        };
        let (Some(from), Some(to)) = (
            enclosing_block(from_chapter, source.node),
            enclosing_block(to_chapter, target.node),
        ) else {
            continue;
        };
        links.push(BlockLink {
            link: source,
            from: GlobalNodeId::new(source.chapter, from),
            to: GlobalNodeId::new(target.chapter, to),
        });
    }
    if links.is_empty() {
        return HashMap::new();
    }

    let order = reading_order(chapters, &links);

    // `reach[b]`: the blocks that links anywhere in `b`'s subtree land in.
    let mut reach: HashMap<GlobalNodeId, HashSet<GlobalNodeId>> = HashMap::new();
    for link in &links {
        let Some(chapter) = by_id.get(&link.link.chapter) else {
            continue;
        };
        let mut block = Some(link.from.node);
        while let Some(id) = block {
            reach
                .entry(GlobalNodeId::new(link.link.chapter, id))
                .or_default()
                .insert(link.to);
            block = chapter
                .node(id)
                .and_then(|n| n.parent)
                .and_then(|p| enclosing_block(chapter, p));
        }
    }

    let is_heading = |block: &GlobalNodeId| {
        by_id
            .get(&block.chapter)
            .and_then(|c| c.node(block.node))
            .is_some_and(|n| matches!(n.role, Role::Heading(_)))
    };

    // `bodies`: the later end of each reciprocal pair.
    let bodies: HashSet<GlobalNodeId> = links
        .iter()
        .filter(|link| precedes(&order, &link.from, &link.to))
        .filter(|link| {
            reach
                .get(&link.to)
                .is_some_and(|targets| targets.contains(&link.from))
        })
        .map(|link| link.to)
        .filter(|body| !nav_targets.contains(body) && !is_heading(body))
        .collect();

    let mut roles: HashMap<GlobalNodeId, NoteRole> =
        bodies.iter().map(|body| (*body, NoteRole::Body)).collect();
    for link in &links {
        if bodies.contains(&link.to) && precedes(&order, &link.from, &link.to) {
            roles.insert(link.link, NoteRole::Reference);
        }
    }
    roles
}

/// True when `first` sorts before `second` in `order`.
fn precedes(
    order: &HashMap<GlobalNodeId, ReadingOrder>,
    first: &GlobalNodeId,
    second: &GlobalNodeId,
) -> bool {
    match (order.get(first), order.get(second)) {
        (Some(first), Some(second)) => first < second,
        _ => false,
    }
}

/// `ReadingOrder` for each `from` and `to` block named in `links`.
fn reading_order(
    chapters: &[(ChapterId, Arc<Chapter>)],
    links: &[BlockLink],
) -> HashMap<GlobalNodeId, ReadingOrder> {
    let mut wanted: HashMap<ChapterId, HashSet<NodeId>> = HashMap::new();
    for link in links {
        wanted
            .entry(link.from.chapter)
            .or_default()
            .insert(link.from.node);
        wanted
            .entry(link.to.chapter)
            .or_default()
            .insert(link.to.node);
    }

    let mut order = HashMap::new();
    for (spine_index, (chapter_id, chapter)) in chapters.iter().enumerate() {
        let Some(blocks) = wanted.get(chapter_id) else {
            continue;
        };
        for (rank, node_id) in chapter.iter_dfs().enumerate() {
            if blocks.contains(&node_id) {
                order.insert(GlobalNodeId::new(*chapter_id, node_id), (spine_index, rank));
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::super::resolved::ResolvedLinksBuilder;
    use super::*;
    use crate::model::Node;

    /// One `Role::Paragraph` per entry in `link_counts`, holding that many
    /// `Role::Link` children, with the ids of each.
    fn blocks(link_counts: &[usize]) -> (Chapter, Vec<(NodeId, Vec<NodeId>)>) {
        let mut chapter = Chapter::new();
        let mut out = Vec::new();
        for &count in link_counts {
            let block = chapter.alloc_node(Node::new(Role::Paragraph));
            chapter.append_child(NodeId::ROOT, block);
            let mut links = Vec::new();
            for _ in 0..count {
                let link = chapter.alloc_node(Node::new(Role::Link));
                chapter.append_child(block, link);
                links.push(link);
            }
            out.push((block, links));
        }
        (chapter, out)
    }

    fn spine(list: Vec<(ChapterId, Chapter)>) -> Vec<(ChapterId, Arc<Chapter>)> {
        list.into_iter().map(|(id, c)| (id, Arc::new(c))).collect()
    }

    #[test]
    fn reciprocal_pair_is_a_note() {
        let (text, text_blocks) = blocks(&[1]);
        let (notes, note_blocks) = blocks(&[1]);
        let marker = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, marker);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &resolved,
            &HashSet::new(),
        );

        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
        assert_eq!(
            roles.get(&GlobalNodeId::new(ChapterId(1), note_blocks[0].0)),
            Some(&NoteRole::Body)
        );
        assert_eq!(roles.get(&back), None);
        assert_eq!(
            roles.get(&GlobalNodeId::new(ChapterId(0), text_blocks[0].0)),
            None
        );
    }

    #[test]
    fn one_way_link_is_a_cross_reference() {
        let (text, text_blocks) = blocks(&[1]);
        let (target, target_blocks) = blocks(&[0]);
        let link = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(link, GlobalNodeId::new(ChapterId(1), target_blocks[0].0));
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), target)]),
            &resolved,
            &HashSet::new(),
        );
        assert!(roles.is_empty());
    }

    /// `entry` links to `heading` and `heading` links back to `entry`.
    fn contents_and_section(
        heading_role: Role,
    ) -> (Vec<(ChapterId, Arc<Chapter>)>, ResolvedLinks, GlobalNodeId) {
        let (contents, contents_blocks) = blocks(&[1]);

        let mut chapter = Chapter::new();
        let heading = chapter.alloc_node(Node::new(heading_role));
        chapter.append_child(NodeId::ROOT, heading);
        let back_link = chapter.alloc_node(Node::new(Role::Link));
        chapter.append_child(heading, back_link);

        let entry = GlobalNodeId::new(ChapterId(0), contents_blocks[0].1[0]);
        let heading = GlobalNodeId::new(ChapterId(1), heading);
        let back_link = GlobalNodeId::new(ChapterId(1), back_link);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(entry, heading);
        builder.add_internal(back_link, entry);
        (
            spine(vec![(ChapterId(0), contents), (ChapterId(1), chapter)]),
            builder.build(),
            heading,
        )
    }

    #[test]
    fn a_section_the_navigation_names_is_no_note() {
        let (spine, resolved, heading) = contents_and_section(Role::Paragraph);
        let roles = detect_notes(&spine, &resolved, &HashSet::from([heading]));
        assert!(roles.is_empty());
    }

    #[test]
    fn a_heading_is_no_note_even_outside_the_navigation() {
        let (spine, resolved, _) = contents_and_section(Role::Heading(1));
        let roles = detect_notes(&spine, &resolved, &HashSet::new());
        assert!(roles.is_empty());
    }

    #[test]
    fn an_index_entry_citing_a_note_stays_a_jump() {
        // `entry` sits in a chapter after `body`.
        let (text, text_blocks) = blocks(&[1]);
        let (notes, note_blocks) = blocks(&[1]);
        let (index, index_blocks) = blocks(&[1]);

        let marker = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);
        let body = GlobalNodeId::new(ChapterId(1), note_blocks[0].0);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);
        let entry = GlobalNodeId::new(ChapterId(2), index_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, marker);
        builder.add_internal(entry, back);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![
                (ChapterId(0), text),
                (ChapterId(1), notes),
                (ChapterId(2), index),
            ]),
            &resolved,
            &HashSet::new(),
        );
        assert_eq!(roles.get(&body), Some(&NoteRole::Body));
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
        assert_eq!(roles.get(&entry), None);
    }

    #[test]
    fn a_note_cited_twice_marks_both_citations() {
        // `back` links to `first` alone.
        let (text, text_blocks) = blocks(&[1, 1]);
        let (notes, note_blocks) = blocks(&[1]);
        let first = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);
        let second = GlobalNodeId::new(ChapterId(0), text_blocks[1].1[0]);
        let body = GlobalNodeId::new(ChapterId(1), note_blocks[0].0);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(first, back);
        builder.add_internal(second, back);
        builder.add_internal(back, first);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &resolved,
            &HashSet::new(),
        );
        assert_eq!(roles.get(&first), Some(&NoteRole::Reference));
        assert_eq!(roles.get(&second), Some(&NoteRole::Reference));
        assert_eq!(roles.get(&body), Some(&NoteRole::Body));
    }

    #[test]
    fn a_note_of_several_paragraphs_answers_for_its_back_link() {
        // `marker` targets `body`; `back_link` sits in a paragraph under it.
        let (text, text_blocks) = blocks(&[1]);
        let marker = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);

        let mut notes = Chapter::new();
        let body = notes.alloc_node(Node::new(Role::Container));
        notes.append_child(NodeId::ROOT, body);
        let para = notes.alloc_node(Node::new(Role::Paragraph));
        notes.append_child(body, para);
        let back_link = notes.alloc_node(Node::new(Role::Link));
        notes.append_child(para, back_link);
        let second = notes.alloc_node(Node::new(Role::Paragraph));
        notes.append_child(body, second);

        let back = GlobalNodeId::new(ChapterId(1), back_link);
        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, GlobalNodeId::new(ChapterId(1), body));
        builder.add_internal(back, marker);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &resolved,
            &HashSet::new(),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
        assert_eq!(
            roles.get(&GlobalNodeId::new(ChapterId(1), body)),
            Some(&NoteRole::Body)
        );
    }

    #[test]
    fn a_link_back_to_the_top_of_its_own_section_is_no_note() {
        let mut chapter = Chapter::new();
        let section = chapter.alloc_node(Node::new(Role::Container));
        chapter.append_child(NodeId::ROOT, section);
        let para = chapter.alloc_node(Node::new(Role::Paragraph));
        chapter.append_child(section, para);
        let up_link = chapter.alloc_node(Node::new(Role::Link));
        chapter.append_child(para, up_link);

        let up = GlobalNodeId::new(ChapterId(0), up_link);
        let section = GlobalNodeId::new(ChapterId(0), section);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(up, section);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), chapter)]),
            &resolved,
            &HashSet::new(),
        );
        assert!(roles.is_empty());
    }

    #[test]
    fn footnotes_later_in_the_same_chapter_are_notes() {
        let (chapter, chapter_blocks) = blocks(&[1, 1]);
        let marker = GlobalNodeId::new(ChapterId(0), chapter_blocks[0].1[0]);
        let back = GlobalNodeId::new(ChapterId(0), chapter_blocks[1].1[0]);
        let body = GlobalNodeId::new(ChapterId(0), chapter_blocks[1].0);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, marker);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), chapter)]),
            &resolved,
            &HashSet::new(),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
        assert_eq!(roles.get(&body), Some(&NoteRole::Body));
    }
}
