use std::process::{Command, Stdio};

/// Cross-compile target for the on-Kindle native binary (`sidle-native`).
/// Cortex-A9 NEON hardfloat, statically linked against musl so the resulting
/// binary is firmware-version-agnostic. See `.cargo/config.toml` for the
/// linker setup.
const KUAL_TARGET: &str = "armv7-unknown-linux-musleabihf";

fn main() {
    rebuild_kual_binary();
    tauri_build::build();
}

/// Build `sidle-native` for the Kindle target as part of every desktop build.
///
/// Cargo only re-runs this script when the watched paths change (see the
/// `rerun-if-changed` directives), so iterating on sidle-tauri alone is
/// free. When sidle-native source actually changed, the inner cargo's own
/// incremental layer still fast-paths unchanged transitive deps.
///
/// Override with `SIDLE_SKIP_NATIVE=1` for the rare desktop-only iteration
/// where you don't want to pay the cross-compile at all (e.g., quick UI
/// experiments before the armv7 toolchain is installed on a fresh machine).
fn rebuild_kual_binary() {
    // Tell cargo when to re-run this script. Anything else changing in
    // sidle-tauri only triggers the normal tauri build path.
    println!("cargo:rerun-if-changed=../native/src");
    println!("cargo:rerun-if-changed=../native/Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rerun-if-env-changed=SIDLE_SKIP_NATIVE");

    if std::env::var_os("SIDLE_SKIP_NATIVE").is_some() {
        println!("cargo:warning=SIDLE_SKIP_NATIVE set; skipping sidle-native cross-compile");
        return;
    }

    // Verify the cross target is installed. A missing target manifests as
    // an opaque "can't find core for armv7-unknown-linux-musleabihf" error
    // deep in the inner cargo's output; surface a one-liner instead.
    if let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        && out.status.success()
    {
        let installed = String::from_utf8_lossy(&out.stdout);
        if !installed.lines().any(|l| l.trim() == KUAL_TARGET) {
            panic!(
                "\n\nsidle-native cross-compile target '{KUAL_TARGET}' is not installed.\n\
                 Install it with:\n    rustup target add {KUAL_TARGET}\n\n\
                 Or set SIDLE_SKIP_NATIVE=1 to skip the kual rebuild for this desktop build.\n"
            );
        }
    }

    // Invoke from the workspace root so the inner cargo sees the workspace
    // Cargo.toml and the .cargo/config.toml linker setup.
    //
    // env_clear() + restore essentials: the outer cargo sets `RUSTFLAGS`,
    // `TARGET=aarch64-apple-darwin`, `CARGO_CFG_TARGET_OS=macos`, and a long
    // list of other `CARGO_*` vars pinned to the host build. Inheriting them
    // poisons the cross-compile (e.g. `RUSTFLAGS` with `-l framework=…` fails
    // with "library kind `framework` is only supported on Apple targets"
    // when applied to armv7-linux). Keep only what the new cargo needs to
    // find tools and config.
    let mut cmd = Command::new("cargo");
    cmd.env_clear();
    for key in ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "RUSTUP_HOME", "CARGO_HOME"] {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }

    let status = cmd
        .args([
            "build",
            "--release",
            "--target",
            KUAL_TARGET,
            "-p",
            "sidle-native",
        ])
        .current_dir("../..")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("failed to invoke cargo for sidle-native");

    if !status.success() {
        panic!("sidle-native cross-compile failed (exit {status})");
    }
}
