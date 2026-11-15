//! `NoteRole` from reciprocal links, for a source without `epub:type`: a
//! `Reference` links into a `Body`, and the `Body` links back into the block
//! of its `Reference`.

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

/// True when `node` is all `block` holds outside its nested blocks: no other
/// text, and no image.
fn fills_block(chapter: &Chapter, block: NodeId, node: NodeId) -> bool {
    chapter.children(block).all(|child| {
        if child == node {
            return true;
        }
        let Some(n) = chapter.node(child) else {
            return true;
        };
        match n.role {
            Role::Text => chapter.text(n.text).trim().is_empty(),
            Role::Image => false,
            role if is_inline(role) => fills_block(chapter, child, node),
            _ => true,
        }
    })
}

/// True when `target` is `marker`, an inline element around it, or a textless
/// node beside either.
fn lands_on(chapter: &Chapter, target: NodeId, marker: NodeId) -> bool {
    let mut current = marker;
    loop {
        let Some(node) = chapter.node(current).filter(|n| is_inline(n.role)) else {
            return false;
        };
        if current == target || textless_beside(chapter, current, target) {
            return true;
        }
        let Some(parent) = node.parent else {
            return false;
        };
        current = parent;
    }
}

/// True when `other` is a sibling of `node` with only textless siblings
/// between them, and no text of its own.
fn textless_beside(chapter: &Chapter, node: NodeId, other: NodeId) -> bool {
    let Some(parent) = chapter.node(node).and_then(|n| n.parent) else {
        return false;
    };
    let siblings: Vec<NodeId> = chapter.children(parent).collect();
    let Some(at) = siblings.iter().position(|&s| s == node) else {
        return false;
    };
    let textless = |s: &&NodeId| leading_text(chapter, **s, 1).is_empty();
    siblings[..at]
        .iter()
        .rev()
        .take_while(textless)
        .any(|&s| s == other)
        || siblings[at + 1..]
            .iter()
            .take_while(textless)
            .any(|&s| s == other)
}

/// The first `limit` characters of `node`'s text, each whitespace run one
/// space and none leading.
fn leading_text(chapter: &Chapter, node: NodeId, limit: usize) -> String {
    fn walk(chapter: &Chapter, node: NodeId, limit: usize, out: &mut String, count: &mut usize) {
        let Some(n) = chapter.node(node) else {
            return;
        };
        if n.role == Role::Text {
            for c in chapter.text(n.text).chars() {
                if *count == limit {
                    return;
                }
                if !c.is_whitespace() {
                    out.push(c);
                } else if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                } else {
                    continue;
                }
                *count += 1;
            }
        }
        for child in chapter.children(node) {
            if *count == limit {
                return;
            }
            walk(chapter, child, limit, out, count);
        }
    }
    let mut out = String::new();
    walk(chapter, node, limit, &mut out, &mut 0);
    out
}

/// True when `block`'s text opens with `name` as a whole word.
fn opens_with(chapter: &Chapter, block: NodeId, name: &str) -> bool {
    let landing = leading_text(chapter, block, name.chars().count() + 1);
    landing
        .strip_prefix(name)
        .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()))
}

/// One internal link, reduced to the two blocks it joins.
struct BlockLink {
    /// The link node itself.
    link: GlobalNodeId,
    /// The block the link sits in.
    from: GlobalNodeId,
    /// The node the link lands on.
    target: GlobalNodeId,
    /// The block the link lands in.
    to: GlobalNodeId,
    /// `link` is all `from` holds: a label, such as a contents entry.
    label: bool,
}

/// True when `back`, landing in `link.from`, returns there as a note body's
/// link does: never one label answering another, and never landing off `link`
/// on a block whose text opens with `back`'s own as a word.
fn answers(back: &BlockLink, link: &BlockLink, by_id: &HashMap<ChapterId, &Arc<Chapter>>) -> bool {
    if back.label && link.label {
        return false;
    }
    let (Some(back_chapter), Some(link_chapter)) =
        (by_id.get(&back.link.chapter), by_id.get(&link.link.chapter))
    else {
        return false;
    };
    if lands_on(link_chapter, back.target.node, link.link.node) {
        return true;
    }
    let name = leading_text(back_chapter, back.link.node, usize::MAX);
    let name = name.trim_end();
    name.is_empty() || !opens_with(link_chapter, link.from.node, name)
}

