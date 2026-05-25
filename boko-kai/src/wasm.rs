//! WASM bindings for browser-based ebook conversion.
//!
//! Exposes the conversions sidle supports to JavaScript via wasm-bindgen:
//! EPUB → KFX, and AZW3 / KFX / MOBI → EPUB. (boko-kai only writes EPUB and
//! KFX, so AZW3 / MOBI / Markdown output bindings were dropped.)

use std::io::Cursor;
use wasm_bindgen::prelude::*;

use crate::model::{Book, Format};

/// Initialize panic hook for better error messages in the browser console.
#[wasm_bindgen(start)]
pub fn init() {
    #[cfg(feature = "wasm")]
    console_error_panic_hook::set_once();
}

/// Convert EPUB to KFX (Kindle Format 10).
///
/// Takes raw EPUB bytes and returns KFX bytes.
#[wasm_bindgen]
pub fn epub_to_kfx(data: &[u8]) -> Result<Vec<u8>, JsValue> {
    let mut book =
        Book::from_bytes(data, Format::Epub).map_err(|e| JsValue::from_str(&e.to_string()))?;

    let mut output = Cursor::new(Vec::new());
    book.export(Format::Kfx, &mut output)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(output.into_inner())
}

/// Convert AZW3 to EPUB.
///
/// Takes raw AZW3 bytes and returns EPUB bytes.
#[wasm_bindgen]
pub fn azw3_to_epub(data: &[u8]) -> Result<Vec<u8>, JsValue> {
    let mut book =
        Book::from_bytes(data, Format::Azw3).map_err(|e| JsValue::from_str(&e.to_string()))?;

    let mut output = Cursor::new(Vec::new());
    book.export(Format::Epub, &mut output)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(output.into_inner())
}

/// Convert KFX to EPUB.
///
/// Takes raw KFX bytes and returns EPUB bytes. Uses the dedicated `kfx_to_epub`
/// mechanical port (same entry as the CLI) — **not** the generic
/// `KfxImporter → IR → EpubExporter` path. The IR path drops KFX-specific
/// output, notably the synthesized `writing-mode` CSS for vertical (CJK) books.
#[wasm_bindgen]
pub fn kfx_to_epub(data: &[u8]) -> Result<Vec<u8>, JsValue> {
    crate::kfx_to_epub::convert_to_epub(data).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Convert MOBI to EPUB.
///
/// Takes raw MOBI bytes and returns EPUB bytes.
/// Handles both legacy MOBI and modern AZW3 (KF8) formats.
#[wasm_bindgen]
pub fn mobi_to_epub(data: &[u8]) -> Result<Vec<u8>, JsValue> {
    let mut book =
        Book::from_bytes(data, Format::Mobi).map_err(|e| JsValue::from_str(&e.to_string()))?;

    let mut output = Cursor::new(Vec::new());
    book.export(Format::Epub, &mut output)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(output.into_inner())
}

/// Merge a `.kfx-zip` bundle (Amazon's multi-container KFX) into a single flat
/// `.kfx` for sideloading. Uses the thread-free mechanical merge — the dedicated
/// `kfx::merge` path, not the IR.
#[wasm_bindgen]
pub fn kfx_zip_to_kfx(data: &[u8]) -> Result<Vec<u8>, JsValue> {
    crate::kfx::merge::merge_kfx_zip_bytes(data).map_err(|e| JsValue::from_str(&e.to_string()))
}
