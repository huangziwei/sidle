//! Pass 3: Inline Split (Blocks Lifted out of Inline Elements)

use crate::model::{Chapter, Node, NodeId, Role};

use super::pass::walk_bottom_up;
use super::predicates::is_inline_role;

/// Lift each block an `Inline` or `Link` holds out to the nearest block
/// ancestor, splitting the inline into fragments around it.
pub fn split_inlines_around_blocks(chapter: &mut Chapter) {
    walk_bottom_up(chapter, |chapter, parent| {
        if !is_block(chapter, parent) || !chapter.children(parent).any(|c| holds_block(chapter, c))
        {
            return;
        }
        for child in take_children(chapter, parent) {
            let pieces = if holds_block(chapter, child) {
                split(chapter, child)
            } else {
                vec![child]
            };
            for piece in pieces {
                chapter.append_child(parent, piece);
            }
        }
    });
}

fn is_block(chapter: &Chapter, node: NodeId) -> bool {
    chapter.node(node).is_some_and(|n| !is_inline_role(n.role))
}

/// True for an `Inline` or `Link` with a block anywhere in its inline subtree.
fn holds_block(chapter: &Chapter, node: NodeId) -> bool {
    chapter
        .node(node)
        .is_some_and(|n| matches!(n.role, Role::Inline | Role::Link))
        && chapter
            .children(node)
            .any(|c| is_block(chapter, c) || holds_block(chapter, c))
}

fn take_children(chapter: &mut Chapter, parent: NodeId) -> Vec<NodeId> {
    let children: Vec<NodeId> = chapter.children(parent).collect();
    for &child in &children {
        chapter.detach(child);
    }
    children
}

/// Split `inline` into the nodes that take its place: fragments of `inline`
/// holding its runs, each block it held lifted out between them.
fn split(chapter: &mut Chapter, inline: NodeId) -> Vec<NodeId> {
    let mut pieces = Vec::new();
    let mut original = Some(inline);
    let mut fragment: Option<NodeId> = None;
    for child in take_children(chapter, inline) {
        let lifted = if holds_block(chapter, child) {
            split(chapter, child)
        } else {
            vec![child]
        };
        for piece in lifted {
            if is_block(chapter, piece) {
                fragment = None;
                carry_into(chapter, inline, original.take(), piece);
                pieces.push(piece);
                continue;
            }
            let host = match fragment {
                Some(host) => host,
                None => {
                    let host = original
                        .take()
                        .unwrap_or_else(|| fragment_of(chapter, inline));
                    pieces.push(host);
                    fragment = Some(host);
                    host
                }
            };
            chapter.append_child(host, piece);
        }
    }
    pieces
}

/// Copy `inline`'s role, style and attributes, less its `id`, into a new node.
fn fragment_of(chapter: &mut Chapter, inline: NodeId) -> NodeId {
    let (role, style) = chapter
        .node(inline)
        .map_or((Role::Inline, Default::default()), |n| (n.role, n.style));
    let mut node = Node::new(role);
    node.style = style;
    let fragment = chapter.alloc_node(node);
    copy_attrs(chapter, inline, fragment);
    let class = chapter.semantics.class(inline).map(str::to_owned);
    if let Some(class) = class {
        chapter.semantics.set_class(fragment, &class);
    }
    let style = chapter.semantics.style(inline).map(str::to_owned);
    if let Some(style) = style {
        chapter.semantics.set_style(fragment, &style);
    }
    fragment
}

fn copy_attrs(chapter: &mut Chapter, from: NodeId, to: NodeId) {
    for name in ["href", "title", "lang", "epub:type", "role", "datetime"] {
        let value = chapter.semantics.get_attr(from, name).map(str::to_owned);
        if let Some(value) = value {
            chapter.semantics.set_attr(to, name, &value);
        }
    }
}

/// Anchor `original`'s `id` in `block`, and link `block`'s inline runs to
/// `inline`'s `href`.
fn carry_into(chapter: &mut Chapter, inline: NodeId, original: Option<NodeId>, block: NodeId) {
    let id = original.and_then(|o| chapter.semantics.id(o).map(str::to_owned));
    if let (Some(original), Some(id)) = (original, id) {
        if chapter.semantics.id(block).is_none() {
            chapter.semantics.set_id(block, &id);
        } else {
            let children = take_children(chapter, block);
            chapter.append_child(block, original);
            for child in children {
                chapter.append_child(block, child);
            }
        }
    }
    if chapter.semantics.href(inline).is_some() {
        link_runs(chapter, inline, block);
    }
}

