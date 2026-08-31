//! A book's resources. [`BookResources::declared`] reads every size from the
//! manifest; [`BookResources::load_named`] decodes the pixels a page needs.

use std::collections::HashMap;
use std::sync::Arc;

use bokai::model::{Book, Chapter, NodeId, Role};

use crate::geom::Size;
use crate::resource::{Bitmap, Resources};

#[derive(Default)]
pub struct BookResources {
    sizes: HashMap<String, Size>,
    bitmaps: HashMap<String, Arc<Bitmap>>,
}

impl BookResources {
    /// Every size the book declares. No image is read.
    pub fn declared(book: &mut Book) -> Self {
        let mut sizes = HashMap::new();
        for asset in book.asset_manifest().unwrap_or_default() {
            let (Some(width), Some(height)) = (asset.width, asset.height) else {
                continue;
            };
            let size = Size::new(width as f32, height as f32);
            let path = asset.path.to_string_lossy().into_owned();
            if let Some(name) = asset.path.file_name() {
                sizes.insert(name.to_string_lossy().into_owned(), size);
            }
            sizes.insert(path, size);
        }
        Self {
            sizes,
            bitmaps: HashMap::new(),
        }
    }

    /// Read and decode every image `chapter` refers to that `bitmaps` lacks.
    /// An image that fails to decode is skipped and its box draws empty.
    pub fn load(&mut self, book: &mut Book, chapter: &Chapter) {
        self.load_named(book, &referenced(chapter));
    }

    /// Read and decode each of `srcs` this holds no pixels for.
    pub fn load_named(&mut self, book: &mut Book, srcs: &[String]) {
        let wanted: Vec<String> = srcs
            .iter()
            .filter(|src| !self.bitmaps.contains_key(*src))
            .cloned()
            .collect();
        if wanted.is_empty() {
            return;
        }
        let paths: Vec<std::path::PathBuf> = wanted.iter().map(std::path::PathBuf::from).collect();
        for (src, stored) in wanted.into_iter().zip(book.load_assets_stored(&paths)) {
            let Ok((bytes, format)) = stored else {
                continue;
            };
            let Some(bitmap) = decode(&bytes, format.as_deref()) else {
                continue;
            };
            self.sizes.insert(src.clone(), bitmap.size());
            self.bitmaps.insert(src, Arc::new(bitmap));
        }
    }
}

impl Resources for BookResources {
    fn image_size(&self, src: &str) -> Option<Size> {
        self.sizes
            .get(src)
            .or_else(|| self.sizes.get(file_name(src)))
            .copied()
    }

    fn image_bitmap(&self, src: &str) -> Option<Arc<Bitmap>> {
        self.bitmaps
            .get(src)
            .or_else(|| self.bitmaps.get(file_name(src)))
            .cloned()
    }
}

/// Every `src` the chapter names, once each.
fn referenced(chapter: &Chapter) -> Vec<String> {
    let mut seen = Vec::new();
    for id in 0..chapter.node_count() {
        let node = NodeId(id as u32);
        if chapter.node(node).map(|n| n.role) != Some(Role::Image) {
            continue;
        }
        let Some(src) = chapter.semantics.src(node) else {
            continue;
        };
        if !seen.iter().any(|held| held == src) {
            seen.push(src.to_string());
        }
    }
    seen
}

/// The last path segment, which is how a document usually names an asset the
/// manifest lists under a directory.
fn file_name(src: &str) -> &str {
    src.rsplit('/').next().unwrap_or(src)
}

/// `bytes` as pixels. A JPEG-XR goes through [`jxr`]; everything else
/// through [`image`].
fn decode(bytes: &[u8], format: Option<&str>) -> Option<Bitmap> {
    if format == Some("jxr") || bytes.starts_with(&[0x49, 0x49, 0xBC]) {
        return decode_jxr(bytes);
    }
    let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
    Some(Bitmap {
        width: decoded.width(),
        height: decoded.height(),
        rgba: decoded.into_raw(),
    })
}

/// A JPEG-XR straight to RGBA, with no re-encode on the way.
fn decode_jxr(bytes: &[u8]) -> Option<Bitmap> {
    use jxr::decode::pixels::{ColorModel, SampleType};
    use jxr::decode::{container, decoder};

    let container = container::parse(bytes).ok()?;
    let decoded = decoder::Decoder::new(container.image_data).decode().ok()?;
    let buffer = decoded.to_pixel_buffer().ok()?;
    if buffer.sample != SampleType::U8 {
        return None;
    }
    let channels = buffer.channels as usize;
    let colour = match buffer.color {
        ColorModel::Gray | ColorModel::NChannel(1) => 1,
        ColorModel::Rgb => 3,
        ColorModel::NChannel(k) if k >= 3 => 3,
        _ => return None,
    };
    let count = buffer.width as usize * buffer.height as usize;
    let mut rgba = vec![0xffu8; count * 4];
    for (out, pixel) in rgba
        .chunks_exact_mut(4)
        .zip(buffer.data.chunks_exact(channels))
    {
        if colour == 1 {
            out[0] = pixel[0];
            out[1] = pixel[0];
            out[2] = pixel[0];
        } else {
            out[..3].copy_from_slice(&pixel[..3]);
        }
    }
    Some(Bitmap {
        width: buffer.width,
        height: buffer.height,
        rgba,
    })
}
