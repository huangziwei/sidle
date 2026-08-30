//! On-demand image bytes for an open book.

use bokai::model::{Book, Format};

/// One image, delivered.
pub struct FetchedImage {
    pub href: String,
    /// Media type of `bytes` as they actually are, sniffed on delivery rather
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Produces a book's image bytes when the reader asks for them.
pub struct ImageStore {
    book: std::sync::Mutex<Book>,
}

impl ImageStore {
    pub(crate) fn new(book: Book) -> Self {
        Self {
            book: std::sync::Mutex::new(book),
        }
    }

    /// Re-open a book for image serving alone, for a reader that kept the
    /// manifest but dropped the store.
    pub fn reopen(kfx: &[u8]) -> Result<Self, String> {
        let book =
            Book::from_bytes(kfx, Format::Kfx).map_err(|e| format!("could not read KFX: {e}"))?;
        Ok(Self::new(book))
    }

    /// One image. `None` for an href this book doesn't hold.
    pub fn fetch(&self, href: &str) -> Option<FetchedImage> {
        let mut book = self.book.lock().ok()?;
        let bytes = book.load_asset(std::path::Path::new(href)).ok()?;
        Some(delivered(href.to_string(), bytes))
    }

    /// Fetch several at once. The importer decodes across cores where the
    /// format makes that worthwhile (KFX transcodes JPEG-XR in parallel).
    pub fn fetch_many(&self, hrefs: &[String]) -> Vec<FetchedImage> {
        let Ok(mut book) = self.book.lock() else {
            return Vec::new();
        };
        let paths: Vec<std::path::PathBuf> = hrefs.iter().map(std::path::PathBuf::from).collect();
        book.load_assets(&paths)
            .into_iter()
            .zip(hrefs)
            .filter_map(|(bytes, href)| match bytes {
                Ok(bytes) => Some(delivered(href.clone(), bytes)),
                Err(e) => {
                    eprintln!("[reader] fetch {href}: {e}");
                    None
                }
            })
            .collect()
    }
}

fn delivered(href: String, bytes: Vec<u8>) -> FetchedImage {
    FetchedImage {
        mime: bokai::image::media_type_of(&bytes).to_string(),
        href,
        bytes,
    }
}
