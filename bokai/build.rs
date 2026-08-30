//! Fold a release stamp into what `bokai --version` prints.

fn main() {
    println!("cargo:rerun-if-env-changed=BOKAI_BUILD");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let stamped = match std::env::var("BOKAI_BUILD") {
        Ok(stamp) if !stamp.is_empty() && stamp != "dev" => format!("{version} ({stamp})"),
        _ => version,
    };
    println!("cargo:rustc-env=BOKAI_VERSION={stamped}");
}
