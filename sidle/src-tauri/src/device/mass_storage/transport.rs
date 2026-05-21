//! `Transport` over a mounted Kindle volume.
//!
//! 1:1 lift of what the pre-Transport `push.rs`/`manifest.rs`/`dedrm.rs`
//! used to do with `std::fs`. No new behavior — atomic writes still go
//! through `<dest>.partial` + `rename`, deletes still succeed silently on
//! `NotFound`, listing returns `[]` when the parent dir is absent.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::device::transport::{TEntry, TPath, Transport};

pub struct MassStorageTransport {
    mount: PathBuf,
}

impl MassStorageTransport {
    pub fn new(mount: PathBuf) -> Self {
        Self { mount }
    }

    fn resolve(&self, path: &TPath) -> PathBuf {
        let mut out = self.mount.clone();
        for seg in path.segments() {
            out.push(seg);
        }
        out
    }
}

impl Transport for MassStorageTransport {
    fn read(&self, path: &TPath) -> Result<Vec<u8>> {
        let p = self.resolve(path);
        std::fs::read(&p).with_context(|| format!("read {}", p.display()))
    }

    fn write_atomic(&self, path: &TPath, bytes: &[u8]) -> Result<()> {
        let dest = self.resolve(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = with_partial_suffix(&dest);
        std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
        Ok(())
    }

    fn copy_in_atomic(&self, src_local: &Path, dest: &TPath) -> Result<()> {
        let dest_full = self.resolve(dest);
        if let Some(parent) = dest_full.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = with_partial_suffix(&dest_full);
        // Plain read/write rather than `std::fs::copy`. On macOS, copy uses
        // `fcopyfile(COPYFILE_ALL)` which carries extended attributes — and
        // on FAT/exFAT (every Kindle's storage) the kernel materializes
        // those xattrs as a hidden `._<filename>` AppleDouble companion
        // next to the real file. Those companions parse as a valid `.<sha>.kfx`
        // and show up as duplicate rows in our scan. read/write copies bytes
        // only, no metadata, no AppleDouble.
        {
            let mut src = std::fs::File::open(src_local)
                .with_context(|| format!("open {}", src_local.display()))?;
            let mut dst = std::fs::File::create(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            std::io::copy(&mut src, &mut dst)
                .with_context(|| format!("copy {} -> {}", src_local.display(), tmp.display()))?;
            dst.sync_all()
                .with_context(|| format!("sync {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &dest_full)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest_full.display()))?;
        Ok(())
    }

    fn delete(&self, path: &TPath) -> Result<bool> {
        let p = self.resolve(path);
        match std::fs::remove_file(&p) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow::anyhow!("remove {}: {}", p.display(), e)),
        }
    }

    fn delete_dir(&self, path: &TPath) -> Result<bool> {
        let p = self.resolve(path);
        match std::fs::remove_dir_all(&p) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow::anyhow!("remove_dir_all {}: {}", p.display(), e)),
        }
    }

    fn exists(&self, path: &TPath) -> Result<bool> {
        Ok(self.resolve(path).exists())
    }

    fn list(&self, dir: &TPath) -> Result<Vec<TEntry>> {
        let p = self.resolve(dir);
        let read_dir = match std::fs::read_dir(&p) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow::anyhow!("read_dir {}: {}", p.display(), e)),
        };
        let mut out = Vec::new();
        for entry in read_dir.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            out.push(TEntry {
                name,
                is_dir: meta.is_dir(),
                size: meta.is_file().then(|| meta.len()),
            });
        }
        Ok(out)
    }

    fn free_space(&self) -> Option<(u64, u64)> {
        let c = CString::new(self.mount.as_os_str().as_bytes()).ok()?;
        unsafe {
            let mut s = MaybeUninit::<libc::statvfs>::uninit();
            if libc::statvfs(c.as_ptr(), s.as_mut_ptr()) != 0 {
                return None;
            }
            let s = s.assume_init();
            let frsize = s.f_frsize as u64;
            let free = (s.f_bavail as u64).checked_mul(frsize)?;
            let total = (s.f_blocks as u64).checked_mul(frsize)?;
            Some((free, total))
        }
    }

    fn display_path(&self, path: &TPath) -> String {
        self.resolve(path).to_string_lossy().into_owned()
    }
}

fn with_partial_suffix(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!("{name}.partial"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_transport() -> (tempfile::TempDir, MassStorageTransport) {
        let tmp = tempfile::tempdir().unwrap();
        let t = MassStorageTransport::new(tmp.path().to_path_buf());
        (tmp, t)
    }

    #[test]
    fn write_then_read_round_trip() {
        let (_tmp, t) = temp_transport();
        let path = TPath::parse(".sidle/sent.json");
        t.write_atomic(&path, b"hello").unwrap();
        assert!(t.exists(&path).unwrap());
        assert_eq!(t.read(&path).unwrap(), b"hello");
    }

    #[test]
    fn write_creates_parents() {
        let (tmp, t) = temp_transport();
        let path = TPath::parse("documents/Sidle/a/b/c.kfx");
        t.write_atomic(&path, b"x").unwrap();
        assert!(tmp.path().join("documents/Sidle/a/b/c.kfx").exists());
    }

    #[test]
    fn delete_missing_returns_false() {
        let (_tmp, t) = temp_transport();
        let p = TPath::parse("nope.txt");
        assert_eq!(t.delete(&p).unwrap(), false);
    }

    #[test]
    fn delete_existing_returns_true() {
        let (_tmp, t) = temp_transport();
        let p = TPath::parse("x.txt");
        t.write_atomic(&p, b"x").unwrap();
        assert_eq!(t.delete(&p).unwrap(), true);
        assert!(!t.exists(&p).unwrap());
    }

    #[test]
    fn list_missing_dir_is_empty() {
        let (_tmp, t) = temp_transport();
        let entries = t.list(&TPath::parse("does/not/exist")).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn copy_in_atomic_lands_full_bytes() {
        let (tmp, t) = temp_transport();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("book.kfx");
        std::fs::write(&src, b"kfx-payload").unwrap();
        let dest = TPath::parse("documents/Sidle/book.kfx");
        t.copy_in_atomic(&src, &dest).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("documents/Sidle/book.kfx")).unwrap(),
            b"kfx-payload"
        );
        // No `.partial` left behind.
        assert!(
            !tmp.path()
                .join("documents/Sidle/book.kfx.partial")
                .exists()
        );
    }

    #[test]
    fn display_path_renders_full_filesystem_path() {
        let (tmp, t) = temp_transport();
        let p = TPath::parse("documents/Sidle/foo.kfx");
        let expected = tmp
            .path()
            .join("documents/Sidle/foo.kfx")
            .to_string_lossy()
            .into_owned();
        assert_eq!(t.display_path(&p), expected);
    }
}
