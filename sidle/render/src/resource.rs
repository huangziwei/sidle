//! What layout needs to know about a book's resources.
//!
//! A replaced element takes its size from the resource its `src` names, and
//! a [`bokai::model::Chapter`] carries no pixels. [`Resources`] answers for
//! them, over a chapter alone.

use std::collections::HashMap;
use std::sync::Arc;

use crate::geom::Size;

/// Decoded pixels, row-major, four bytes per pixel with straight (not
/// premultiplied) alpha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Bitmap {
    pub fn size(&self) -> Size {
        Size::new(self.width as f32, self.height as f32)
    }
}

/// The resources a chapter refers to.
pub trait Resources {
    /// The image's intrinsic size in CSS pixels, keyed by the `src` the
    /// document names. `None` where nothing is known about it.
    fn image_size(&self, src: &str) -> Option<Size>;

    /// The image's pixels, for painting. Decoding is the consumer's, which
    /// keeps image codecs out of the renderer and lets a caller cache,
    /// downsample or defer as it sees fit.
    fn image_bitmap(&self, _src: &str) -> Option<Arc<Bitmap>> {
        None
    }
}

/// A book whose resources are not available. Images fall back to the size
/// CSS gives an object of unknown proportions.
pub struct Unknown;

impl Resources for Unknown {
    fn image_size(&self, _src: &str) -> Option<Size> {
        None
    }
}

impl Resources for HashMap<String, Size> {
    fn image_size(&self, src: &str) -> Option<Size> {
        self.get(src).copied()
    }
}

impl Resources for HashMap<String, Arc<Bitmap>> {
    fn image_size(&self, src: &str) -> Option<Size> {
        self.get(src).map(|bitmap| bitmap.size())
    }

    fn image_bitmap(&self, src: &str) -> Option<Arc<Bitmap>> {
        self.get(src).cloned()
    }
}

impl<T: Resources + ?Sized> Resources for &T {
    fn image_size(&self, src: &str) -> Option<Size> {
        (**self).image_size(src)
    }
}
