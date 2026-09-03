//! In-place KFX container editing — the shared surgical-write harness.

use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::package::KfxPackage;

/// One entity as presented to an [`edit_container`] callback. This is
/// [`package::Entity`](crate::formats::kfx::package::Entity) — its id, type and
/// verbatim ENTY-wrapped bytes, with `parse_ion` for the Ion body.
pub use crate::formats::kfx::package::Entity as EntityView;

/// The transform [`edit_container`] applies to one entity.
pub enum EntityEdit {
    /// Pass the entity through byte for byte.
    Keep,
    /// Replace with a fresh Ion value; the harness re-wraps it in an ENTY
    /// header (`create_entity_data`).
    Ion(IonValue),
    /// Replace raw-media bytes (an image or font — stored without Ion encoding;
    /// `create_raw_media_data`).
    RawMedia(Vec<u8>),
    /// Replace with ENTY-wrapped bytes, framing included.
    /// [`Ion`](Self::Ion) and [`RawMedia`](Self::RawMedia) frame their own.
    Bytes(Vec<u8>),
}

/// Parse `kfx_bytes`, call `edit` once per entity in index-table order, and
/// re-serialize. [`EntityEdit::Keep`] copies an entity verbatim, and
/// [`KfxPackage::into_bytes`] recomputes every offset and the payload SHA-1.
pub fn edit_container(
    kfx_bytes: &[u8],
    mut edit: impl FnMut(&EntityView) -> Result<EntityEdit, KfxError>,
) -> Result<Vec<u8>, KfxError> {
    let mut pkg = KfxPackage::parse(kfx_bytes)?;
    for i in 0..pkg.entities().len() {
        // The view borrows the package, so resolve the edit before applying it.
        let action = edit(&pkg.entities()[i])?;
        match action {
            EntityEdit::Keep => {}
            EntityEdit::Ion(v) => pkg.set_ion(i, &v)?,
            EntityEdit::RawMedia(b) => pkg.set_media(i, &b)?,
            EntityEdit::Bytes(b) => pkg.set_raw(i, b)?,
        }
    }
    Ok(pkg.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::kfx::loader;
    use crate::formats::kfx::symbols::KfxSymbol;

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    /// An all-`Keep` edit is a faithful passthrough: the rewritten container
    /// re-loads, holds every entity and raw-media resource, and converts to
    /// EPUB.
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
            crate::formats::kfx::converts_to_epub(&out),
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
