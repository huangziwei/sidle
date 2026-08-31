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
        for (src, bytes) in wanted.into_iter().zip(book.load_assets(&paths)) {
            let Ok(bytes) = bytes else { continue };
            let Some(bitmap) = decode(&bytes) else {
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

fn decode(bytes: &[u8]) -> Option<Bitmap> {
    let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
    Some(Bitmap {
        width: decoded.width(),
        height: decoded.height(),
        rgba: decoded.into_raw(),
    })
}
