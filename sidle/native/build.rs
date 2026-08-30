//! `SIDLE_BUILD_TS` (unix seconds) baked in as a `rustc-env`.

fn main() {
    println!("cargo:rerun-if-env-changed=SIDLE_BUILD_TS");
    let ts = std::env::var("SIDLE_BUILD_TS").unwrap_or_else(|_| "0".to_string());
    println!("cargo:rustc-env=SIDLE_BUILD_TS={ts}");
}
