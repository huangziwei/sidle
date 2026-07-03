//! JPEG-XR decoder: TIFF-like container parsing + full T.832 codestream
//! reconstruction. Line-by-line port of `jxr_image.py` (John Howell, KFX
//! Input plugin), itself written from the ITU-T T.832 pseudo-code — so the
//! codestream side covers the whole spec, not just the Kindle subset.
//!
//! Entry points: [`container::parse`] for the outer file, then
//! [`decoder::Decoder`] on the extracted codestream bytes.
//!
//! ## Layout
//!
//! Public modules are the decoded-artifact vocabulary: [`container`] (outer
//! file), [`decoder`] (codestream pipeline, [`decoder::DecodedImage`], and
//! the [`decoder::Decoder::parse_headers`] sniffing view), [`pixels`]
//! (interleaved pixel-buffer view) and [`consts`] (the T.832 constants the
//! raw `u8` fields are expressed in). The machinery — `misc` (bit reader),
//! `math` (transform primitives), `state` (plane/MB/VLC state), `tables`
//! (spec Huffman tables) — is crate-internal.

pub mod consts;
pub mod container;
pub mod decoder;
pub(crate) mod math;
pub(crate) mod misc;
pub mod pixels;
pub(crate) mod state;
pub(crate) mod tables;

/// Decode a parsed container completely: the primary codestream, plus the
/// separate planar-alpha codestream when the container carries one (the
/// `-a 2` encoding — alpha as its own YONLY image appended via
/// ALPHA_OFFSET/ALPHA_BYTE_COUNT), merged as the final component.
pub fn decode_image(
    c: &container::JxrContainer<'_>,
) -> Result<decoder::DecodedImage, decoder::DecodeError> {
    let mut img = decoder::Decoder::new(c.image_data).decode()?;
    if let Some(alpha_bytes) = c.alpha_data {
        let alpha = decoder::Decoder::new(alpha_bytes).decode()?;
        if (alpha.width, alpha.height) != (img.width, img.height) {
            return Err(decoder::DecodeError::Unsupported(format!(
                "alpha image {}x{} mismatches primary {}x{}",
                alpha.width, alpha.height, img.width, img.height
            )));
        }
        img.image_plane
            .extend(alpha.image_plane.into_iter().take(1));
        img.num_components += 1;
        img.has_alpha = true;
    }
    Ok(img)
}

/// Apply a presentation orientation (the container's SPATIAL_XFRM_PRIMARY,
/// also `JxrDecApp -O`) to a decoded image, in place. Values 0–7:
/// 0 none, 1 flip vertical, 2 flip horizontal, 3 both (180°),
/// 4 rotate 90° CW, 5–7 rotate 90° CW followed by the flips of 1–3.
/// Not applied automatically by [`decoder::Decoder`] (matching the libjxr
/// decoder, which only transforms on request).
pub fn apply_orientation(img: &mut decoder::DecodedImage, orientation: u8) {
    if orientation == 0 || orientation > 7 {
        return;
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let rotate = orientation >= 4;
    let (ow, oh) = if rotate { (h, w) } else { (w, h) };
    let flip_v = matches!(orientation, 1 | 3 | 5 | 7);
    let flip_h = matches!(orientation, 2 | 3 | 6 | 7);
    for plane in &mut img.image_plane {
        let mut out = vec![0i32; w * h];
        for dy in 0..oh {
            for dx in 0..ow {
                // Undo flips, then undo the rotation, to find the source.
                let (ux, uy) = (
                    if flip_h { ow - 1 - dx } else { dx },
                    if flip_v { oh - 1 - dy } else { dy },
                );
                let (sx, sy) = if rotate { (uy, h - 1 - ux) } else { (ux, uy) };
                out[dy * ow + dx] = plane[sy * w + sx];
            }
        }
        *plane = out;
    }
    img.width = ow as u32;
    img.height = oh as u32;
}