/// Wrap each inline run under `block` in a `Role::Link` to `link`'s `href`,
/// styled as the block holding the run.
fn link_runs(chapter: &mut Chapter, link: NodeId, block: NodeId) {
    let style = chapter.node(block).map(|n| n.style).unwrap_or_default();
    let mut wrapper: Option<NodeId> = None;
    for child in take_children(chapter, block) {
        if is_block(chapter, child) {
            wrapper = None;
            link_runs(chapter, link, child);
            chapter.append_child(block, child);
            continue;
        }
        let host = match wrapper {
            Some(host) => host,
            None => {
                let mut node = Node::new(Role::Link);
                node.style = style;
                let host = chapter.alloc_node(node);
                copy_attrs(chapter, link, host);
                chapter.append_child(block, host);
                wrapper = Some(host);
                host
            }
        };
        chapter.append_child(host, child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(chapter: &mut Chapter, parent: NodeId, s: &str) {
        let range = chapter.append_text(s);
        let node = chapter.alloc_node(Node::text(range));
        chapter.append_child(parent, node);
    }

    fn element(chapter: &mut Chapter, parent: NodeId, role: Role) -> NodeId {
        let node = chapter.alloc_node(Node::new(role));
        chapter.append_child(parent, node);
        node
    }

    /// List each node as its role indented by depth, a text leaf as its text.
    fn outline(chapter: &Chapter) -> Vec<String> {
        fn walk(chapter: &Chapter, id: NodeId, depth: usize, out: &mut Vec<String>) {
            let node = chapter.node(id).unwrap();
            let label = match node.role {
                Role::Text => format!("{:?}", chapter.text(node.text)),
                role => format!("{role:?}"),
            };
            out.push(format!("{}{label}", "  ".repeat(depth)));
            for child in chapter.children(id) {
                walk(chapter, child, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for child in chapter.children(NodeId::ROOT) {
            walk(chapter, child, 0, &mut out);
        }
        out
    }

    #[test]
    fn a_block_in_a_span_splits_the_span_around_it() {
        let mut chapter = Chapter::new();
        let para = element(&mut chapter, NodeId::ROOT, Role::Paragraph);
        let span = element(&mut chapter, para, Role::Inline);
        chapter.semantics.set_class(span, "em");
        text(&mut chapter, span, "one");
        let block = element(&mut chapter, span, Role::Container);
        text(&mut chapter, block, "two");
        text(&mut chapter, span, "three");

        split_inlines_around_blocks(&mut chapter);

        assert_eq!(
            outline(&chapter),
            [
                "Paragraph",
                "  Inline",
                "    \"one\"",
                "  Container",
                "    \"two\"",
                "  Inline",
                "    \"three\"",
            ]
        );
        let fragments: Vec<NodeId> = chapter
            .children(para)
            .filter(|&c| chapter.node(c).unwrap().role == Role::Inline)
            .collect();
        assert_eq!(fragments[0], span);
        assert_eq!(chapter.semantics.class(fragments[1]), Some("em"));
    }

    #[test]
    fn a_link_around_blocks_links_each_block_and_keeps_its_anchor() {
        let mut chapter = Chapter::new();
        let link = element(&mut chapter, NodeId::ROOT, Role::Link);
        chapter.semantics.set_href(link, "#t");
        chapter.semantics.set_id(link, "x");
        for s in ["one", "two"] {
            let para = element(&mut chapter, link, Role::Paragraph);
            text(&mut chapter, para, s);
        }

        split_inlines_around_blocks(&mut chapter);

        assert_eq!(
            outline(&chapter),
            [
                "Paragraph",
                "  Link",
                "    \"one\"",
                "Paragraph",
                "  Link",
                "    \"two\"",
            ]
        );
        let paras: Vec<NodeId> = chapter.children(NodeId::ROOT).collect();
        assert_eq!(chapter.semantics.id(paras[0]), Some("x"));
        assert_eq!(chapter.semantics.id(paras[1]), None);
        for para in paras {
            let link = chapter.children(para).next().unwrap();
            assert_eq!(chapter.semantics.href(link), Some("#t"));
            assert_eq!(chapter.semantics.id(link), None);
        }
    }

    #[test]
    fn an_anchor_beside_a_block_that_has_its_own_id_stays_an_empty_marker() {
        let mut chapter = Chapter::new();
        let span = element(&mut chapter, NodeId::ROOT, Role::Inline);
        chapter.semantics.set_id(span, "x");
        let para = element(&mut chapter, span, Role::Paragraph);
        chapter.semantics.set_id(para, "p");
        text(&mut chapter, para, "one");

        split_inlines_around_blocks(&mut chapter);

        assert_eq!(outline(&chapter), ["Paragraph", "  Inline", "  \"one\""]);
        let marker = chapter.children(para).next().unwrap();
        assert_eq!(marker, span);
        assert_eq!(chapter.semantics.id(para), Some("p"));
    }

    #[test]
    fn nested_inlines_split_at_every_level() {
        let mut chapter = Chapter::new();
        let para = element(&mut chapter, NodeId::ROOT, Role::Paragraph);
        let outer = element(&mut chapter, para, Role::Inline);
        text(&mut chapter, outer, "a");
        let inner = element(&mut chapter, outer, Role::Inline);
        text(&mut chapter, inner, "b");
        let block = element(&mut chapter, inner, Role::Container);
        text(&mut chapter, block, "c");
        text(&mut chapter, inner, "d");
        text(&mut chapter, outer, "e");

        split_inlines_around_blocks(&mut chapter);

        assert_eq!(
            outline(&chapter),
            [
                "Paragraph",
                "  Inline",
                "    \"a\"",
                "    Inline",
                "      \"b\"",
                "  Container",
                "    \"c\"",
                "  Inline",
                "    Inline",
                "      \"d\"",
                "    \"e\"",
            ]
        );
    }

    #[test]
    fn inline_only_content_is_left_alone() {
        let mut chapter = Chapter::new();
        let para = element(&mut chapter, NodeId::ROOT, Role::Paragraph);
        let span = element(&mut chapter, para, Role::Inline);
        text(&mut chapter, span, "a");
        let image = element(&mut chapter, span, Role::Image);
        let before = chapter.children(para).collect::<Vec<_>>();

        split_inlines_around_blocks(&mut chapter);

        assert_eq!(chapter.children(para).collect::<Vec<_>>(), before);
        assert_eq!(chapter.node(image).unwrap().parent, Some(span));
    }
}
