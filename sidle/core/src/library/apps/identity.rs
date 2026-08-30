//! What an app's tree says it is.

use std::path::Path;

use anyhow::{Result, bail};
use quick_xml::events::Event;
use serde::Serialize;

/// The Kindle's own extension descriptor, inside the app's directory.
pub const KUAL_DESCRIPTOR: &str = "config.xml";

/// A tile is a shell scriptlet whose `# Icon:` line carries a base64 PNG, so it
/// runs to tens of kilobytes. Past this, it is not a tile.
const MAX_TILE_BYTES: u64 = 1 << 20;

/// The `# Icon:` value's prefix. Anything else in that header is not art, and
/// never reaches an `<img>`.
const ICON_PREFIX: &str = "data:image/";

/// Who an app is, as its own tree states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppIdentity {
    /// The directory name under `extensions/`. Also the key of the `apps`
    /// table, the manifest's per-app group, and the install receipt.
    pub id: String,
    /// For display. The descriptor's `<name>`, else the tile's `# Name:`, else
    /// the id — which is what bokai, whose name *is* its directory, shows.
    pub name: String,
    /// Whatever the repo calls this build, when it states one. Compared as an
    /// opaque string and never as an ordering; the downgrade guard uses
    /// `built_at`.
    pub version: Option<String>,
    /// The app's launcher tile, mount-relative under `documents/`. The tile
    /// runs the extension, so it names it, and that is how it is found. An
    /// extension with no front door — bokai is run over SSH — has none.
    pub tile: Option<String>,
    /// The tile's `# Icon:` art, a `data:image/…;base64,…` URI the library
    /// draws on the home screen. Absent for an app with no tile.
    pub icon: Option<String>,
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
        let scriptlet = tile
            .as_ref()
            .and_then(|t| std::fs::read(mount.join(t)).ok());
        let name = xml_name
            .or_else(|| scriptlet.as_deref().and_then(tile_name))
            .unwrap_or_else(|| id.to_string());
        let icon = scriptlet.as_deref().and_then(tile_icon);
        Ok(Self {
            id: id.to_string(),
            name,
            version,
            tile,
            icon,
        })
    }

    /// The app's own directory, mount-relative.
    pub fn extension_dir(&self) -> String {
        format!("extensions/{}", self.id)
    }
}

/// `<name>` and `<version>` from the `<information>` block of a KUAL
/// descriptor. Anything that does not parse yields neither.
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
    tile_header(bytes, "Name:")
}

/// The `# Icon:` art the library draws on that tile, as a data URI.
pub fn tile_icon(bytes: &[u8]) -> Option<String> {
    let value = tile_header(bytes, "Icon:")?;
    (value.starts_with(ICON_PREFIX) && value.contains(";base64,")).then_some(value)
}

