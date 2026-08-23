//! `auxiliary_data` ($597) and `ruby_content` ($756) fragments for KFX export.
//!
//! One `auxiliary_data` entity per section marks it a navigation target.

use super::context::ExportContext;
use super::fragment::KfxFragment;
use super::ion::IonValue;
use super::symbols::KfxSymbol;

/// The `auxiliary_data` ($597) fragment naming `section_name` a target
/// section, keyed `<section_name>-ad`.
pub fn build_auxiliary_data_fragment(section_name: &str, ctx: &mut ExportContext) -> KfxFragment {
    let kfx_id = format!("{}-ad", section_name);
    let kfx_id_symbol = ctx.symbols.get_or_intern(&kfx_id);

    let metadata_entry = IonValue::Struct(vec![
        (
            KfxSymbol::Key as u64,
            IonValue::String("IS_TARGET_SECTION".to_string()),
        ),
        (KfxSymbol::Value as u64, IonValue::Bool(true)),
    ]);

    let ion = IonValue::Struct(vec![
        (KfxSymbol::KfxId as u64, IonValue::Symbol(kfx_id_symbol)),
        (
            KfxSymbol::Metadata as u64,
            IonValue::List(vec![metadata_entry]),
        ),
    ]);

    KfxFragment::new(KfxSymbol::AuxiliaryData, &kfx_id, ion)
}

/// The `ruby_content` fragments in `ctx.ruby_registry`, up to
/// `RubyContentRegistry::ENTRIES_PER_FRAGMENT` annotations each, keyed
/// `b_ruby_<N>`. A style_event cites one by `ruby_name` and 1-based `ruby_id`.
pub fn build_ruby_content_fragments(ctx: &mut ExportContext) -> Vec<KfxFragment> {
    let frag_count = ctx.ruby_registry.fragment_count();
    if frag_count == 0 {
        return Vec::new();
    }

    let mut frags = Vec::with_capacity(frag_count);
    for frag_idx in 0..frag_count {
        let entries: Vec<String> = ctx.ruby_registry.fragment_entries(frag_idx).to_vec();
        let kfx_id = format!("b_ruby_{}", frag_idx);
        let kfx_id_symbol = ctx.symbols.get_or_intern(&kfx_id);
        let style_symbol = ctx.default_style_symbol;

        let content_list: Vec<IonValue> = entries
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let ruby_id = (i + 1) as i64;
                let entry_id = ctx.fragment_ids.next_id() as i64;
                IonValue::Struct(vec![
                    (KfxSymbol::Id as u64, IonValue::Int(entry_id)),
                    (KfxSymbol::Style as u64, IonValue::Symbol(style_symbol)),
                    (
                        KfxSymbol::Type as u64,
                        IonValue::Symbol(KfxSymbol::Text as u64),
                    ),
                    (KfxSymbol::RubyId as u64, IonValue::Int(ruby_id)),
                    (KfxSymbol::Content as u64, IonValue::String(text.clone())),
                ])
            })
            .collect();

        let ion = IonValue::Struct(vec![
            (KfxSymbol::RubyName as u64, IonValue::Symbol(kfx_id_symbol)),
            (KfxSymbol::ContentList as u64, IonValue::List(content_list)),
        ]);

        frags.push(KfxFragment::new(KfxSymbol::RubyContent, &kfx_id, ion));
    }
    frags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::kfx::fragment::FragmentData;

    #[test]
    fn test_build_auxiliary_data_fragment() {
        let mut ctx = ExportContext::new();
        let frag = build_auxiliary_data_fragment("c0", &mut ctx);

        assert_eq!(frag.ftype, KfxSymbol::AuxiliaryData as u64);
        assert_eq!(frag.fid, "c0-ad");

        if let FragmentData::Ion(IonValue::Struct(fields)) = &frag.data {
            let kfx_id = fields.iter().find(|(id, _)| *id == KfxSymbol::KfxId as u64);
            assert!(kfx_id.is_some(), "should have kfx_id");
            assert!(
                matches!(kfx_id, Some((_, IonValue::Symbol(_)))),
                "kfx_id should be a symbol"
            );

            // Check metadata
            let metadata = fields
                .iter()
                .find(|(id, _)| *id == KfxSymbol::Metadata as u64);
            assert!(metadata.is_some(), "should have metadata");
            if let Some((_, IonValue::List(entries))) = metadata {
                assert_eq!(entries.len(), 1);
                if let IonValue::Struct(entry_fields) = &entries[0] {
                    let key = entry_fields
                        .iter()
                        .find(|(id, _)| *id == KfxSymbol::Key as u64);
                    let value = entry_fields
                        .iter()
                        .find(|(id, _)| *id == KfxSymbol::Value as u64);

                    if let Some((_, IonValue::String(k))) = key {
                        assert_eq!(k, "IS_TARGET_SECTION");
                    }
                    if let Some((_, IonValue::Bool(v))) = value {
                        assert!(*v);
                    }
                }
            }
        } else {
            panic!("expected Ion struct data");
        }
    }
}
