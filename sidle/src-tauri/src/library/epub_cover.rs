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
/// — used both to rename the in-zip entry when the original cover had a
/// different extension, and to compute the new `media-type` for the OPF.
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

    let new_basename = format!("cover.{new_ext}");
    let cover_dir = parent_dir(&cover_href);
    let new_href = if cover_dir.is_empty() {
        new_basename.clone()
    } else {
        format!("{cover_dir}/{new_basename}")
    };
    let new_media_type = media_type_for_ext(new_ext);
    let need_rename = !new_href.eq_ignore_ascii_case(&cover_href);
    let old_basename = basename(&cover_href);

    let mut out: Vec<u8> = Vec::with_capacity(epub_bytes.len() + new_bytes.len());
    rewrite_zip(
        &epub_bytes,
        &mut out,
        &cover_href,
        &new_href,
        new_bytes,
        need_rename.then_some((opf_path.as_str(), &old_basename, &new_basename, new_media_type)),
    )?;

    write_bytes_atomic(epub_path, &out)?;
    Ok(())
}

/// Walks the source EPUB and writes a fresh zip to `out`. The cover entry
/// gets replaced; if `opf_rewrite` is `Some`, the OPF entry also gets a
/// targeted edit. Everything else is `raw_copy_file`'d, preserving the
/// original compression (and the EPUB-spec-required uncompressed `mimetype`
/// at offset 0).
fn rewrite_zip(
    epub_bytes: &[u8],
    out: &mut Vec<u8>,
    old_cover_href: &str,
    new_cover_href: &str,
    new_cover_bytes: &[u8],
    opf_rewrite: Option<(&str, &str, &str, &'static str)>,
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

        if name == old_cover_href {
            let opts = zip::write::SimpleFileOptions::default().compression_method(compression);
            writer
                .start_file(new_cover_href, opts)
                .with_context(|| format!("start cover entry {new_cover_href}"))?;
            writer.write_all(new_cover_bytes)?;
        } else if let Some((opf_path, old_basename, new_basename, new_media_type)) = opf_rewrite
            && name == opf_path
        {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .with_context(|| "read opf for rewrite")?;
            let rewritten =
                rewrite_opf_for_cover(&text, old_basename, new_basename, new_media_type);
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

fn parent_dir(href: &str) -> &str {
    href.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
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
/// `old_basename` instead carries `new_basename` and `new_media_type`. The
/// OPF references files relative to its own directory, so a basename match
/// is correct for the same-directory case (which is what `kfx_to_epub`
/// emits — `OEBPS/content.opf` plus `OEBPS/cover.<ext>` next to it).
fn rewrite_opf_for_cover(
    opf: &str,
    old_basename: &str,
    new_basename: &str,
    new_media_type: &str,
) -> String {
    let mut out = String::with_capacity(opf.len() + 16);
    let href_needle = format!("href=\"{old_basename}\"");
    for line in opf.split_inclusive('\n') {
        if line.contains("<item") && line.contains(&href_needle) {
            let with_href =
                line.replace(&href_needle, &format!("href=\"{new_basename}\""));
            let with_mime = replace_attr_value(&with_href, "media-type", new_media_type);
            out.push_str(&with_mime);
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
    fn rewrite_opf_swaps_href_and_mime_on_matching_line() {
        let opf = "<package>\n  <manifest>\n    <item id=\"cover\" href=\"cover.png\" media-type=\"image/png\"/>\n    <item id=\"toc\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n  </manifest>\n</package>\n";
        let out = rewrite_opf_for_cover(opf, "cover.png", "cover.jpg", "image/jpeg");
        assert!(out.contains("href=\"cover.jpg\""));
        assert!(out.contains("media-type=\"image/jpeg\""));
        // Other item untouched.
        assert!(out.contains("href=\"toc.ncx\""));
        assert!(out.contains("application/x-dtbncx+xml"));
        // No stale entry left.
        assert!(!out.contains("href=\"cover.png\""));
        assert!(!out.contains("media-type=\"image/png\""));
    }

    #[test]
    fn parent_and_basename() {
        assert_eq!(parent_dir("OEBPS/cover.jpg"), "OEBPS");
        assert_eq!(basename("OEBPS/cover.jpg"), "cover.jpg");
        assert_eq!(parent_dir("cover.jpg"), "");
        assert_eq!(basename("cover.jpg"), "cover.jpg");
    }
}
