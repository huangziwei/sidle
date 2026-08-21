//! What an app's tree already says it is.
//!
//! An app is a directory under `extensions/` on a Kindle's `/mnt/us`, built by
//! a repo that has never heard of sidle. So none of this is read out of a file
//! sidle asks that repo to carry: the id is the directory's own name, the
//! display name and version come from the KUAL descriptor the Kindle itself
//! defines at `extensions/<id>/config.xml`, and the tile is whichever
//! `documents/*.sh` launches the extension.
//!
//! Every field but the id is optional. A tree that states none of them is a
//! whole app — `extensions/steb/bin/steb` installs the same whether or not
//! anything names a version — so a missing field shows as a missing field and
//! never as a reason to refuse the tree.

use std::path::Path;

use anyhow::{Result, bail};
use quick_xml::events::Event;
use serde::Serialize;

/// The Kindle's own extension descriptor, inside the app's directory.
pub const KUAL_DESCRIPTOR: &str = "config.xml";

/// A tile is a shell scriptlet whose `# Icon:` line carries a base64 PNG, so it
/// runs to tens of kilobytes. Past this, it is not a tile.
const MAX_TILE_BYTES: u64 = 1 << 20;

/// How far into a scriptlet the `# Name:` header can be.
const TILE_HEADER_BYTES: usize = 4096;

/// Who an app is, as its own tree states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppIdentity {
    /// The directory name under `extensions/`. Also the key of the `apps`
    /// table, the manifest's per-app group, and the install receipt.
    pub id: String,
    /// For display. The descriptor's `<name>`, else the tile's `# Name:`, else
    /// the id — which is what bokai, whose name *is* its directory, shows.
    pub name: String,
    /// Whatever the repo calls this build, when it says so. Compared as an
    /// opaque string and never as an ordering; the downgrade guard uses
    /// `built_at`.
    pub version: Option<String>,
    /// The app's launcher tile, mount-relative under `documents/`. The tile
    /// runs the extension, so it names it, and that is how it is found. An
    /// extension with no front door — bokai is run over SSH — has none.
    pub tile: Option<String>,
}

impl AppIdentity {
    /// Read what `mount`'s tree says about the app in `extensions/<id>`.
    pub fn read(mount: &Path, id: &str) -> Result<Self> {
        validate_id(id)?;
        let descriptor = mount.join("extensions").join(id).join(KUAL_DESCRIPTOR);
        let (xml_name, version) = match std::fs::read(&descriptor) {
            Ok(bytes) => kual_information(&bytes),
            Err(_) => (None, None),
        };
        let tile = find_tile(mount, id);
        let name = xml_name
            .or_else(|| {
                tile.as_ref()
                    .and_then(|t| std::fs::read(mount.join(t)).ok())
                    .and_then(|bytes| tile_name(&bytes))
            })
            .unwrap_or_else(|| id.to_string());
        Ok(Self {
            id: id.to_string(),
            name,
            version,
            tile,
        })
    }

    /// The app's own directory, mount-relative.
    pub fn extension_dir(&self) -> String {
        format!("extensions/{}", self.id)
    }
}

