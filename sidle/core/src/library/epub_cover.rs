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
/// Returns `Ok(true)` when the cover was swapped, `Ok(false)` when the EPUB
/// declares no cover entry (nothing to overwrite — see [`ensure_cover`], which
/// handles that case by regenerating from the KFX).
pub fn replace_cover(epub_path: &Path, new_bytes: &[u8], new_ext: &str) -> Result<bool> {
    let epub_bytes =
        std::fs::read(epub_path).with_context(|| format!("read {}", epub_path.display()))?;

    // boko's EPUB importer resolves cover_image to the zip-absolute path,
    // which is exactly what we need to look up the entry below.
    let cover_href: String = {
        let book = boko::Book::from_bytes(&epub_bytes, boko::Format::Epub)
            .with_context(|| "open epub for cover swap")?;
        match book.metadata().cover_image.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return Ok(false), // No cover declared — nothing to overwrite.
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
    Ok(true)
}

/// Ensure the EPUB at `epub_path` shows `new_bytes` as its cover, healing EPUBs
/// that carry no cover slot to overwrite. Three cases, in order:
///
/// 1. The EPUB already declares a cover → fast in-place [`replace_cover`] swap.
/// 2. The EPUB is cover-less *and is the derived side* of a KFX that has a cover
///    (a KFX import whose EPUB predates the `$258`/loc-0 cover fixes) →
///    regenerate the EPUB from `kfx_path`, then swap.
/// 3. Otherwise → [`insert_cover`] a fresh cover designation into the EPUB.
///
/// `epub_is_derived` enforces the one-way source→target conversion invariant:
/// case 2 regenerates the EPUB, so it may run **only** when the EPUB is derived
/// (the book's source is its KFX). For an EPUB-sourced book the EPUB *is* the
/// source and must never be overwritten by a regeneration — those fall to the
/// non-destructive insert of case 3. `kfx_path` is the KFX to regenerate from.
/// Best-effort by contract: callers treat any `Err` as a non-fatal, logged skip.
pub fn ensure_cover(
    epub_path: &Path,
    kfx_path: Option<&Path>,
    new_bytes: &[u8],
    new_ext: &str,
    epub_is_derived: bool,
) -> Result<()> {
    if replace_cover(epub_path, new_bytes, new_ext)? {
        return Ok(());
    }
    // Case 2: regenerate — but only from a KFX that is the source (EPUB derived)
    // and actually has a cover to carry over.
    if epub_is_derived
        && let Some(kfx) = kfx_path
    {
        let kfx_bytes = std::fs::read(kfx).with_context(|| format!("read {}", kfx.display()))?;
        if kfx_declares_cover(&kfx_bytes) {
            let epub_bytes = boko::kfx_to_epub::convert_to_epub(&kfx_bytes)
                .map_err(|e| anyhow::anyhow!("regenerate epub for coverless swap: {e:?}"))?;
            write_bytes_atomic(epub_path, &epub_bytes)?;
            replace_cover(epub_path, new_bytes, new_ext)?;
            return Ok(());
        }
    }
    // Case 3: the EPUB is the source (or has no covered KFX to derive from) —
    // insert a cover in place rather than regenerating it.
    insert_cover(epub_path, new_bytes, new_ext)
}

/// True if the KFX declares a resolvable cover. Used by [`ensure_cover`] to
/// decide between regenerating the EPUB from the KFX (cover present) and
/// inserting one directly (cover absent).
fn kfx_declares_cover(kfx_bytes: &[u8]) -> bool {
    boko::kfx_to_epub::loader::load(kfx_bytes)
        .map(|b| b.metadata.cover_resource_name.is_some())
        .unwrap_or(false)
}

/// Insert a cover into an EPUB that declares none: write the image next to the
/// OPF and add a `properties="cover-image"` manifest item (boko's top-priority
/// cover signal) plus a legacy `<meta name="cover">` for EPUB-2 readers. Unlike
/// [`replace_cover`], this *adds* the designation rather than overwriting an
/// existing one — used by [`ensure_cover`] for books whose source had no cover.
///
/// The designation is enough for readers to show the cover and for boko's
/// EPUB→KFX exporter to build a real cover section, so a subsequent reconvert
/// carries the cover into the KFX too.
pub fn insert_cover(epub_path: &Path, new_bytes: &[u8], new_ext: &str) -> Result<()> {
    let epub_bytes =
        std::fs::read(epub_path).with_context(|| format!("read {}", epub_path.display()))?;
    let opf_path = find_opf_path(&epub_bytes)?;
    // Cover asset lives next to the OPF, referenced by its relative basename.
    let opf_dir = opf_path
        .rsplit_once('/')
        .map(|(d, _)| format!("{d}/"))
        .unwrap_or_default();
    let cover_basename = format!("sidle_cover.{}", new_ext.to_ascii_lowercase());
    let cover_zip_path = format!("{opf_dir}{cover_basename}");
    let media_type = media_type_for_ext(new_ext);

    let cursor = Cursor::new(&epub_bytes);
    let mut archive = zip::ZipArchive::new(cursor).with_context(|| "read epub zip")?;
    let mut out: Vec<u8> = Vec::with_capacity(epub_bytes.len() + new_bytes.len());
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut out));
        for i in 0..archive.len() {
            let entry = archive
                .by_index(i)
                .with_context(|| format!("read entry {i}"))?;
            let name = entry.name().to_string();
            if name == opf_path {
                let compression = entry.compression();
                let mut text = String::new();
                let mut entry = entry;
                entry.read_to_string(&mut text).with_context(|| "read opf")?;
                let rewritten = inject_cover_into_opf(&text, &cover_basename, media_type);
                let opts = zip::write::SimpleFileOptions::default().compression_method(compression);
                writer.start_file(&name, opts)?;
                writer.write_all(rewritten.as_bytes())?;
            } else {
                writer
                    .raw_copy_file(entry)
                    .with_context(|| format!("copy entry {name}"))?;
            }
        }
        // The cover image itself (JPEG/PNG is already compressed → store).
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file(&cover_zip_path, opts)?;
        writer.write_all(new_bytes)?;
        writer.finish().with_context(|| "finish epub zip")?;
    }
    write_bytes_atomic(epub_path, &out)?;
    Ok(())
}

