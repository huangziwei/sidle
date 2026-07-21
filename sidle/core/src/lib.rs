//! sidle-core — Tauri-independent crate shared by the desktop app
//! (`sidle/src-tauri`) and the LAN HTTP server (`sidle/server`, future).
//!
//! Owns the on-disk library: rusqlite-backed `library.db`, the per-book
//! `books/<sha>/` directory layout, and the import pipeline that lands new
//! EPUB/KFX/AZW3 files. Deliberately no Tauri / no axum / no async runtime
//! dependency — callers bring their own.

pub mod library;
pub mod reader;