/// Per `BlockLink`: `link` is a `Reference` and `to` its `Body` when `to` comes
/// later and a link in it `answers` `link` — unless `to` is a `Role::Heading`,
/// or a node of `nav_targets` is `to` or fills it.
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
            target: *target,
            to: GlobalNodeId::new(target.chapter, to),
            label: fills_block(from_chapter, from, source.node),
        });
    }
    if links.is_empty() {
        return HashMap::new();
    }

    let order = reading_order(chapters, &links);

    // `returns[b][t]`: the indexes of the links in `b`'s subtree landing in `t`.
    let mut returns: HashMap<GlobalNodeId, HashMap<GlobalNodeId, Vec<usize>>> = HashMap::new();
    for (index, link) in links.iter().enumerate() {
        let Some(chapter) = by_id.get(&link.link.chapter) else {
            continue;
        };
        let mut block = Some(link.from.node);
        while let Some(id) = block {
            returns
                .entry(GlobalNodeId::new(link.link.chapter, id))
                .or_default()
                .entry(link.to)
                .or_default()
                .push(index);
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

    let nav_blocks: HashSet<GlobalNodeId> = nav_targets
        .iter()
        .filter_map(|target| {
            let chapter = by_id.get(&target.chapter)?;
            let block = enclosing_block(chapter, target.node)?;
            (block == target.node || fills_block(chapter, block, target.node))
                .then(|| GlobalNodeId::new(target.chapter, block))
        })
        .collect();

    // `answered(link)`: `to` follows `from`, and a link in `to` `answers` it.
    let answered = |link: &BlockLink| {
        precedes(&order, &link.from, &link.to)
            && returns
                .get(&link.to)
                .and_then(|landing| landing.get(&link.from))
                .is_some_and(|backs| backs.iter().any(|&b| answers(&links[b], link, &by_id)))
    };

    // `bodies`: the later end of each reciprocal pair.
    let bodies: HashSet<GlobalNodeId> = links
        .iter()
        .filter(|link| answered(link))
        .map(|link| link.to)
        .filter(|body| !nav_blocks.contains(body) && !is_heading(body))
        .collect();

    let mut roles: HashMap<GlobalNodeId, NoteRole> =
        bodies.iter().map(|body| (*body, NoteRole::Body)).collect();
    for link in &links {
        let cites = if link.label {
            answered(link)
        } else {
            precedes(&order, &link.from, &link.to)
        };
        if cites && bodies.contains(&link.to) {
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

    /// One `Role::Paragraph` per entry in `link_counts`, holding a run of text
    /// and then that many `Role::Link` children, with the ids of each.
    fn blocks(link_counts: &[usize]) -> (Chapter, Vec<(NodeId, Vec<NodeId>)>) {
        let mut chapter = Chapter::new();
        let mut out = Vec::new();
        for &count in link_counts {
            let block = chapter.alloc_node(Node::new(Role::Paragraph));
            chapter.append_child(NodeId::ROOT, block);
            append_text(&mut chapter, block, "text");
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

    /// One `role` block per entry of `labels`, holding nothing but a
    /// `Role::Link` around that label, with the link ids.
    fn link_blocks(role: Role, labels: &[&str]) -> (Chapter, Vec<NodeId>) {
        let mut chapter = Chapter::new();
        let mut links = Vec::new();
        for label in labels {
            let block = chapter.alloc_node(Node::new(role));
            chapter.append_child(NodeId::ROOT, block);
            let link = chapter.alloc_node(Node::new(Role::Link));
            chapter.append_child(block, link);
            append_text(&mut chapter, link, label);
            links.push(link);
        }
        (chapter, links)
    }

    fn append_text(chapter: &mut Chapter, parent: NodeId, text: &str) -> NodeId {
        let range = chapter.append_text(text);
        let node = chapter.alloc_node(Node::text(range));
        chapter.append_child(parent, node);
        node
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
    fn a_title_the_navigation_names_through_its_own_link_is_no_note() {
        let (contents, contents_blocks) = blocks(&[1]);
        let (section, titles) = link_blocks(Role::Paragraph, &["The Tide"]);
        let entry = GlobalNodeId::new(ChapterId(0), contents_blocks[0].1[0]);
        let title_link = GlobalNodeId::new(ChapterId(1), titles[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(entry, title_link);
        builder.add_internal(title_link, entry);
        let resolved = builder.build();

        let roles = detect_notes(
            &spine(vec![(ChapterId(0), contents), (ChapterId(1), section)]),
            &resolved,
            &HashSet::from([title_link]),
        );
        assert!(roles.is_empty());
    }

    #[test]
    fn a_note_the_navigation_names_inside_its_text_is_a_note() {
        let (text, text_blocks) = blocks(&[1]);
        let (notes, note_blocks) = blocks(&[1]);
        let marker = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, marker);
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &builder.build(),
            &HashSet::from([back]),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
    }

    #[test]
    fn see_also_entries_linking_each_other_are_no_note() {
        let mut index = Chapter::new();
        let mut entry = |heading: &str, see: &str| {
            let block = index.alloc_node(Node::new(Role::Paragraph));
            index.append_child(NodeId::ROOT, block);
            append_text(&mut index, block, &format!("{heading}, 69, 115 see also "));
            let link = index.alloc_node(Node::new(Role::Link));
            index.append_child(block, link);
            append_text(&mut index, link, see);
            (block, link)
        };
        let (castor, to_dioscuri) = entry("Castor", "Dioscuri");
        let (dioscuri, to_castor) = entry("Dioscuri", "Castor");

        let node = |id| GlobalNodeId::new(ChapterId(0), id);
        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(node(to_dioscuri), node(dioscuri));
        builder.add_internal(node(to_castor), node(castor));
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), index)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert!(roles.is_empty());
    }

    #[test]
    fn a_note_linking_back_to_its_marker_paragraph_is_a_note() {
        let (text, text_blocks) = blocks(&[1]);
        let (mut notes, note_blocks) = blocks(&[1]);
        append_text(&mut notes, note_blocks[0].1[0], "1.");
        let marker = GlobalNodeId::new(ChapterId(0), text_blocks[0].1[0]);
        let paragraph = GlobalNodeId::new(ChapterId(0), text_blocks[0].0);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, paragraph);
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
    }

    /// One `Role::Paragraph` of `text`, an empty `Role::Link` anchor, then a
    /// `Role::Link` marker reading `number`: the chapter, paragraph, anchor
    /// and marker.
    fn marked_paragraph(text: &str, number: &str) -> (Chapter, NodeId, NodeId, NodeId) {
        let mut chapter = Chapter::new();
        let para = chapter.alloc_node(Node::new(Role::Paragraph));
        chapter.append_child(NodeId::ROOT, para);
        append_text(&mut chapter, para, text);
        let anchor = chapter.alloc_node(Node::new(Role::Link));
        chapter.append_child(para, anchor);
        let marker = chapter.alloc_node(Node::new(Role::Link));
        chapter.append_child(para, marker);
        append_text(&mut chapter, marker, number);
        (chapter, para, anchor, marker)
    }

    /// `NoteRole`s for `marker` in `text` linking to a note whose back link,
    /// reading `number`, lands on `back_target`.
    fn note_roles_for(
        text: Chapter,
        marker: NodeId,
        number: &str,
        back_target: NodeId,
    ) -> HashMap<GlobalNodeId, NoteRole> {
        let (mut notes, note_blocks) = blocks(&[1]);
        append_text(&mut notes, note_blocks[0].1[0], number);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);
        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(GlobalNodeId::new(ChapterId(0), marker), back);
        builder.add_internal(back, GlobalNodeId::new(ChapterId(0), back_target));
        detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &builder.build(),
            &HashSet::new(),
        )
    }

    #[test]
    fn a_note_returning_to_the_anchor_beside_its_marker_is_a_note() {
        let (text, _, anchor, marker) = marked_paragraph("1. First things", "1");
        let roles = note_roles_for(text, marker, "1", anchor);
        assert_eq!(
            roles.get(&GlobalNodeId::new(ChapterId(0), marker)),
            Some(&NoteRole::Reference)
        );
    }

    #[test]
    fn a_number_opening_a_longer_one_names_no_paragraph() {
        let (text, para, _, marker) = marked_paragraph("14. those that resemble flies.", "1");
        let roles = note_roles_for(text, marker, "1", para);
        assert_eq!(
            roles.get(&GlobalNodeId::new(ChapterId(0), marker)),
            Some(&NoteRole::Reference)
        );
    }

    #[test]
    fn a_contents_entry_and_the_title_it_opens_are_no_note() {
        let (contents, entries) = link_blocks(Role::Paragraph, &["The Tide"]);
        let (section, titles) = link_blocks(Role::Paragraph, &["The Tide"]);
        let entry = GlobalNodeId::new(ChapterId(0), entries[0]);
        let title = GlobalNodeId::new(ChapterId(1), titles[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(entry, title);
        builder.add_internal(title, entry);
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), contents), (ChapterId(1), section)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert!(roles.is_empty());
    }

    #[test]
    fn a_title_linking_back_to_a_contents_page_after_it_is_no_note() {
        let (section, titles) = link_blocks(Role::Heading(1), &["Before"]);
        let (contents, entries) = link_blocks(Role::ListItem, &["Before"]);
        let title = GlobalNodeId::new(ChapterId(0), titles[0]);
        let entry = GlobalNodeId::new(ChapterId(1), entries[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(title, entry);
        builder.add_internal(entry, title);
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), section), (ChapterId(1), contents)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert!(roles.is_empty());
    }

    #[test]
    fn a_lone_marker_answered_from_prose_is_a_note() {
        let (text, markers) = link_blocks(Role::Paragraph, &["27"]);
        let (notes, note_blocks) = blocks(&[1]);
        let marker = GlobalNodeId::new(ChapterId(0), markers[0]);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);

        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, marker);
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), text), (ChapterId(1), notes)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
        assert_eq!(
            roles.get(&GlobalNodeId::new(ChapterId(1), note_blocks[0].0)),
            Some(&NoteRole::Body)
        );
    }

    #[test]
    fn a_marker_beside_a_title_still_opens_its_note() {
        let mut section = Chapter::new();
        let heading = section.alloc_node(Node::new(Role::Heading(1)));
        section.append_child(NodeId::ROOT, heading);
        append_text(&mut section, heading, "Introduction");
        let marker = section.alloc_node(Node::new(Role::Link));
        section.append_child(heading, marker);
        append_text(&mut section, marker, "1");
        let (notes, note_blocks) = blocks(&[1]);

        let marker = GlobalNodeId::new(ChapterId(0), marker);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);
        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, GlobalNodeId::new(ChapterId(0), heading));
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), section), (ChapterId(1), notes)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
    }

    #[test]
    fn a_marker_beside_an_image_still_opens_its_note() {
        let mut figure = Chapter::new();
        let block = figure.alloc_node(Node::new(Role::Paragraph));
        figure.append_child(NodeId::ROOT, block);
        let image = figure.alloc_node(Node::new(Role::Image));
        figure.append_child(block, image);
        let marker = figure.alloc_node(Node::new(Role::Link));
        figure.append_child(block, marker);
        append_text(&mut figure, marker, "1");
        let (notes, note_blocks) = blocks(&[1]);

        let marker = GlobalNodeId::new(ChapterId(0), marker);
        let back = GlobalNodeId::new(ChapterId(1), note_blocks[0].1[0]);
        let mut builder = ResolvedLinksBuilder::new();
        builder.add_internal(marker, back);
        builder.add_internal(back, marker);
        let roles = detect_notes(
            &spine(vec![(ChapterId(0), figure), (ChapterId(1), notes)]),
            &builder.build(),
            &HashSet::new(),
        );
        assert_eq!(roles.get(&marker), Some(&NoteRole::Reference));
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
