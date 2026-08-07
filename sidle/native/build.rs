//! Bake the build timestamp into the picker so a running binary knows its own
//! age. The LAN self-update (`selfupdate.rs`) compares this against the
//! manifest's `built_at` and refuses anything not strictly newer — so a stale
//! `device-dist` can no longer downgrade the device over Wi-Fi.
//!
//! `build.sh` exports `SIDLE_BUILD_TS` (unix seconds) for the cross-compile and
//! writes a matching `sidle.build-ts` sidecar that the server reads into the
//! manifest, so both sides share one clock. A bare `cargo build` (no env) bakes
//! `0` = "unknown", which disables the guard for that binary (the device falls
//! back to the sha-only check).

fn main() {
    println!("cargo:rerun-if-env-changed=SIDLE_BUILD_TS");
    let ts = std::env::var("SIDLE_BUILD_TS").unwrap_or_else(|_| "0".to_string());
    println!("cargo:rustc-env=SIDLE_BUILD_TS={ts}");
}
