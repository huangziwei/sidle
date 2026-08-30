//! IR optimization passes.

mod fuse;
mod merge;
mod pass;
mod predicates;
mod prune;
mod table;
mod vacuum;
mod wrap;

use crate::model::Chapter;

/// Run all optimization passes on a chapter.
///
/// Passes are ordered for maximum effectiveness:
pub fn optimize(chapter: &mut Chapter) {
    vacuum::vacuum(chapter);
    merge::merge_adjacent_spans(chapter);
    fuse::fuse_lists(chapter);
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

        // Create a structure with adjacent lists:
        // Root > Container > [ul1, ul2]
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

        // Before: 2 lists under container
        assert_eq!(chapter.children(container).count(), 2);

        optimize(&mut chapter);

        // After optimization:
        // - Lists fused into 1
        // = 1 child (the fused list)
        let children: Vec<_> = chapter.children(container).collect();
        assert_eq!(children.len(), 1);

        // The fused list should have 2 items
        let list_items: Vec<_> = chapter.children(children[0]).collect();
        assert_eq!(list_items.len(), 2);
    }
}
