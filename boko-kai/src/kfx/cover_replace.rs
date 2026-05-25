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
//! same resolver [`kfx_to_epub::loader`] uses (so the *dynamic* doc-symbol
//! `base_len` is correct — a fixed `KFX_SYMBOL_TABLE.len()` mis-resolves), then
//! emit a fresh container in which only those two entities change and every
//! other entity is copied through verbatim. [`serialize_container`] recomputes
//! the index-table offsets and `kfxgen_payload_sha1`.
//!
//! The replacement is normalized through [`sanitize_for_kfx`] first: it strips
//! EXIF (the KOA2 sleep-screen decoder rejects EXIF-tagged JPEGs and falls back
//! to the auto title-card) and transcodes PNG/WebP → JFIF JPEG. We then update
//! the resource's declared `resource_width`/`resource_height` to the real pixel
//! dimensions and, if the slot was previously non-JPEG (e.g. JXR), flip its
//! `format`/`mime` to JPEG.

use crate::kfx::container::{
    self, get_field, parse_container_header, parse_index_table, symbol_id_for_name,
};
use crate::kfx::image_transcode::sanitize_for_kfx;
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::serialization::{
    SerializedEntity, create_entity_data, create_raw_media_data, serialize_container,
};
use crate::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::ConvertError;
use crate::kfx_to_epub::loader;