/// `<name>` and `<version>` from the `<information>` block of a KUAL
/// descriptor. Anything that does not parse yields neither rather than an
/// error: the descriptor is a courtesy, and an app whose display name falls
/// back to its id still installs byte for byte.
fn kual_information(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (None, None);
    };
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let (mut name, mut version) = (None, None);
    // A web app's descriptor uses the same element names under a `<widget>`
    // root, which is a different thing at a different path; the `<information>`
    // parent is what makes them this extension's.
    let mut in_information = false;
    let mut field: Option<Vec<u8>> = None;
    let mut value = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_vec();
                match local.as_slice() {
                    b"information" => in_information = true,
                    b"name" | b"version" if in_information => {
                        field = Some(local);
                        value.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) if field.is_some() => {
                if let Ok(chunk) = e.decode() {
                    value.push_str(&chunk);
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name().as_ref().to_vec();
                if local == b"information" {
                    in_information = false;
                }
                if field.as_deref() == Some(local.as_slice()) {
                    let text = value.trim().to_string();
                    let slot = if local == b"name" {
                        &mut name
                    } else {
                        &mut version
                    };
                    if !text.is_empty() && slot.is_none() {
                        *slot = Some(text);
                    }
                    field = None;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (name, version)
}

/// The `# Name:` a hotfix scriptlet gives itself, which is the name the library
/// shows on its tile.
pub fn tile_name(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(TILE_HEADER_BYTES)];
    for line in std::str::from_utf8(head).ok()?.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // The header ends at the first statement; past it is the script, not a
        // declaration about it.
        let comment = trimmed.strip_prefix('#')?;
        if let Some(name) = comment.trim_start().strip_prefix("Name:") {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// The tile that launches `extensions/<id>`.
///
/// A tile exists to run the app, so it spells the app's directory out — every
/// one in the fleet execs or calls something under
/// `/mnt/us/extensions/<id>/`. That reference is the link, so no repo has to
/// declare it and a tile renamed inside its own repo is still found. The
/// trailing separator is what keeps `kfxdedrm` from claiming `kfxdedrm-fe`'s.
fn find_tile(mount: &Path, id: &str) -> Option<String> {
    let needle = format!("extensions/{id}/");
    let mut hits: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(mount.join("documents")).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sh") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_TILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if text.contains(&needle)
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            hits.push(format!("documents/{name}"));
        }
    }
    hits.sort();
    hits.into_iter().next()
}

/// An id is a directory name that has to serve as a path segment on a FAT
/// mount, in a URL, and in a mount-relative key. It is not sidle's to choose —
/// it is whatever the repo named the directory — so this refuses only what
/// could not address a file.
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id == "." || id == ".." {
        bail!("an app id is a directory name under extensions/, and {id:?} is not one");
    }
    if id.starts_with('.') {
        bail!("{id:?} is a hidden directory, not an app");
    }
    if id.contains('/') || id.contains('\\') {
        bail!("{id:?} contains a path separator, and an id is one directory name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    const DESCRIPTOR: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<extension>
    <information>
        <name>KFX DeDRM</name>
        <version>0.3.0</version>
        <author>hzwei.dev</author>
        <id>kfxdedrm-fe</id>
    </information>
    <menus>
        <menu type="json">menu.json</menu>
    </menus>
</extension>"#;

    #[test]
    fn the_kual_descriptor_names_the_app() {
        let (name, version) = kual_information(DESCRIPTOR);
        assert_eq!(name.as_deref(), Some("KFX DeDRM"));
        assert_eq!(version.as_deref(), Some("0.3.0"));
    }

    /// An on-device web app's descriptor uses the same element names outside an
    /// `<information>` block. Reading one as an app's identity would take a
    /// vendored widget's name for the app's.
    #[test]
    fn a_widget_descriptor_yields_nothing() {
        let widget = br#"<widget id="com.lzampier.btmanager" version="1.0">
            <name xml:lang="en">BT Manager</name>
        </widget>"#;
        assert_eq!(kual_information(widget), (None, None));
    }

    #[test]
    fn a_tree_with_no_descriptor_still_has_an_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/steb/bin/steb"), b"elf");
        write(
            &dev.join("documents/Steb.sh"),
            b"#!/bin/sh\n# Name: Steb\n\n/mnt/us/extensions/steb/bin/steb\n",
        );
        let app = AppIdentity::read(&dev, "steb").unwrap();
        assert_eq!(app.name, "Steb", "the tile names it when nothing else does");
        assert_eq!(app.version, None);
        assert_eq!(app.tile.as_deref(), Some("documents/Steb.sh"));
    }

    #[test]
    fn an_app_with_neither_descriptor_nor_tile_is_named_by_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/bokai/bin/bokai"), b"elf");
        let app = AppIdentity::read(&dev, "bokai").unwrap();
        assert_eq!(app.name, "bokai");
        assert_eq!(app.tile, None);
    }

    /// Two apps whose ids share a prefix sit next to each other in the fleet.
    #[test]
    fn a_tile_goes_to_the_extension_it_launches() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(
            &dev.join("extensions/kfxdedrm-fe/bin/launch.sh"),
            b"#!/bin/sh\n",
        );
        write(&dev.join("extensions/kfxdedrm/bin/engine"), b"elf");
        write(
            &dev.join("documents/KFXDeDRM.sh"),
            b"#!/bin/sh\n# Name: KFX DeDRM\nexec /mnt/us/extensions/kfxdedrm-fe/bin/launch.sh\n",
        );
        assert_eq!(
            AppIdentity::read(&dev, "kfxdedrm-fe")
                .unwrap()
                .tile
                .as_deref(),
            Some("documents/KFXDeDRM.sh")
        );
        assert_eq!(AppIdentity::read(&dev, "kfxdedrm").unwrap().tile, None);
    }

    /// documents/ is the writer's directory. Only the scriptlet that launches
    /// the app is the app's; a book that happens to sit beside it is not.
    #[test]
    fn nothing_else_in_documents_is_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/steb/bin/steb"), b"elf");
        write(&dev.join("documents/My Novel.txt"), b"chapter one");
        write(
            &dev.join("documents/Other.sh"),
            b"# Name: Other\nexec /mnt/us/extensions/other/x\n",
        );
        assert_eq!(AppIdentity::read(&dev, "steb").unwrap().tile, None);
    }

    #[test]
    fn the_descriptor_outranks_the_tile_for_the_display_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/kfxdedrm-fe/config.xml"), DESCRIPTOR);
        write(
            &dev.join("documents/KFXDeDRM.sh"),
            b"#!/bin/sh\n# Name: DeDRM\nexec /mnt/us/extensions/kfxdedrm-fe/bin/launch.sh\n",
        );
        let app = AppIdentity::read(&dev, "kfxdedrm-fe").unwrap();
        assert_eq!(app.name, "KFX DeDRM");
        assert_eq!(app.version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn a_header_comment_after_the_name_does_not_hide_it() {
        let tile = b"#!/bin/sh\n# Name: Karyll\n# Author: Ziwei Huang\n# DontUseFBInk\n";
        assert_eq!(tile_name(tile).as_deref(), Some("Karyll"));
    }

    #[test]
    fn a_hidden_directory_is_not_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(AppIdentity::read(tmp.path(), ".git").is_err());
        assert!(AppIdentity::read(tmp.path(), "..").is_err());
    }
}
