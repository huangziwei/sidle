//! IR optimization passes.

mod fuse;
mod merge;
mod pass;
mod predicates;
mod prune;
mod split;
mod table;
mod wrap;

use crate::model::Chapter;

/// Run all optimization passes on a chapter.
pub fn optimize(chapter: &mut Chapter) {
    merge::merge_adjacent_spans(chapter);
    fuse::fuse_lists(chapter);
    split::split_inlines_around_blocks(chapter);
    wrap::wrap_mixed_content(chapter);
    table::normalize_table_structure(chapter);
    prune::prune_empty(chapter);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeId, Role};

    #[test]
    fn test_full_pipeline() {
        let mut chapter = Chapter::new();

        let container = chapter.alloc_node(Node::new(Role::Container));
        chapter.append_child(NodeId::ROOT, container);

        let ul1 = chapter.alloc_node(Node::new(Role::UnorderedList));
        chapter.append_child(container, ul1);
        let li1 = chapter.alloc_node(Node::new(Role::ListItem));
        chapter.append_child(ul1, li1);

        let ul2 = chapter.alloc_node(Node::new(Role::UnorderedList));
        chapter.append_child(container, ul2);
        let li2 = chapter.alloc_node(Node::new(Role::ListItem));
        chapter.append_child(ul2, li2);

        assert_eq!(chapter.children(container).count(), 2);

        optimize(&mut chapter);

        let children: Vec<_> = chapter.children(container).collect();
        assert_eq!(children.len(), 1);

        let list_items: Vec<_> = chapter.children(children[0]).collect();
        assert_eq!(list_items.len(), 2);
    }
}