/// Add a `properties="cover-image"` manifest item (+ legacy `<meta name="cover">`)
/// to an OPF that declares no cover. `cover_basename` is the cover file's name
/// relative to the OPF. If the expected `</manifest>`/`</metadata>` closers are
/// missing the corresponding insert is skipped; the manifest item alone is
/// enough for boko to resolve the cover.
fn inject_cover_into_opf(opf: &str, cover_basename: &str, media_type: &str) -> String {
    let item = format!(
        "<item id=\"sidle-cover\" href=\"{cover_basename}\" media-type=\"{media_type}\" properties=\"cover-image\"/>"
    );
    let meta = "<meta name=\"cover\" content=\"sidle-cover\"/>";
    let mut out = opf.to_string();
    if out.contains("</manifest>") {
        out = out.replacen("</manifest>", &format!("  {item}\n</manifest>"), 1);
    }
    if out.contains("</metadata>") {
        out = out.replacen("</metadata>", &format!("  {meta}\n</metadata>"), 1);
    }
    out
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
fn rewrite_opf_for_cover(opf: &str, cover_basename: &str, new_media_type: &str) -> String {
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

    #[test]
    fn inject_cover_adds_cover_image_item_and_meta() {
        let opf = "<package>\n  <metadata>\n    <dc:title>T</dc:title>\n  </metadata>\n  <manifest>\n    <item id=\"toc\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n  </manifest>\n</package>\n";
        let out = inject_cover_into_opf(opf, "sidle_cover.jpg", "image/jpeg");
        // A properties="cover-image" manifest item (boko's top-priority signal).
        assert!(out.contains(r#"href="sidle_cover.jpg""#));
        assert!(out.contains(r#"properties="cover-image""#));
        assert!(out.contains(r#"media-type="image/jpeg""#));
        // A legacy EPUB-2 <meta name="cover"> pointing at it.
        assert!(out.contains(r#"<meta name="cover" content="sidle-cover"/>"#));
        // Existing content preserved.
        assert!(out.contains("href=\"toc.ncx\""));
        assert!(out.contains("<dc:title>T</dc:title>"));
    }

    #[test]
    fn inject_cover_tolerates_missing_closers() {
        // No </manifest> or </metadata> — must not panic, just no-op that part.
        let opf = "<package></package>";
        let out = inject_cover_into_opf(opf, "sidle_cover.png", "image/png");
        assert_eq!(out, opf);
    }
}
