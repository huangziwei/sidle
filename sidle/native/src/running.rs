//! Which files a process on this device is running.
//!
//! FAT keeps no inode alive behind a replaced directory entry, and `sh` reads a
//! script by offset: a file cannot be rewritten while something executes it.
//! The two paths sidle itself runs are written as `<path>.new` for the process
//! one level up to swap in. Every other app's files are plain writes, and
//! [`InUse`] is what says whether one of them is in use.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The kernel's process table.
pub const PROC: &str = "/proc";

/// What the kernel appends to a `/proc` path whose directory entry is gone. A
/// write at that path reaches a new file.
const DELETED: &str = " (deleted)";

/// Every file `/proc` names as in use: each process's executable, the absolute
/// paths in its argv, and its file-backed mappings.
#[derive(Debug, Default)]
pub struct InUse {
    paths: HashSet<PathBuf>,
}

impl InUse {
    /// Read every numbered directory under `proc`. A process that exits during
    /// the scan contributes what was readable, and a `/proc` that cannot be
    /// read at all yields an empty set.
    pub fn scan(proc: &Path) -> InUse {
        let mut paths = HashSet::new();
        let Ok(entries) = std::fs::read_dir(proc) else {
            return InUse { paths };
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().and_then(|n| n.parse::<u32>().ok()).is_none() {
                continue;
            }
            let dir = entry.path();

            if let Ok(exe) = std::fs::read_link(dir.join("exe")) {
                keep(&mut paths, &exe);
            }
            // A script is read by the shell running it, and names itself in
            // that shell's argv.
            if let Ok(cmdline) = std::fs::read(dir.join("cmdline")) {
                for arg in cmdline.split(|b| *b == 0) {
                    if let Ok(arg) = std::str::from_utf8(arg) {
                        keep(&mut paths, Path::new(arg));
                    }
                }
            }
            // A mapped library is read from its clusters for as long as the
            // mapping stands.
            if let Ok(maps) = std::fs::read_to_string(dir.join("maps")) {
                for line in maps.lines() {
                    // The five fields ahead of the pathname carry no `/`, and
                    // an anonymous mapping has no pathname at all.
                    if let Some(at) = line.find('/') {
                        keep(&mut paths, Path::new(line[at..].trim_end()));
                    }
                }
            }
        }
        InUse { paths }
    }

    /// Whether a process is running `path`.
    pub fn holds(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// How many distinct files the scan found.
    pub fn count(&self) -> usize {
        self.paths.len()
    }
}

/// Take an absolute path whose directory entry exists.
fn keep(paths: &mut HashSet<PathBuf>, path: &Path) {
    let Some(s) = path.to_str() else { return };
    if !s.starts_with('/') || s.ends_with(DELETED) {
        return;
    }
    paths.insert(PathBuf::from(s));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc` holding one process per `(pid, exe, cmdline, maps)`.
    fn fake_proc(tag: &str, procs: &[(u32, &str, &[&str], &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("sidle-running-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (pid, exe, argv, maps) in procs {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            // An empty `exe` stands for a process whose link the scan cannot
            // read.
            if !exe.is_empty() {
                let _ = std::os::unix::fs::symlink(exe, dir.join("exe"));
            }
            std::fs::write(dir.join("cmdline"), argv.join("\0")).unwrap();
            std::fs::write(dir.join("maps"), maps).unwrap();
        }
        // Not a pid, and never opened.
        std::fs::create_dir_all(root.join("self")).unwrap();
        root
    }

    #[test]
    fn a_running_binary_its_script_and_its_libraries_are_all_in_use() {
        let proc = fake_proc(
            "found",
            &[
                (
                    412,
                    "/mnt/us/extensions/sprocket/bin/sprocket",
                    &["/mnt/us/extensions/sprocket/bin/sprocket", "--daemon"],
                    "b6f00000-b6f20000 r-xp 00000000 b3:04 91 \
                     /mnt/us/extensions/sprocket/hid/lib/libhid.so\n",
                ),
                (
                    88,
                    "/bin/busybox",
                    &["sh", "/mnt/us/documents/Gadget.sh"],
                    "00008000-0000c000 r-xp 00000000 b3:04 12 /bin/busybox\n",
                ),
            ],
        );
        let in_use = InUse::scan(&proc);

        assert!(in_use.holds(Path::new("/mnt/us/extensions/sprocket/bin/sprocket")));
        assert!(in_use.holds(Path::new("/mnt/us/extensions/sprocket/hid/lib/libhid.so")));
        assert!(
            in_use.holds(Path::new("/mnt/us/documents/Gadget.sh")),
            "the shell reads the script it was handed"
        );
        assert!(!in_use.holds(Path::new("/mnt/us/extensions/gadget/bin/gadget")));
        assert!(!in_use.holds(Path::new("sh")), "argv keeps absolute paths");
        let _ = std::fs::remove_dir_all(&proc);
    }

    #[test]
    fn a_deleted_target_and_an_anonymous_mapping_are_not_paths() {
        let proc = fake_proc(
            "gone",
            &[(
                7,
                "",
                &["/mnt/us/extensions/gadget/bin/gadget (deleted)"],
                "b6e00000-b6e01000 rw-p 00000000 00:00 0 \n\
                 b6e01000-b6e02000 rw-p 00000000 00:00 0          [heap]\n",
            )],
        );
        let in_use = InUse::scan(&proc);

        assert!(!in_use.holds(Path::new("/mnt/us/extensions/gadget/bin/gadget")));
        assert_eq!(in_use.count(), 0, "nothing here names a live file");
        let _ = std::fs::remove_dir_all(&proc);
    }

    #[test]
    fn a_proc_that_cannot_be_read_holds_nothing() {
        let in_use = InUse::scan(Path::new("/nowhere/proc"));
        assert_eq!(in_use.count(), 0);
    }
}