/// The value of one `# <key> <value>` header. The header ends at the first
/// statement; past it is the script, not a declaration about it.
fn tile_header(bytes: &[u8], key: &str) -> Option<String> {
    for line in std::str::from_utf8(bytes).ok()?.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let comment = trimmed.strip_prefix('#')?;
        if let Some(value) = comment.trim_start().strip_prefix(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The tile that launches `extensions/<id>`.
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
        <name>Gizmo UI</name>
        <version>0.3.0</version>
        <author>example.dev</author>
        <id>gizmo-ui</id>
    </information>
    <menus>
        <menu type="json">menu.json</menu>
    </menus>
</extension>"#;

    #[test]
    fn the_kual_descriptor_names_the_app() {
        let (name, version) = kual_information(DESCRIPTOR);
        assert_eq!(name.as_deref(), Some("Gizmo UI"));
        assert_eq!(version.as_deref(), Some("0.3.0"));
    }

    /// A web app's descriptor carries the same element names outside an
    /// `<information>` block.
    #[test]
    fn a_widget_descriptor_yields_nothing() {
        let widget = br#"<widget id="com.example.btmanager" version="1.0">
            <name xml:lang="en">BT Manager</name>
        </widget>"#;
        assert_eq!(kual_information(widget), (None, None));
    }

    #[test]
    fn a_tree_with_no_descriptor_still_has_an_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/gadget/bin/gadget"), b"elf");
        write(
            &dev.join("documents/Gadget.sh"),
            b"#!/bin/sh\n# Name: Gadget\n\n/mnt/us/extensions/gadget/bin/gadget\n",
        );
        let app = AppIdentity::read(&dev, "gadget").unwrap();
        assert_eq!(
            app.name, "Gadget",
            "the tile names it when nothing else does"
        );
        assert_eq!(app.version, None);
        assert_eq!(app.tile.as_deref(), Some("documents/Gadget.sh"));
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
            &dev.join("extensions/gizmo-ui/bin/launch.sh"),
            b"#!/bin/sh\n",
        );
        write(&dev.join("extensions/gizmo/bin/engine"), b"elf");
        write(
            &dev.join("documents/GizmoUI.sh"),
            b"#!/bin/sh\n# Name: Gizmo UI\nexec /mnt/us/extensions/gizmo-ui/bin/launch.sh\n",
        );
        assert_eq!(
            AppIdentity::read(&dev, "gizmo-ui").unwrap().tile.as_deref(),
            Some("documents/GizmoUI.sh")
        );
        assert_eq!(AppIdentity::read(&dev, "gizmo").unwrap().tile, None);
    }

    /// documents/ is the writer's directory. Only the scriptlet that launches
    /// the app is the app's; a book that happens to sit beside it is not.
    #[test]
    fn nothing_else_in_documents_is_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/gadget/bin/gadget"), b"elf");
        write(&dev.join("documents/My Novel.txt"), b"chapter one");
        write(
            &dev.join("documents/Other.sh"),
            b"# Name: Other\nexec /mnt/us/extensions/other/x\n",
        );
        assert_eq!(AppIdentity::read(&dev, "gadget").unwrap().tile, None);
    }

    #[test]
    fn the_descriptor_outranks_the_tile_for_the_display_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/gizmo-ui/config.xml"), DESCRIPTOR);
        write(
            &dev.join("documents/GizmoUI.sh"),
            b"#!/bin/sh\n# Name: Gizmo\nexec /mnt/us/extensions/gizmo-ui/bin/launch.sh\n",
        );
        let app = AppIdentity::read(&dev, "gizmo-ui").unwrap();
        assert_eq!(app.name, "Gizmo UI");
        assert_eq!(app.version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn a_header_comment_after_the_name_does_not_hide_it() {
        let tile = b"#!/bin/sh\n# Name: Sprocket\n# Author: nobody\n# DontUseFBInk\n";
        assert_eq!(tile_name(tile).as_deref(), Some("Sprocket"));
    }

    /// The art runs to tens of kilobytes on its one line.
    #[test]
    fn the_icon_is_the_whole_data_uri() {
        let art = format!("data:image/png;base64,{}", "iVBOR".repeat(6000));
        let tile = format!("#!/bin/sh\n# Name: Gadget\n# Icon: {art}\n# DontUseFBInk\n\nexec x\n");
        assert_eq!(tile_icon(tile.as_bytes()).as_deref(), Some(art.as_str()));
        assert_eq!(tile_name(tile.as_bytes()).as_deref(), Some("Gadget"));
    }

    /// The value reaches an `<img src>`.
    #[test]
    fn a_header_that_is_not_a_data_uri_is_not_an_icon() {
        for header in [
            "# Icon: javascript:alert(1)\n",
            "# Icon: ../../etc/passwd\n",
            "# Icon: data:image/png,notbase64\n",
        ] {
            let tile = format!("#!/bin/sh\n{header}exec x\n");
            assert_eq!(tile_icon(tile.as_bytes()), None, "accepted {header}");
        }
    }

    #[test]
    fn a_tile_carries_its_art_into_the_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/gadget/bin/gadget"), b"elf");
        write(
            &dev.join("documents/Gadget.sh"),
            b"#!/bin/sh\n# Name: Gadget\n# Icon: data:image/png;base64,iVBORw0K\n\
              exec /mnt/us/extensions/gadget/bin/gadget\n",
        );
        let app = AppIdentity::read(&dev, "gadget").unwrap();
        assert_eq!(app.icon.as_deref(), Some("data:image/png;base64,iVBORw0K"));
    }

    #[test]
    fn an_app_with_no_tile_has_no_art() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/bokai/bin/bokai"), b"elf");
        assert_eq!(AppIdentity::read(&dev, "bokai").unwrap().icon, None);
    }

    #[test]
    fn a_hidden_directory_is_not_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(AppIdentity::read(tmp.path(), ".git").is_err());
        assert!(AppIdentity::read(tmp.path(), "..").is_err());
    }
}
