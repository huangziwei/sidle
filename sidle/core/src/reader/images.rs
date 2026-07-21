//! On-demand image bytes for an open book.

use bokai::model::{Book, Format};

/// Produces a book's image bytes when the reader asks for them.
///
/// Holds the parsed book so a fetch is a decode of one image rather than a
/// re-parse of the container. Fetches are stateless — re-fetching an href just
/// decodes it again — so the reader can drop and re-request freely.
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

    /// One image's bytes. `None` for an href this book doesn't hold.
    pub fn fetch(&self, href: &str) -> Option<Vec<u8>> {
        let mut book = self.book.lock().ok()?;
        book.load_asset(std::path::Path::new(href)).ok()
    }

    /// Fetch several at once. The importer decodes across cores where the
    /// format makes that worthwhile (KFX transcodes JPEG-XR in parallel).
    /// Unknown hrefs are dropped from the result rather than failing the
    /// batch.
    pub fn fetch_many(&self, hrefs: &[String]) -> Vec<(String, Vec<u8>)> {
        let Ok(mut book) = self.book.lock() else {
            return Vec::new();
        };
        let paths: Vec<std::path::PathBuf> = hrefs.iter().map(std::path::PathBuf::from).collect();
        book.load_assets(&paths)
            .into_iter()
            .zip(hrefs)
            .filter_map(|(bytes, href)| Some((href.clone(), bytes.ok()?)))
            .collect()
    }
}
