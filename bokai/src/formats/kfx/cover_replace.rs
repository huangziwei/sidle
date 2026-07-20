//! Replace the cover image inside an existing KFX container, in place.
//!
//! Why this exists: a store KFX can ship the *wrong* cover. Verified on
//! こちらあみ子 (ASIN B073J24TDK): `cover_image → e6 → resource/rsrc1JG` is a
//! valid, intact link — but it points at a 1200×1600 "筑摩eBOOKS" publisher
//! house-logo placeholder, not the book's art. The Kindle home tile and
//! sleep-screen render whatever that resource holds, so a sideload shows the
//! logo. After the cover-fetch flow pulls the true cover by ASIN, we swap it
//! into the KFX too — the KFX-side parallel of [`crate`]'s EPUB cover swap.
//!
//! Mechanism: byte-passthrough re-serialize. We resolve the cover's
//! `external_resource` ($164) and its backing `bcRawMedia` ($417) through the
//! same resolver [`crate::formats::kfx::loader`] uses (so the *dynamic* doc-symbol
//! `base_len` is correct — a fixed `KFX_SYMBOL_TABLE.len()` mis-resolves), then
//! emit a fresh container in which only those two entities change and every
//! other entity is copied through verbatim.
//! [`serialize_container`](crate::formats::kfx::serialization::serialize_container) recomputes
//! the index-table offsets and `kfxgen_payload_sha1`.
//!
//! The replacement is normalized through [`sanitize_for_kfx`] first: it strips
//! EXIF (the KOA2 sleep-screen decoder rejects EXIF-tagged JPEGs and falls back
//! to the auto title-card) and transcodes PNG/WebP → JFIF JPEG. We then update
//! the resource's declared `resource_width`/`resource_height` to the real pixel
//! dimensions and, if the slot was previously non-JPEG (e.g. JXR), flip its
//! `format`/`mime` to JPEG.
//!
//! Loc-0 covers: some KFX declare no `cover_image` at all — they merely open on
//! a full-page cover image, which the loader infers from the first section. For
//! those, swapping the pixels isn't enough: the home tile / sleep screen resolve
//! their art through `cover_image` metadata, which is missing. So when the cover
//! wasn't declared, we also backfill the pointer into both metadata shapes
//! (`book_metadata`/$490 `kindle_title_metadata` and the flat `metadata`/$258
//! `$424`), keyed to the same resource — leaving the book declaring its cover
//! exactly like any other. Books that already declare `cover_image` are left
//! untouched.

use crate::formats::kfx::container::{self, get_field, symbol_id_for_name};
use crate::formats::kfx::container_edit::{EntityEdit, edit_container};
use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::ion::{IonParser, IonValue};
use crate::formats::kfx::loader;
use crate::formats::kfx::symbols::KfxSymbol;
use crate::image::jpeg::sanitize_for_kfx;

