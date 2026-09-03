//! Helpers shared between the mechanical and fast merge paths.

use super::node::IonNode;

/// Rewrite a `$490` book_metadata `cde_content_type` to `"PDOC"`, so the device
/// reads the embedded cover instead of probing the ASIN catalog. `true` if changed.
pub fn rewrite_cde_content_type_pdoc(value: &mut IonNode) -> bool {
    let Some(top) = value.as_struct_mut() else {
        return false;
    };
    let Some(categories) = top.iter_mut().find(|(k, _)| k == "$491").map(|(_, v)| v) else {
        return false;
    };
    let IonNode::List(cats) = categories else {
        return false;
    };
    let mut changed = false;
    for cat in cats {
        let Some(cat_fields) = cat.as_struct_mut() else {
            continue;
        };
        let name_matches = cat_fields
            .iter()
            .find(|(k, _)| k == "$495")
            .and_then(|(_, v)| v.as_string())
            .is_some_and(|n| n == "kindle_title_metadata");
        if !name_matches {
            continue;
        }
        let Some(kv_list) = cat_fields
            .iter_mut()
            .find(|(k, _)| k == "$258")
            .map(|(_, v)| v)
        else {
            continue;
        };
        let IonNode::List(kvs) = kv_list else {
            continue;
        };
        for kv in kvs {
            let Some(kv_fields) = kv.as_struct_mut() else {
                continue;
            };
            let key_is_cct = kv_fields
                .iter()
                .find(|(k, _)| k == "$492")
                .and_then(|(_, v)| v.as_string())
                .is_some_and(|n| n == "cde_content_type");
            if !key_is_cct {
                continue;
            }
            for (k, v) in kv_fields.iter_mut() {
                if k != "$307" {
                    continue;
                }
                if let IonNode::String(s) = v
                    && s != "PDOC"
                {
                    *s = "PDOC".to_string();
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Calibre-style `CR!` container id for a merged bundle when no source
pub use crate::formats::kfx::serialization::generate_container_id;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_metadata(content_type: &str) -> IonNode {
        IonNode::Struct(vec![(
            "$491".into(),
            IonNode::List(vec![
                IonNode::Struct(vec![
                    (
                        "$495".into(),
                        IonNode::String("kindle_ebook_metadata".into()),
                    ),
                    ("$258".into(), IonNode::List(vec![])),
                ]),
                IonNode::Struct(vec![
                    (
                        "$495".into(),
                        IonNode::String("kindle_title_metadata".into()),
                    ),
                    (
                        "$258".into(),
                        IonNode::List(vec![
                            IonNode::Struct(vec![
                                ("$492".into(), IonNode::String("ASIN".into())),
                                ("$307".into(), IonNode::String("B0XXXX".into())),
                            ]),
                            IonNode::Struct(vec![
                                ("$492".into(), IonNode::String("cde_content_type".into())),
                                ("$307".into(), IonNode::String(content_type.into())),
                            ]),
                        ]),
                    ),
                ]),
            ]),
        )])
    }

    fn get_cde_content_type(value: &IonNode) -> Option<String> {
        let cats = value.get_field("$491")?.as_list()?;
        for cat in cats {
            if cat
                .get_field("$495")
                .and_then(|n| n.as_string())
                .is_some_and(|n| n == "kindle_title_metadata")
            {
                for kv in cat.get_field("$258")?.as_list()? {
                    let key_matches = kv
                        .get_field("$492")
                        .and_then(|n| n.as_string())
                        .is_some_and(|n| n == "cde_content_type");
                    if key_matches {
                        return kv.get_field("$307")?.as_string().map(|s| s.to_string());
                    }
                }
            }
        }
        None
    }

    #[test]
    fn rewrites_ebok_to_pdoc() {
        let mut v = make_metadata("EBOK");
        assert!(rewrite_cde_content_type_pdoc(&mut v));
        assert_eq!(get_cde_content_type(&v).as_deref(), Some("PDOC"));
    }

    #[test]
    fn leaves_pdoc_alone_and_reports_unchanged() {
        let mut v = make_metadata("PDOC");
        assert!(!rewrite_cde_content_type_pdoc(&mut v));
        assert_eq!(get_cde_content_type(&v).as_deref(), Some("PDOC"));
    }

    #[test]
    fn returns_false_when_metadata_struct_lacks_categories() {
        let mut v = IonNode::Struct(vec![]);
        assert!(!rewrite_cde_content_type_pdoc(&mut v));
    }
}
