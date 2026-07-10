//! In-place KFX container editing — the shared surgical-write harness.
//!
//! Every surgical KFX edit has the same shape: parse the container, transform a
//! chosen handful of entities, and pass every *other* entity through byte for
//! byte, letting [`serialize_container`] recompute the index-table offsets and
//! `kfxgen_payload_sha1`. [`super::cover_replace`] was the first instance;
//! this module factors that mechanism out so cover / image / metadata / nav
//! edits all share one audited core instead of re-deriving the container walk.
//!
//! Usage: [`crate::kfx_to_epub::loader::load`] the container first to resolve
//! whatever identity the edit needs (symbol table, `by_type`, `raw_media`,
//! metadata), then call [`edit_container`] with a callback that returns an
//! [`EntityEdit`] per entity. The callback captures the resolved identity; the
//! harness owns all the offset/slice/serialize bookkeeping.
//!
//! What this harness deliberately does **not** touch: the doc-symbol table and
//! format-capabilities section pass through verbatim. Edits that introduce a
//! genuinely new doc-symbol need a symbol-table grow step this harness does not
//! provide — text and most metadata/nav *values* are inline Ion strings and
//! need no new symbol, so the common edits don't hit that limit.

use crate::kfx::container::{
    self, EntityLoc, get_field, parse_container_header, parse_index_table,
};
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::serialization::{
    SerializedEntity, create_entity_data, create_raw_media_data, serialize_container,
};
use crate::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::ConvertError;

/// One entity as presented to an [`edit_container`] callback: its index-table
/// entry plus the verbatim ENTY-wrapped bytes currently backing it.
pub struct EntityView<'a> {
    loc: EntityLoc,
    raw: &'a [u8],
}

impl<'a> EntityView<'a> {
    /// The entity id — the fragment-name symbol id. Resolve to a name via the
    /// `SymbolTable` from the same `loader::load` (`book.symbols.resolve(id)`).
    #[inline]
    pub fn id(&self) -> u32 {
        self.loc.id
    }

    /// The entity type id (e.g. `KfxSymbol::ExternalResource as u32`).
    #[inline]
    pub fn type_id(&self) -> u32 {
        self.loc.type_id
    }

    /// True if this entity has the given KFX type.
    #[inline]
    pub fn is_type(&self, t: KfxSymbol) -> bool {
        self.loc.type_id == t as u32
    }

    /// The index-table entry (id / type / offset / length), copied out.
    #[inline]
    pub fn loc(&self) -> EntityLoc {
        self.loc
    }

    /// The full ENTY-wrapped source bytes, verbatim.
    #[inline]
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// The entity payload after the ENTY header — raw media bytes for
    /// `bcRawMedia`/`bcRawFont`, or the Ion body for everything else.
    #[inline]
    pub fn media(&self) -> &'a [u8] {
        container::skip_enty_header(self.raw)
    }

    /// Parse the payload as an Ion value. Errors if it is not valid Ion — do
    /// not call this on a raw-media entity (an image/font); use [`media`] there.
    ///
    /// [`media`]: Self::media
    pub fn parse_ion(&self) -> Result<IonValue, ConvertError> {
        IonParser::new(self.media())
            .parse()
            .map_err(|e| ConvertError::InvalidKfx(format!("parse entity {}: {e}", self.loc.id)))
    }
}

/// The transform [`edit_container`] applies to one entity.
pub enum EntityEdit {
    /// Pass the entity through byte for byte. The default for the overwhelming
    /// majority of entities in any surgical edit.
    Keep,
    /// Replace with a fresh Ion value; the harness re-wraps it in an ENTY
    /// header (`create_entity_data`).
    Ion(IonValue),
    /// Replace raw-media bytes (an image or font — stored without Ion encoding;
    /// `create_raw_media_data`).
    RawMedia(Vec<u8>),
    /// Replace with bytes you have already ENTY-wrapped yourself. Advanced —
    /// you own the framing; prefer [`Ion`](Self::Ion)/[`RawMedia`](Self::RawMedia).
    Bytes(Vec<u8>),
}