/// Replace the cover image inside `kfx_bytes` with `new_image`, returning the
/// rewritten container. `new_image` may be JPEG, PNG, or WebP; it's normalized
/// to a sleep-screen-safe JFIF JPEG before embedding.
///
/// Errors (via [`KfxError::InvalidKfx`]) if the KFX declares no
/// `cover_image`, if the declared cover can't be matched to a backing
/// `bcRawMedia`, or if the replacement image's dimensions can't be read.
pub fn replace_cover(kfx_bytes: &[u8], new_image: &[u8]) -> Result<Vec<u8>, KfxError> {
    // 1. Resolve cover identity via the proven loader: correct dynamic
    //    base_len, cover_image extraction, and bcRawMedia keying all in one.
    let book = loader::load(kfx_bytes)?;
    let cover_name = book
        .metadata
        .cover_resource_name
        .clone()
        .ok_or_else(|| KfxError::InvalidKfx("KFX declares no cover_image".into()))?;

    // The cover external_resource → its `location` (the bcRawMedia key) and its
    // current `format`. `by_type[164]` is keyed by resolved fid; match on the
    // `resource_name` field so we don't assume fid == resource_name.
    let resources = book
        .by_type
        .get(&(KfxSymbol::ExternalResource as u64))
        .ok_or_else(|| KfxError::InvalidKfx("KFX has no external_resource entities".into()))?;
    let mut cover_location: Option<String> = None;
    let mut original_format: Option<String> = None;
    // The `resource_name` field's symbol id, for keying a `$258` cover_image
    // pointer (see the loc-0 metadata backfill below).
    let mut cover_name_sym: Option<u64> = None;
    for v in resources.values() {
        let Some(fields) = v.unwrap_annotated().as_struct() else {
            continue;
        };
        let rn_field = get_field(fields, KfxSymbol::ResourceName as u64);
        let rn = rn_field.and_then(|x| book.symbols.text_of(x));
        if rn != Some(cover_name.as_str()) {
            continue;
        }
        if let Some(IonValue::Symbol(id)) = rn_field {
            cover_name_sym = Some(*id);
        }
        cover_location = get_field(fields, KfxSymbol::Location as u64)
            .and_then(|x| x.as_string())
            .map(str::to_string);
        original_format = get_field(fields, KfxSymbol::Format as u64)
            .and_then(|x| book.symbols.text_of(x))
            .map(str::to_string);
        break;
    }
    let cover_location = cover_location.ok_or_else(|| {
        KfxError::InvalidKfx(format!(
            "cover_image {cover_name:?} matches no external_resource location"
        ))
    })?;

    // 2. Normalize the replacement for KFX / the sleep-screen decoder, then read
    //    its true pixel dimensions. `sanitize_for_kfx` returns None for an
    //    already-clean JPEG (use as-is) and Some(jpeg) for stripped/transcoded.
    let clean = sanitize_for_kfx(new_image).unwrap_or_else(|| new_image.to_vec());
    let (new_w, new_h) = jpeg_dimensions(&clean).ok_or_else(|| {
        KfxError::InvalidKfx("could not read dimensions of replacement cover".into())
    })?;
    let flip_format = original_format.as_deref() != Some("jpg");

    // A cover resolved by the loader but *not* declared by `cover_image`
    // metadata is a "loc-0" cover: the book opens on a full-page cover image and
    // the loader inferred it from the first section. Such a KFX has color pixels
    // once we swap them, but the Kindle home tile and sleep screen resolve their
    // art through `cover_image` metadata — which is absent — so they'd still show
    // nothing. Backfill the pointer (into both the `$490` and `$258` shapes, as
    // Amazon/calibre do) so the book declares its cover exactly like every other.
    let backfill_cover_meta = !metadata_declares_cover(&book);

    // 3. Rewrite the container through the shared edit harness: swap the two
    //    cover entities (and, for a loc-0 cover, backfill the metadata pointer);
    //    the harness passes every other entity through byte-for-byte and
    //    recomputes the index-table offsets + payload sha1.
    let jpg_sym = symbol_id_for_name("jpg").unwrap_or(285);
    let mut swapped_media = false;
    let out = edit_container(kfx_bytes, |e| {
        let edit = if e.is_type(KfxSymbol::Bcrawmedia)
            && book.symbols.resolve(e.id() as u64) == cover_location
        {
            swapped_media = true;
            EntityEdit::RawMedia(clean.clone())
        } else if e.is_type(KfxSymbol::ExternalResource)
            && external_resource_location(e.raw()).as_deref() == Some(cover_location.as_str())
        {
            EntityEdit::Ion(rebuild_external_resource(
                &e.parse_ion()?,
                new_w,
                new_h,
                flip_format,
                jpg_sym,
            ))
        } else if backfill_cover_meta && e.is_type(KfxSymbol::BookMetadata) {
            EntityEdit::Ion(add_cover_image_to_book_metadata(
                &e.parse_ion()?,
                &cover_name,
                &book.symbols,
            ))
        } else if backfill_cover_meta
            && e.is_type(KfxSymbol::Metadata)
            && let Some(sym) = cover_name_sym
        {
            EntityEdit::Ion(add_cover_image_to_flat_metadata(&e.parse_ion()?, sym))
        } else {
            EntityEdit::Keep
        };
        Ok(edit)
    })?;

    if !swapped_media {
        // The declared cover has no backing `bcRawMedia` — the whole container
        // is image-less (its image data lives in a companion resource container
        // that was never imported). There's nothing to swap; surface it so the
        // caller can skip rather than emit a cover-onto-an-image-less-book.
        return Err(KfxError::InvalidKfx(format!(
            "cover bcRawMedia for {cover_location:?} not found (image-less container)"
        )));
    }

    Ok(out)
}