/// Replace the cover image inside `kfx_bytes` with `new_image`, returning the
/// rewritten container. `new_image` may be JPEG, PNG, or WebP; it's normalized
/// to a sleep-screen-safe JFIF JPEG before embedding.
///
/// Errors (via [`ConvertError::InvalidKfx`]) if the KFX declares no
/// `cover_image`, if the declared cover can't be matched to a backing
/// `bcRawMedia`, or if the replacement image's dimensions can't be read.
pub fn replace_cover(kfx_bytes: &[u8], new_image: &[u8]) -> Result<Vec<u8>, ConvertError> {
    // 1. Resolve cover identity via the proven loader: correct dynamic
    //    base_len, cover_image extraction, and bcRawMedia keying all in one.
    let book = loader::load(kfx_bytes)?;
    let cover_name = book
        .metadata
        .cover_resource_name
        .clone()
        .ok_or_else(|| ConvertError::InvalidKfx("KFX declares no cover_image".into()))?;

    // The cover external_resource → its `location` (the bcRawMedia key) and its
    // current `format`. `by_type[164]` is keyed by resolved fid; match on the
    // `resource_name` field so we don't assume fid == resource_name.
    let resources = book
        .by_type
        .get(&(KfxSymbol::ExternalResource as u64))
        .ok_or_else(|| ConvertError::InvalidKfx("KFX has no external_resource entities".into()))?;
    let mut cover_location: Option<String> = None;
    let mut original_format: Option<String> = None;
    for v in resources.values() {
        let Some(fields) = v.unwrap_annotated().as_struct() else {
            continue;
        };
        let rn = get_field(fields, KfxSymbol::ResourceName as u64)
            .and_then(|x| book.symbols.text_of(x));
        if rn != Some(cover_name.as_str()) {
            continue;
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
        ConvertError::InvalidKfx(format!(
            "cover_image {cover_name:?} matches no external_resource location"
        ))
    })?;

    // 2. Normalize the replacement for KFX / the sleep-screen decoder, then read
    //    its true pixel dimensions. `sanitize_for_kfx` returns None for an
    //    already-clean JPEG (use as-is) and Some(jpeg) for stripped/transcoded.
    let clean = sanitize_for_kfx(new_image).unwrap_or_else(|| new_image.to_vec());
    let (new_w, new_h) = jpeg_dimensions(&clean).ok_or_else(|| {
        ConvertError::InvalidKfx("could not read dimensions of replacement cover".into())
    })?;
    let flip_format = original_format.as_deref() != Some("jpg");

    // 3. Re-parse the container for byte offsets and the header sections we pass
    //    through (doc symbols + format capabilities) and the container id.
    let header = parse_container_header(kfx_bytes)
        .map_err(|e| ConvertError::InvalidKfx(e.to_string()))?;
    let ci_end = header.container_info_offset + header.container_info_length;
    if ci_end > kfx_bytes.len() {
        return Err(ConvertError::InvalidKfx("container info out of bounds".into()));
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
    let idx_off = geti(KfxSymbol::Bcindextaboffset)
        .ok_or_else(|| ConvertError::InvalidKfx("no index table offset".into()))?;
    let idx_len = geti(KfxSymbol::Bcindextablength)
        .ok_or_else(|| ConvertError::InvalidKfx("no index table length".into()))?;
    let container_id = get_field(&ci_fields, KfxSymbol::Bccontid as u64)
        .and_then(IonValue::as_string)
        .unwrap_or("")
        .to_string();

    let slice_section = |off: Option<usize>, len: Option<usize>| -> &[u8] {
        match (off, len) {
            (Some(o), Some(l)) if o + l <= kfx_bytes.len() => &kfx_bytes[o..o + l],
            _ => &[],
        }
    };
    let symtab_ion = slice_section(
        geti(KfxSymbol::Bcdocsymboloffset),
        geti(KfxSymbol::Bcdocsymbollength),
    );
    let format_caps_ion = slice_section(
        geti(KfxSymbol::Bcfcapabilitiesoffset),
        geti(KfxSymbol::Bcfcapabilitieslength),
    );

    if idx_off + idx_len > kfx_bytes.len() {
        return Err(ConvertError::InvalidKfx("index table out of bounds".into()));
    }
    let entities = parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    // 4. Rebuild the entity list: swap the two cover entities, pass the rest
    //    through byte-for-byte (their ENTY-wrapped bytes are copied verbatim).
    let jpg_sym = symbol_id_for_name("jpg").unwrap_or(285);
    let mut out_entities: Vec<SerializedEntity> = Vec::with_capacity(entities.len());
    let mut swapped_media = false;
    for e in &entities {
        if e.offset + e.length > kfx_bytes.len() {
            return Err(ConvertError::InvalidKfx("entity payload out of bounds".into()));
        }
        let raw = &kfx_bytes[e.offset..e.offset + e.length];

        let data = if e.type_id == KfxSymbol::Bcrawmedia as u32
            && book.symbols.resolve(e.id as u64) == cover_location
        {
            swapped_media = true;
            create_raw_media_data(&clean)
        } else if e.type_id == KfxSymbol::ExternalResource as u32
            && external_resource_location(raw).as_deref() == Some(cover_location.as_str())
        {
            let payload = container::skip_enty_header(raw);
            let parsed = IonParser::new(payload)
                .parse()
                .map_err(|err| ConvertError::InvalidKfx(format!("parse external_resource: {err}")))?;
            let rebuilt = rebuild_external_resource(&parsed, new_w, new_h, flip_format, jpg_sym);
            create_entity_data(&rebuilt)
        } else {
            raw.to_vec()
        };

        out_entities.push(SerializedEntity {
            id: e.id,
            entity_type: e.type_id,
            data,
        });
    }

    if !swapped_media {
        return Err(ConvertError::InvalidKfx(format!(
            "cover bcRawMedia for {cover_location:?} not found"
        )));
    }

    Ok(serialize_container(
        &container_id,
        &out_entities,
        symtab_ion,
        format_caps_ion,
    ))
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
        if (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC
        {
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
            0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, // APP0
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x03, 0x00, 0x02, 0x01, 0x01, 0x11, 0x00, // SOF0 3h×2w
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
            (KfxSymbol::Location as u64, IonValue::String("resource/r".into())),
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

    /// End-to-end on the committed `tests/fixtures/epictetus.kfx` (cover at
    /// `resource/rsrc7`): swap the cover, then prove the patched container
    /// re-parses with the new cover and still converts to EPUB.
    #[test]
    fn replace_cover_roundtrip_on_fixture() {
        let kfx = std::fs::read("tests/fixtures/epictetus.kfx").expect("read fixture");

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
        assert_eq!((w1, h1), (1, 1), "declared dims updated to the 1×1 replacement");
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
}