/// Parse `kfx_bytes`, call `edit` once per entity in index-table order, and
/// re-serialize the container. Entities the callback returns [`EntityEdit::Keep`]
/// for are copied verbatim; the rest are rebuilt from the returned value. The
/// doc-symbol table and format-capabilities section pass through unchanged, and
/// [`serialize_container`] recomputes every index-table offset and the payload
/// SHA-1.
///
/// An `Err` from the callback aborts the whole edit — nothing is written and the
/// error propagates. This is the surgical write half of the "save = re-ingest
/// the edited source" seam for KFX-source books.
pub fn edit_container(
    kfx_bytes: &[u8],
    mut edit: impl FnMut(&EntityView) -> Result<EntityEdit, ConvertError>,
) -> Result<Vec<u8>, ConvertError> {
    let layout = parse_layout(kfx_bytes)?;

    // Passthrough sections. Out-of-range offsets collapse to empty, matching the
    // original `cover_replace` behavior (valid KFX always keep these in range).
    let slice = |sec: Option<(usize, usize)>| -> &[u8] {
        match sec {
            Some((o, l)) if o + l <= kfx_bytes.len() => &kfx_bytes[o..o + l],
            _ => &[],
        }
    };
    let symtab_ion = slice(layout.symtab);
    let format_caps_ion = slice(layout.format_caps);

    let mut out_entities: Vec<SerializedEntity> = Vec::with_capacity(layout.entities.len());
    for e in &layout.entities {
        if e.offset + e.length > kfx_bytes.len() {
            return Err(ConvertError::InvalidKfx(
                "entity payload out of bounds".into(),
            ));
        }
        let view = EntityView {
            loc: *e,
            raw: &kfx_bytes[e.offset..e.offset + e.length],
        };
        let data = match edit(&view)? {
            EntityEdit::Keep => view.raw.to_vec(),
            EntityEdit::Ion(v) => create_entity_data(&v),
            EntityEdit::RawMedia(b) => create_raw_media_data(&b),
            EntityEdit::Bytes(b) => b,
        };
        out_entities.push(SerializedEntity {
            id: e.id,
            entity_type: e.type_id,
            data,
        });
    }

    Ok(serialize_container(
        &layout.container_id,
        &out_entities,
        symtab_ion,
        format_caps_ion,
    ))
}

/// The passthrough sections + entity table of a parsed container: the read half
/// of an edit, factored from `cover_replace`'s inline container-info walk.
struct ContainerLayout {
    container_id: String,
    /// `(offset, length)` of the doc-symbol Ion section, if declared.
    symtab: Option<(usize, usize)>,
    /// `(offset, length)` of the format-capabilities Ion section, if declared.
    format_caps: Option<(usize, usize)>,
    entities: Vec<EntityLoc>,
}

