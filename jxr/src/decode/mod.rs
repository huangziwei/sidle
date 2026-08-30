//! JPEG-XR decoder: TIFF-like container parsing + full T.832 codestream
//! reconstruction. The codestream side follows the ITU-T T.832 pseudo-code
//! and covers the whole specification, past the subset a Kindle emits.

pub mod consts;
pub mod container;
pub mod decoder;
pub(crate) mod math;
pub(crate) mod misc;
pub mod pixels;
pub(crate) mod state;
pub(crate) mod tables;

/// Decode a parsed container completely: `c.image_data`, plus `c.alpha_data`
/// where the container carries one — alpha as its own YONLY image, merged as
/// the final component.
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

/// Apply the container's SPATIAL_XFRM_PRIMARY to `img` in place. `orientation`
/// 0–7: 0 none, 1 flip vertical, 2 flip horizontal, 3 both (180°), 4 rotate
/// 90° CW, 5–7 that rotation followed by the flips of 1–3.
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