/// Read the `location` string off an ENTY-wrapped `external_resource` entity,
/// without resolving any symbols (location is a plain Ion string).
fn external_resource_location(raw_entity: &[u8]) -> Option<String> {
    let payload = container::skip_enty_header(raw_entity);
    let value = IonParser::new(payload).parse().ok()?;
    let fields = value.unwrap_annotated().as_struct()?;
    get_field(fields, KfxSymbol::Location as u64)
        .and_then(IonValue::as_string)
        .map(str::to_string)
}

/// True if the book already declares `cover_image` in either metadata shape:
/// the `$258` (`$424`) field or a `kindle_title_metadata` `cover_image` key
/// inside `book_metadata` ($490). When false, the cover was inferred from the
/// first section and needs a pointer backfilled (see `replace_cover`).
fn metadata_declares_cover(book: &loader::BookData) -> bool {
    if let Some(m) = book
        .by_type
        .get(&(KfxSymbol::Metadata as u64))
        .and_then(|m| m.values().next())
        .and_then(|r| r.unwrap_annotated().as_struct())
        && get_field(m, KfxSymbol::CoverImage as u64).is_some()
    {
        return true;
    }
    let Some(cats) = book
        .by_type
        .get(&(KfxSymbol::BookMetadata as u64))
        .and_then(|m| m.values().next())
        .and_then(|r| r.unwrap_annotated().as_struct())
        .and_then(|f| get_field(f, KfxSymbol::CategorisedMetadata as u64))
        .and_then(IonValue::as_list)
    else {
        return false;
    };
    cats.iter().any(|cat| {
        let Some(cf) = cat.unwrap_annotated().as_struct() else {
            return false;
        };
        get_field(cf, KfxSymbol::Category as u64).and_then(|c| book.symbols.text_of(c))
            == Some("kindle_title_metadata")
            && get_field(cf, KfxSymbol::Metadata as u64)
                .and_then(IonValue::as_list)
                .is_some_and(|items| {
                    items.iter().any(|it| {
                        it.as_struct().and_then(|itf| {
                            get_field(itf, KfxSymbol::Key as u64).and_then(IonValue::as_string)
                        }) == Some("cover_image")
                    })
                })
    })
}

/// Add a `cover_image` → `cover_name` entry to `book_metadata`'s ($490)
/// `kindle_title_metadata` category (value is the resource name as a string,
/// matching Amazon's `$490` shape). Preserves annotations and field order.
fn add_cover_image_to_book_metadata(
    parsed: &IonValue,
    cover_name: &str,
    symbols: &loader::SymbolTable,
) -> IonValue {
    if let IonValue::Annotated(anns, inner) = parsed {
        return IonValue::Annotated(
            anns.clone(),
            Box::new(add_cover_image_to_book_metadata(inner, cover_name, symbols)),
        );
    }
    let Some(fields) = parsed.as_struct() else {
        return parsed.clone();
    };
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len());
    for (k, v) in fields {
        if *k == KfxSymbol::CategorisedMetadata as u64
            && let IonValue::List(cats) = v
        {
            let new_cats = cats
                .iter()
                .map(|cat| append_cover_to_title_metadata(cat, cover_name, symbols))
                .collect();
            out.push((*k, IonValue::List(new_cats)));
        } else {
            out.push((*k, v.clone()));
        }
    }
    IonValue::Struct(out)
}

