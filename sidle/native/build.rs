//! `SIDLE_BUILD_TS` (unix seconds) baked in as a `rustc-env`.
//!
//! `selfupdate::decide` compares it against a manifest `built_at` and refuses
//! anything not strictly newer. `build.sh` exports it for the cross-compile and
//! writes the matching `sidle.build-ts` sidecar the manifest carries.
//!
//! A bare `cargo build` bakes `0`, leaving `decide` on its sha-only branch.

fn main() {
    println!("cargo:rerun-if-env-changed=SIDLE_BUILD_TS");
    let ts = std::env::var("SIDLE_BUILD_TS").unwrap_or_else(|_| "0".to_string());
    println!("cargo:rustc-env=SIDLE_BUILD_TS={ts}");
}
