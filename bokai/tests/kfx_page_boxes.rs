//! The pixel box a KFX section states for itself, read through the public API.

use std::path::PathBuf;

use bokai::Book;
use bokai::model::Format;

/// Every `.kfx` under `tests/fixtures`, in name order.
fn fixtures() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("tests/fixtures") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "kfx"))
        .collect();
    paths.sort();
    paths
}

/// A reflowable book's cover is a section authored to a pixel box — the
/// `fixed_width`/`fixed_height` its page template states, which the reader
/// scales to the screen. Every other section reflows into the reading area
/// and states none.
#[test]
fn only_the_cover_of_a_reflowable_kfx_states_a_page_box() {
    let mut read = 0;
    for path in fixtures() {
        let Ok(kfx) = std::fs::read(&path) else {
            continue;
        };
        let Ok(book) = Book::from_bytes(&kfx, Format::Kfx) else {
            continue;
        };
        let boxes: Vec<Option<(u32, u32)>> =
            book.spine().iter().map(|entry| entry.viewport).collect();
        if boxes.len() < 2 {
            continue;
        }
        read += 1;

        let cover = boxes.first().copied().flatten();
        assert!(
            cover.is_some_and(|(width, height)| width > 0 && height > 0),
            "{}: the cover states no page box",
            path.display()
        );
        assert!(
            boxes[1..].iter().all(Option::is_none),
            "{}: a section past the cover states a box: {boxes:?}",
            path.display()
        );
    }
    assert!(read > 0, "no reflowable KFX fixture was read");
}