/// If `cat` is the `kindle_title_metadata` category, append a
/// `{key: "cover_image", value: cover_name}` item to its metadata list.
fn append_cover_to_title_metadata(
    cat: &IonValue,
    cover_name: &str,
    symbols: &loader::SymbolTable,
) -> IonValue {
    let Some(fields) = cat.unwrap_annotated().as_struct() else {
        return cat.clone();
    };
    let is_title = get_field(fields, KfxSymbol::Category as u64).and_then(|c| symbols.text_of(c))
        == Some("kindle_title_metadata");
    if !is_title {
        return cat.clone();
    }
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len());
    for (k, v) in fields {
        if *k == KfxSymbol::Metadata as u64
            && let IonValue::List(items) = v
        {
            let mut items = items.clone();
            items.push(IonValue::Struct(vec![
                (
                    KfxSymbol::Key as u64,
                    IonValue::String("cover_image".into()),
                ),
                (KfxSymbol::Value as u64, IonValue::String(cover_name.into())),
            ]));
            out.push((*k, IonValue::List(items)));
        } else {
            out.push((*k, v.clone()));
        }
    }
    // Categories are plain structs in practice, but re-wrap if annotated.
    match cat {
        IonValue::Annotated(anns, _) => {
            IonValue::Annotated(anns.clone(), Box::new(IonValue::Struct(out)))
        }
        _ => IonValue::Struct(out),
    }
}

/// Add `cover_image` ($424) → `Symbol(cover_name_sym)` to the flat `metadata`
/// ($258) fragment, mirroring the `$490` backfill in the older shape. No-op if
/// the fragment already carries it. Preserves annotations and field order.
fn add_cover_image_to_flat_metadata(parsed: &IonValue, cover_name_sym: u64) -> IonValue {
    if let IonValue::Annotated(anns, inner) = parsed {
        return IonValue::Annotated(
            anns.clone(),
            Box::new(add_cover_image_to_flat_metadata(inner, cover_name_sym)),
        );
    }
    let Some(fields) = parsed.as_struct() else {
        return parsed.clone();
    };
    let mut out = fields.to_vec();
    if get_field(fields, KfxSymbol::CoverImage as u64).is_none() {
        out.push((
            KfxSymbol::CoverImage as u64,
            IonValue::Symbol(cover_name_sym),
        ));
    }
    IonValue::Struct(out)
}

/// Rebuild an `external_resource` Ion value with updated dimensions (and,
/// when `flip_format`, JPEG `format`/`mime`). Preserves the annotation wrapper
/// and original field order; appends `resource_width`/`resource_height` if the
/// source lacked them.
fn rebuild_external_resource(
    parsed: &IonValue,
    w: u32,
    h: u32,
    flip_format: bool,
    jpg_sym: u64,
) -> IonValue {
    if let IonValue::Annotated(anns, inner) = parsed {
        return IonValue::Annotated(
            anns.clone(),
            Box::new(rebuild_external_resource(inner, w, h, flip_format, jpg_sym)),
        );
    }
    let Some(fields) = parsed.as_struct() else {
        return parsed.clone();
    };
    let mut out: Vec<(u64, IonValue)> = Vec::with_capacity(fields.len() + 2);
    let (mut saw_w, mut saw_h) = (false, false);
    for (k, val) in fields {
        let k = *k;
        if k == KfxSymbol::ResourceWidth as u64 {
            out.push((k, IonValue::Int(w as i64)));
            saw_w = true;
        } else if k == KfxSymbol::ResourceHeight as u64 {
            out.push((k, IonValue::Int(h as i64)));
            saw_h = true;
        } else if flip_format && k == KfxSymbol::Format as u64 {
            out.push((k, IonValue::Symbol(jpg_sym)));
        } else if flip_format && k == KfxSymbol::Mime as u64 {
            out.push((k, IonValue::String("image/jpeg".into())));
        } else {
            out.push((k, val.clone()));
        }
    }
    if !saw_w {
        out.push((KfxSymbol::ResourceWidth as u64, IonValue::Int(w as i64)));
    }
    if !saw_h {
        out.push((KfxSymbol::ResourceHeight as u64, IonValue::Int(h as i64)));
    }
    IonValue::Struct(out)
}

