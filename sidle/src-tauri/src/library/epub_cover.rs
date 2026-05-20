//! Replace the cover image inside an existing EPUB.
//!
//! Used by the cover-fetch flow: after `cover_fetch::fetch_color_cover` gives
//! us the color JPG from amazon.<region>, we both write the sidecar (for the
//! sidle gallery) and call into here to swap the cover entry inside the EPUB
//! itself. That way any external reader the user opens the EPUB with also
//! sees the color cover, not the grayscale baked-in one from a monochrome
//! Kindle build.
//!
//! Approach: rewrite the EPUB zip entry-by-entry. The cover entry is
//! replaced with the new bytes (renamed if the extension changes, e.g.
//! `cover.png` → `cover.jpg`); the OPF gets a targeted edit to the matching
//! `<item>` `href` and `media-type` attributes; every other entry is
//! `raw_copy_file`d through verbatim so we preserve compression methods and
//! — critically — the EPUB-required uncompressed first `mimetype` entry.

use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::library::import::write_bytes_atomic;

/// Replace the cover image inside `epub_path` with `new_bytes`. `new_ext` is
/// the lowercased extension matching the format of those bytes (e.g. `"jpg"`)
/// — used to compute the new `media-type` for the OPF manifest entry. The
/// in-zip filename is **kept** (we overwrite at the original path); only the
/// media-type attribute is updated. Renaming would orphan internal
/// references like `<image xlink:href="cover.jpeg"/>` inside
/// `titlepage.xhtml` and break the cover render in Apple Books.
///
/// No-ops cleanly (returns Ok) when the EPUB has no cover entry.
pub fn replace_cover(epub_path: &Path, new_bytes: &[u8], new_ext: &str) -> Result<()> {
    let epub_bytes = std::fs::read(epub_path)
        .with_context(|| format!("read {}", epub_path.display()))?;

    // boko's EPUB importer resolves cover_image to the zip-absolute path,
    // which is exactly what we need to look up the entry below.
    let cover_href: String = {
        let book = boko::Book::from_bytes(&epub_bytes, boko::Format::Epub)
            .with_context(|| "open epub for cover swap")?;
        match book.metadata().cover_image.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return Ok(()), // No cover declared — nothing to do.
        }
    };
    let opf_path = find_opf_path(&epub_bytes)?;

    let new_media_type = media_type_for_ext(new_ext);
    let cover_basename = basename(&cover_href);
    let old_media_type = media_type_for_ext(
        cover_basename
            .rsplit_once('.')
            .map(|(_, e)| e)
            .unwrap_or(""),
    );
    let media_type_changed = old_media_type != new_media_type;

    let mut out: Vec<u8> = Vec::with_capacity(epub_bytes.len() + new_bytes.len());
    rewrite_zip(
        &epub_bytes,
        &mut out,
        &cover_href,
        new_bytes,
        media_type_changed.then_some((opf_path.as_str(), cover_basename.as_str(), new_media_type)),
    )?;

    write_bytes_atomic(epub_path, &out)?;
    Ok(())
}

/// Walks the source EPUB and writes a fresh zip to `out`. The cover entry's
/// bytes get replaced in place (filename unchanged); when `opf_rewrite` is
/// `Some` (media-type changed), the OPF entry also gets a targeted edit.
/// Everything else is `raw_copy_file`'d, preserving the original compression
/// (and the EPUB-spec-required uncompressed `mimetype` at offset 0).
fn rewrite_zip(
    epub_bytes: &[u8],
    out: &mut Vec<u8>,
    cover_href: &str,
    new_cover_bytes: &[u8],
    opf_rewrite: Option<(&str, &str, &'static str)>,
) -> Result<()> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = zip::ZipArchive::new(cursor).with_context(|| "read epub zip")?;
    let mut writer = zip::ZipWriter::new(Cursor::new(out));

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("read entry {i}"))?;
        let name = entry.name().to_string();
        let compression = entry.compression();

        if name == cover_href {
            let opts = zip::write::SimpleFileOptions::default().compression_method(compression);
            writer
                .start_file(&name, opts)
                .with_context(|| format!("start cover entry {name}"))?;
            writer.write_all(new_cover_bytes)?;
        } else if let Some((opf_path, cover_basename, new_media_type)) = opf_rewrite
            && name == opf_path
        {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .with_context(|| "read opf for rewrite")?;
            let rewritten = rewrite_opf_for_cover(&text, cover_basename, new_media_type);
            let opts = zip::write::SimpleFileOptions::default().compression_method(compression);
            writer
                .start_file(&name, opts)
                .with_context(|| format!("start opf rewrite {name}"))?;
            writer.write_all(rewritten.as_bytes())?;
        } else {
            writer
                .raw_copy_file(entry)
                .with_context(|| format!("copy entry {name}"))?;
        }
    }

    let mut cursor = writer.finish().with_context(|| "finish epub zip")?;
    // `start_file` advances the cursor; rewinding isn't necessary because
    // we own the underlying Vec, but be explicit so the caller's `out`
    // reflects the final length without extra capacity slop being read.
    let _ = cursor.seek(std::io::SeekFrom::Start(0));
    Ok(())
}

