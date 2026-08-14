use std::hash::{Hash, Hasher};
use std::path::Path;

fn main() {
    // `generate_context!` embeds the frontend (`frontendDist = ../web`) into the
    // binary at compile time. Cargo doesn't reliably treat those files as inputs
    // to this crate, so a frontend-only edit could leave `cargo run` serving the
    // previously-embedded (stale) assets — which is exactly what bit us.
    //
    // Fingerprint the whole web tree into a rustc env var: when any file's bytes
    // change, the fingerprint changes, which is a crate compilation input, so the
    // crate recompiles and `generate_context!` re-reads + re-embeds. The
    // per-file rerun-if-changed makes this build script itself rerun to recompute
    // the fingerprint.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    fingerprint_dir(Path::new("../web"), &mut hasher);
    println!(
        "cargo:rustc-env=SIDLE_WEB_FINGERPRINT={:x}",
        hasher.finish()
    );

    tauri_build::build()
}

fn fingerprint_dir(dir: &Path, hasher: &mut impl Hasher) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort(); // stable order so the fingerprint is deterministic
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            fingerprint_dir(&path, hasher);
        } else if let Ok(bytes) = std::fs::read(&path) {
            path.to_string_lossy().hash(hasher);
            bytes.hash(hasher);
        }
    }
}