/// Read `(width, height)` from a JPEG's first SOF marker. Dependency-free
/// (the `image` crate is built without the jpeg feature), and we only ever
/// feed this JFIF JPEGs produced by `sanitize_for_kfx`.
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 2 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        // Skip any 0xFF fill bytes preceding the marker.
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        let marker = *data.get(i)?;
        i += 1;
        // Standalone markers carry no length payload: RSTn (D0–D7), SOI (D8),
        // EOI (D9), TEM (01).
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            continue;
        }
        let seg_len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
        // SOF markers C0–CF, excluding DHT (C4), JPG (C8), DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            // Segment payload after the 2 length bytes: precision(1),
            // height(2, BE), width(2, BE).
            let height = u16::from_be_bytes([*data.get(i + 3)?, *data.get(i + 4)?]) as u32;
            let width = u16::from_be_bytes([*data.get(i + 5)?, *data.get(i + 6)?]) as u32;
            return Some((width, height));
        }
        if seg_len < 2 {
            return None;
        }
        i += seg_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_dimensions_reads_sof0() {
        // SOI + SOF0(len 0x11) precision=8 height=0x0190(400) width=0x012C(300).
        let jpeg = [
            0xFF, 0xD8, // SOI
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x90, 0x01, 0x2C, // SOF0
            0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((300, 400)));
    }

    #[test]
    fn jpeg_dimensions_skips_app0_before_sof() {
        // SOI + APP0(JFIF, len 0x10) + SOF0 with 2×3 dims.
        let jpeg = [
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0,
            0, // APP0
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x03, 0x00, 0x02, 0x01, 0x01, 0x11,
            0x00, // SOF0 3h×2w
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((2, 3)));
    }

    #[test]
    fn jpeg_dimensions_rejects_non_jpeg() {
        assert_eq!(jpeg_dimensions(&[0x89, 0x50, 0x4E, 0x47]), None);
        assert_eq!(jpeg_dimensions(&[]), None);
    }

    #[test]
    fn rebuild_updates_dims_and_flips_format() {
        // external_resource { format: <jxr sym>, mime: "image/jxr",
        //   location: "resource/r", resource_width: 50, resource_height: 53 }
        let jxr_sym = symbol_id_for_name("jxr").unwrap_or(999);
        let original = IonValue::Struct(vec![
            (KfxSymbol::Format as u64, IonValue::Symbol(jxr_sym)),
            (KfxSymbol::Mime as u64, IonValue::String("image/jxr".into())),
            (
                KfxSymbol::Location as u64,
                IonValue::String("resource/r".into()),
            ),
            (KfxSymbol::ResourceWidth as u64, IonValue::Int(50)),
            (KfxSymbol::ResourceHeight as u64, IonValue::Int(53)),
        ]);
        let jpg_sym = symbol_id_for_name("jpg").unwrap_or(285);
        let out = rebuild_external_resource(&original, 300, 400, true, jpg_sym);
        let fields = out.as_struct().unwrap();
        let int = |k: KfxSymbol| get_field(fields, k as u64).and_then(IonValue::as_int);
        let string = |k: KfxSymbol| {
            get_field(fields, k as u64)
                .and_then(IonValue::as_string)
                .map(str::to_string)
        };
        assert_eq!(int(KfxSymbol::ResourceWidth), Some(300));
        assert_eq!(int(KfxSymbol::ResourceHeight), Some(400));
        match get_field(fields, KfxSymbol::Format as u64) {
            Some(IonValue::Symbol(id)) => assert_eq!(*id, jpg_sym),
            other => panic!("format not flipped to jpg symbol: {other:?}"),
        }
        assert_eq!(string(KfxSymbol::Mime).as_deref(), Some("image/jpeg"));
        // location preserved untouched.
        assert_eq!(string(KfxSymbol::Location).as_deref(), Some("resource/r"));
    }

    #[test]
    fn rebuild_keeps_jpg_format_when_not_flipping() {
        let jpg_sym = symbol_id_for_name("jpg").unwrap_or(285);
        let original = IonValue::Struct(vec![
            (KfxSymbol::Format as u64, IonValue::Symbol(jpg_sym)),
            (KfxSymbol::Mime as u64, IonValue::String("image/jpg".into())),
            (KfxSymbol::ResourceWidth as u64, IonValue::Int(1200)),
            (KfxSymbol::ResourceHeight as u64, IonValue::Int(1600)),
        ]);
        let out = rebuild_external_resource(&original, 355, 500, false, jpg_sym);
        let fields = out.as_struct().unwrap();
        let int = |k: KfxSymbol| get_field(fields, k as u64).and_then(IonValue::as_int);
        assert_eq!(int(KfxSymbol::ResourceWidth), Some(355));
        assert_eq!(int(KfxSymbol::ResourceHeight), Some(500));
        // mime left exactly as the source had it (not normalized) when no flip.
        assert_eq!(
            get_field(fields, KfxSymbol::Mime as u64).and_then(IonValue::as_string),
            Some("image/jpg")
        );
    }

    #[test]
    fn rebuild_preserves_annotation() {
        let inner = IonValue::Struct(vec![(KfxSymbol::ResourceWidth as u64, IonValue::Int(1))]);
        let annotated = IonValue::Annotated(vec![164], Box::new(inner));
        let out = rebuild_external_resource(&annotated, 9, 9, false, 285);
        match out {
            IonValue::Annotated(anns, _) => assert_eq!(anns, vec![164]),
            _ => panic!("annotation dropped"),
        }
    }

    /// The tiniest valid PNG (1×1). `sanitize_for_kfx` transcodes it to JPEG,
    /// so `replace_cover` exercises the PNG→JPEG path too.
    const MINIMAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// Find the declared cover's `(location, width, height, bytes)` in a loaded
    /// container — mirrors how `replace_cover` resolves it.
    fn declared_cover(book: &loader::BookData) -> Option<(String, i64, i64, Vec<u8>)> {
        let cover_name = book.metadata.cover_resource_name.clone()?;
        let resources = book.by_type.get(&(KfxSymbol::ExternalResource as u64))?;
        for v in resources.values() {
            let fields = v.unwrap_annotated().as_struct()?;
            let rn = get_field(fields, KfxSymbol::ResourceName as u64)
                .and_then(|x| book.symbols.text_of(x));
            if rn != Some(cover_name.as_str()) {
                continue;
            }
            let loc = get_field(fields, KfxSymbol::Location as u64)
                .and_then(IonValue::as_string)?
                .to_string();
            let w = get_field(fields, KfxSymbol::ResourceWidth as u64)
                .and_then(IonValue::as_int)
                .unwrap_or(-1);
            let h = get_field(fields, KfxSymbol::ResourceHeight as u64)
                .and_then(IonValue::as_int)
                .unwrap_or(-1);
            let bytes = book.raw_media.get(&loc)?.clone();
            return Some((loc, w, h, bytes));
        }
        None
    }

    /// End-to-end on the committed `tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx` (cover at
    /// `resource/rsrc7`): swap the cover, then prove the patched container
    /// re-parses with the new cover and still converts to EPUB.
    #[test]
    fn replace_cover_roundtrip_on_fixture() {
        let kfx = std::fs::read("tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx")
            .expect("read fixture");

        let before = loader::load(&kfx).expect("load original");
        let (loc0, _, _, bytes0) = declared_cover(&before).expect("fixture has a cover");

        let patched = replace_cover(&kfx, MINIMAL_PNG).expect("replace_cover");

        let after = loader::load(&patched).expect("patched container must re-parse");
        let (loc1, w1, h1, bytes1) = declared_cover(&after).expect("patched has a cover");

        // Cover location + name preserved; only the backing bytes changed.
        assert_eq!(loc0, loc1, "cover location must be stable");
        assert_eq!(
            before.metadata.cover_resource_name, after.metadata.cover_resource_name,
            "cover_resource_name preserved"
        );
        assert_ne!(bytes0, bytes1, "cover bytes must change");
        assert_eq!(&bytes1[..3], &[0xFF, 0xD8, 0xFF], "new cover is JPEG");
        assert_eq!(
            (w1, h1),
            (1, 1),
            "declared dims updated to the 1×1 replacement"
        );
        assert_eq!(
            before.raw_media.len(),
            after.raw_media.len(),
            "no resources added or dropped"
        );

        // The patched container is still a usable KFX.
        assert!(
            crate::kfx_to_epub::convert_to_epub(&patched).is_ok(),
            "patched KFX must still convert to EPUB"
        );
    }

    #[test]
    fn replace_cover_errors_without_cover_metadata() {
        // A non-KFX byte string can't load; ensure we surface an error rather
        // than panic. (Covers the early loader::load failure path.)
        assert!(replace_cover(b"not a kfx container", MINIMAL_PNG).is_err());
    }

    #[test]
    fn flat_metadata_backfills_cover_image_symbol_once() {
        let frag = IonValue::Struct(vec![(KfxSymbol::ReadingOrders as u64, IonValue::Int(0))]);
        let out = add_cover_image_to_flat_metadata(&frag, 953);
        match get_field(out.as_struct().unwrap(), KfxSymbol::CoverImage as u64) {
            Some(IonValue::Symbol(id)) => assert_eq!(*id, 953),
            other => panic!("cover_image not added as symbol: {other:?}"),
        }
        // Idempotent: a second pass doesn't duplicate the field.
        let again = add_cover_image_to_flat_metadata(&out, 953);
        let dupes = again
            .as_struct()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == KfxSymbol::CoverImage as u64)
            .count();
        assert_eq!(dupes, 1);
    }

    #[test]
    fn book_metadata_backfills_cover_image_into_title_category() {
        // book_metadata { categorised_metadata: [ {category:"kindle_title_metadata",
        //   metadata:[{key:"title", value:"T"}]}, {category:"kindle_audit_metadata", ...} ] }
        let title_cat = IonValue::Struct(vec![
            (
                KfxSymbol::Category as u64,
                IonValue::String("kindle_title_metadata".into()),
            ),
            (
                KfxSymbol::Metadata as u64,
                IonValue::List(vec![IonValue::Struct(vec![
                    (KfxSymbol::Key as u64, IonValue::String("title".into())),
                    (KfxSymbol::Value as u64, IonValue::String("T".into())),
                ])]),
            ),
        ]);
        let audit_cat = IonValue::Struct(vec![(
            KfxSymbol::Category as u64,
            IonValue::String("kindle_audit_metadata".into()),
        )]);
        let bm = IonValue::Struct(vec![(
            KfxSymbol::CategorisedMetadata as u64,
            IonValue::List(vec![title_cat, audit_cat]),
        )]);

        let book = crate::formats::kfx::loader::empty_book_for_test();
        let out = add_cover_image_to_book_metadata(&bm, "e6", &book.symbols);

        // The title category's metadata list gained a cover_image=e6 item; the
        // audit category is untouched.
        let cats = get_field(
            out.as_struct().unwrap(),
            KfxSymbol::CategorisedMetadata as u64,
        )
        .and_then(IonValue::as_list)
        .unwrap();
        let title = cats[0].as_struct().unwrap();
        let items = get_field(title, KfxSymbol::Metadata as u64)
            .and_then(IonValue::as_list)
            .unwrap();
        let cover = items.iter().find(|it| {
            it.as_struct()
                .and_then(|f| get_field(f, KfxSymbol::Key as u64).and_then(IonValue::as_string))
                == Some("cover_image")
        });
        let cover = cover
            .expect("cover_image item appended")
            .as_struct()
            .unwrap();
        assert_eq!(
            get_field(cover, KfxSymbol::Value as u64).and_then(IonValue::as_string),
            Some("e6")
        );
        assert_eq!(
            cats[1].as_struct().unwrap().len(),
            1,
            "audit category untouched"
        );
    }
}