/// Read `META-INF/container.xml` and pull the `full-path="…"` value off the
/// first `<rootfile>`. That's where the OPF lives.
fn find_opf_path(epub_bytes: &[u8]) -> Result<String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;
    let mut entry = archive
        .by_name("META-INF/container.xml")
        .with_context(|| "epub missing META-INF/container.xml")?;
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    const NEEDLE: &str = "full-path=\"";
    if let Some(idx) = text.find(NEEDLE) {
        let rest = &text[idx + NEEDLE.len()..];
        if let Some(end) = rest.find('"') {
            return Ok(rest[..end].to_string());
        }
    }
    bail!("could not parse OPF path from META-INF/container.xml")
}

fn basename(href: &str) -> String {
    href.rsplit_once('/')
        .map(|(_, b)| b)
        .unwrap_or(href)
        .to_string()
}

fn media_type_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Rewrite the OPF so the manifest `<item>` whose `href` ends with
/// `cover_basename` carries the new `media-type` (the href is unchanged —
/// we keep the cover file at its original path to avoid orphaning internal
/// references like `<image xlink:href="cover.jpeg"/>` inside
/// `titlepage.xhtml`). The OPF references files relative to its own
/// directory, so a basename match is correct for the same-directory case
/// (which is what `kfx_to_epub` emits — `OEBPS/content.opf` plus
/// `OEBPS/cover.<ext>` next to it).
fn rewrite_opf_for_cover(
    opf: &str,
    cover_basename: &str,
    new_media_type: &str,
) -> String {
    let mut out = String::with_capacity(opf.len() + 16);
    let href_needle = format!("href=\"{cover_basename}\"");
    for line in opf.split_inclusive('\n') {
        if line.contains("<item") && line.contains(&href_needle) {
            out.push_str(&replace_attr_value(line, "media-type", new_media_type));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Replace the value of `attr="..."` on a single line. Returns the line
/// unchanged if the attribute isn't found. Matches the leading-space form
/// (`" attr=\""`) so we don't accidentally match a longer attribute name
/// ending in `attr` (e.g. `mime-media-type=`).
fn replace_attr_value(line: &str, attr: &str, new_value: &str) -> String {
    let needle = format!(" {attr}=\"");
    let Some(start) = line.find(&needle) else {
        return line.to_string();
    };
    let val_start = start + needle.len();
    let Some(rel_end) = line[val_start..].find('"') else {
        return line.to_string();
    };
    let end = val_start + rel_end;
    let mut out = String::with_capacity(line.len() + new_value.len());
    out.push_str(&line[..val_start]);
    out.push_str(new_value);
    out.push_str(&line[end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_attr_value_basic() {
        let line = "    <item id=\"cover\" href=\"cover.png\" media-type=\"image/png\"/>";
        let out = replace_attr_value(line, "media-type", "image/jpeg");
        assert!(out.contains("media-type=\"image/jpeg\""));
        assert!(out.contains("href=\"cover.png\""));
    }

    #[test]
    fn rewrite_opf_swaps_mime_keeps_href() {
        let opf = "<package>\n  <manifest>\n    <item id=\"cover\" href=\"cover.png\" media-type=\"image/png\"/>\n    <item id=\"toc\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n  </manifest>\n</package>\n";
        let out = rewrite_opf_for_cover(opf, "cover.png", "image/jpeg");
        // media-type swapped, href preserved (in-zip file is overwritten
        // at the same path so internal SVG xlink:href references stay valid).
        assert!(out.contains("href=\"cover.png\""));
        assert!(out.contains("media-type=\"image/jpeg\""));
        // Other item untouched.
        assert!(out.contains("href=\"toc.ncx\""));
        assert!(out.contains("application/x-dtbncx+xml"));
        // Old media-type cleared.
        assert!(!out.contains("media-type=\"image/png\""));
    }

    #[test]
    fn basename_pulls_last_segment() {
        assert_eq!(basename("OEBPS/cover.jpg"), "cover.jpg");
        assert_eq!(basename("cover.jpg"), "cover.jpg");
        assert_eq!(basename("a/b/c.png"), "c.png");
    }
}