/// Walk the container header + container-info to recover the passthrough section
/// ranges, the container id, and the entity index table.
fn parse_layout(kfx_bytes: &[u8]) -> Result<ContainerLayout, ConvertError> {
    let header =
        parse_container_header(kfx_bytes).map_err(|e| ConvertError::InvalidKfx(e.to_string()))?;
    let ci_end = header.container_info_offset + header.container_info_length;
    if ci_end > kfx_bytes.len() {
        return Err(ConvertError::InvalidKfx(
            "container info out of bounds".into(),
        ));
    }
    let ci_fields = {
        let mut p = IonParser::new(&kfx_bytes[header.container_info_offset..ci_end]);
        p.parse()
            .ok()
            .and_then(|v| v.as_struct().map(<[_]>::to_vec))
            .ok_or_else(|| ConvertError::InvalidKfx("container info is not a struct".into()))?
    };
    let geti = |sym: KfxSymbol| {
        get_field(&ci_fields, sym as u64)
            .and_then(IonValue::as_int)
            .map(|n| n as usize)
    };
    let section = |off: KfxSymbol, len: KfxSymbol| geti(off).zip(geti(len));

    let idx_off = geti(KfxSymbol::Bcindextaboffset)
        .ok_or_else(|| ConvertError::InvalidKfx("no index table offset".into()))?;
    let idx_len = geti(KfxSymbol::Bcindextablength)
        .ok_or_else(|| ConvertError::InvalidKfx("no index table length".into()))?;
    let container_id = get_field(&ci_fields, KfxSymbol::Bccontid as u64)
        .and_then(IonValue::as_string)
        .unwrap_or("")
        .to_string();
    let symtab = section(KfxSymbol::Bcdocsymboloffset, KfxSymbol::Bcdocsymbollength);
    let format_caps = section(
        KfxSymbol::Bcfcapabilitiesoffset,
        KfxSymbol::Bcfcapabilitieslength,
    );

    if idx_off + idx_len > kfx_bytes.len() {
        return Err(ConvertError::InvalidKfx("index table out of bounds".into()));
    }
    let entities = parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    Ok(ContainerLayout {
        container_id,
        symtab,
        format_caps,
        entities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kfx_to_epub::loader;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.kfx";

    /// An all-`Keep` edit is a faithful passthrough: the rewritten container
    /// re-loads, preserves every entity + raw-media resource, and still converts
    /// to EPUB. This is the harness's core guarantee — everything not touched
    /// survives byte-for-byte through the offset/sha1 recompute.
    #[test]
    fn keep_all_is_faithful_passthrough() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let before = loader::load(&kfx).expect("load original");

        let out = edit_container(&kfx, |_| Ok(EntityEdit::Keep)).expect("edit_container");
        let after = loader::load(&out).expect("rewritten container must re-load");

        assert_eq!(
            before.raw_media.len(),
            after.raw_media.len(),
            "no raw-media resources added or dropped"
        );
        for (k, v) in &before.raw_media {
            assert_eq!(
                after.raw_media.get(k),
                Some(v),
                "raw-media {k} must pass through byte-for-byte"
            );
        }
        let mut before_types: Vec<_> = before.by_type.iter().map(|(t, m)| (*t, m.len())).collect();
        let mut after_types: Vec<_> = after.by_type.iter().map(|(t, m)| (*t, m.len())).collect();
        before_types.sort_unstable();
        after_types.sort_unstable();
        assert_eq!(
            before_types, after_types,
            "every fragment type + count preserved"
        );
        assert_eq!(
            before.metadata.title, after.metadata.title,
            "metadata survives passthrough"
        );
        assert!(
            crate::kfx_to_epub::convert_to_epub(&out).is_ok(),
            "passthrough container must still convert to EPUB"
        );
    }

    /// A targeted `RawMedia` edit changes exactly one resource and leaves the
    /// rest untouched — the minimal surgical-swap the harness exists for.
    #[test]
    fn raw_media_edit_swaps_one_resource() {
        let kfx = std::fs::read(FIXTURE).expect("read fixture");
        let before = loader::load(&kfx).expect("load original");
        // Pick any raw-media resource; find the entity id that resolves to it.
        let (target_name, _) = before
            .raw_media
            .iter()
            .next()
            .expect("fixture has raw media");
        let new_bytes = vec![0xFF, 0xD8, 0xFF, 0xD9]; // tiny stand-in payload

        let out = edit_container(&kfx, |view| {
            if view.is_type(KfxSymbol::Bcrawmedia)
                && before.symbols.resolve(view.id() as u64) == *target_name
            {
                Ok(EntityEdit::RawMedia(new_bytes.clone()))
            } else {
                Ok(EntityEdit::Keep)
            }
        })
        .expect("edit_container");

        let after = loader::load(&out).expect("rewritten container must re-load");
        assert_eq!(
            after.raw_media.get(target_name).map(Vec::as_slice),
            Some(new_bytes.as_slice()),
            "target resource holds the new bytes"
        );
        assert_eq!(
            before.raw_media.len(),
            after.raw_media.len(),
            "no resources added or dropped by a targeted swap"
        );
        // Every other resource is untouched.
        for (k, v) in &before.raw_media {
            if k != target_name {
                assert_eq!(after.raw_media.get(k), Some(v), "{k} must be untouched");
            }
        }
    }

    #[test]
    fn non_kfx_bytes_error() {
        assert!(edit_container(b"not a kfx container", |_| Ok(EntityEdit::Keep)).is_err());
    }
}
