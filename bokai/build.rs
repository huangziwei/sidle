//! Fold a release stamp into what `bokai --version` prints.
//!
//! The crate version alone cannot identify a build on a device. bokai ships
//! attached to sidle releases and its own version stands still across most of
//! them, so two Kindles can hold different binaries that both answer `0.1.2`.
//! `BOKAI_BUILD` is the release tag the artifact was cut from — `build-bokai.sh`
//! passes it, and the release workflow passes the tag to `build-bokai.sh`.
//!
//! Cargo does not track what a build script reads from the environment, hence
//! the `rerun-if-env-changed`: without it the crate counts as fresh when only
//! the stamp moved, and the binary keeps whichever value was set the last time
//! its source changed.

fn main() {
    println!("cargo:rerun-if-env-changed=BOKAI_BUILD");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let stamped = match std::env::var("BOKAI_BUILD") {
        Ok(stamp) if !stamp.is_empty() && stamp != "dev" => format!("{version} ({stamp})"),
        _ => version,
    };
    println!("cargo:rustc-env=BOKAI_VERSION={stamped}");
}
