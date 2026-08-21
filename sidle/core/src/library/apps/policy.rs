//! Per-path install rules: which files are payload, which are produced by the
//! install itself, and which are written as `<path>.new`.

use serde::{Deserialize, Serialize};

/// Suffix of the sidecar holding a file's build time in unix seconds.
pub const BUILD_STAMP_SUFFIX: &str = ".build-ts";

/// Mount-relative paths written as `<path>.new` for a process one level up to
/// swap in. `sh` reads a script by offset, and FAT keeps no inode alive under a
/// replacement.
const STAGED: &[&str] = &[
    "extensions/sidle/bin/sidle",
    "extensions/sidle/bin/sidle.sh",
];

/// Mount-relative paths the install produces: `etc/server.conf` is rendered per
/// device, `etc/ca.pem` is copied from the library root.
const PER_INSTALL: &[&str] = &[
    "extensions/sidle/etc/server.conf",
    "extensions/sidle/etc/ca.pem",
];

/// How a write lands on the device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// Write the path itself.
    #[default]
    Direct,
    /// Write `<path>.new`.
    Staged,
}

/// [`Apply::Staged`] for a path in [`STAGED`], [`Apply::Direct`] otherwise.
pub fn apply_for(mount_rel: &str) -> Apply {
    if STAGED.contains(&mount_rel) {
        Apply::Staged
    } else {
        Apply::Direct
    }
}

/// Whether `file_name` is installed: not `.DS_Store`, not a `._` resource fork,
/// not a [`BUILD_STAMP_SUFFIX`] sidecar.
pub fn is_payload(file_name: &str) -> bool {
    !(file_name == ".DS_Store"
        || file_name.starts_with("._")
        || file_name.ends_with(BUILD_STAMP_SUFFIX))
}

/// Whether `mount_rel` is in [`PER_INSTALL`].
pub fn is_per_install(mount_rel: &str) -> bool {
    PER_INSTALL.contains(&mount_rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_files_the_picker_executes_are_staged() {
        assert_eq!(apply_for("extensions/sidle/bin/sidle"), Apply::Staged);
        assert_eq!(apply_for("extensions/sidle/bin/sidle.sh"), Apply::Staged);
        assert_eq!(apply_for("extensions/sidle/config.xml"), Apply::Direct);
        assert_eq!(apply_for("extensions/karyll/bin/karyll"), Apply::Direct);
        assert_eq!(apply_for("documents/Sidle.sh"), Apply::Direct);
    }

    #[test]
    fn build_stamps_and_desktop_droppings_are_not_payload() {
        assert!(!is_payload(".DS_Store"));
        assert!(!is_payload("._karyll"));
        assert!(!is_payload("sidle.build-ts"));
        assert!(is_payload("sidle"));
        assert!(is_payload("config.xml"));
    }

    #[test]
    fn per_install_bytes_never_come_from_a_tree() {
        assert!(is_per_install("extensions/sidle/etc/server.conf"));
        assert!(is_per_install("extensions/sidle/etc/ca.pem"));
        assert!(!is_per_install("extensions/sidle/menu.json"));
        assert!(!is_per_install("extensions/karyll/hid/config.ini"));
    }
}
