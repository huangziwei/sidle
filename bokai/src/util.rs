//! Utility functions with platform-specific implementations.

use std::borrow::Cow;

/// Seconds since the Unix epoch.
pub fn time_now_secs() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // `SOURCE_DATE_EPOCH`, when set, is the value returned in place of the
    // wall clock.
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH")
        && let Ok(secs) = epoch.trim().parse::<u64>()
    {
        return secs as u32;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Seed for the AZW3 writer's PalmDB unique id and FONT XOR key. Derived from
/// [`time_now_secs`], and pinned by `SOURCE_DATE_EPOCH` with it. The multiply
/// spreads the seconds value across the u64.
pub fn time_seed_nanos() -> u64 {
    (time_now_secs() as u64).wrapping_mul(1_000_000_007)
}

/// RFC 4122 v5 UUID derived from `name` via SHA-1 over the URL namespace.
/// Deterministic: one `name` always yields one UUID. Fills the OPF
/// `<dc:identifier opf:scheme="uuid">` slot.
pub fn uuid_v5(name: &str) -> String {
    const URL_NAMESPACE: [u8; 16] = [
        0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30,
        0xc8,
    ];
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(&URL_NAMESPACE);
    hasher.update(name.as_bytes());
    let digest = hasher.digest().bytes();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50; // version 5
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15],
    )
}

/// Current UTC time formatted as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`).
///
/// Fills `dcterms:modified` and KFX `modified_date`.
pub fn time_now_iso8601_utc() -> String {
    let secs = time_now_secs() as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let h = (sod / 3600) as u32;
    let m = ((sod / 60) % 60) as u32;
    let s = (sod % 60) as u32;
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
}

/// Map `f` over `items` across `available_parallelism` workers, preserving
/// input order. Each worker takes one contiguous chunk. Empty, single-item and
/// single-core input maps serially.
pub fn parallel_map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let n_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(items.len());
    if n_workers <= 1 {
        return items.iter().map(f).collect();
    }
    let mut out: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let chunk_size = items.len().div_ceil(n_workers);
        let f = &f;
        let mut handles = Vec::with_capacity(n_workers);
        for chunk in items.chunks(chunk_size) {
            handles.push(scope.spawn(move || chunk.iter().map(f).collect::<Vec<R>>()));
        }
        let mut write_idx = 0;
        for h in handles {
            for r in h.join().expect("parallel_map worker panicked") {
                out[write_idx] = Some(r);
                write_idx += 1;
            }
        }
    });
    out.into_iter().map(|slot| slot.expect("filled")).collect()
}

// Howard Hinnant's `civil_from_days`. Days are counted from 1970-01-01.
// Reference: https://howardhinnant.github.io/date_algorithms.html
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Decode `bytes` as UTF-8, then as `hint_encoding`, then as Windows-1252.
/// `hint_encoding` is the label from an XML declaration or document metadata.
/// `Cow::Borrowed` for input that is valid UTF-8.
pub fn decode_text<'a>(bytes: &'a [u8], hint_encoding: Option<&str>) -> Cow<'a, str> {
    // `decode` strips a BOM.
    let (result, _encoding, malformed) = encoding_rs::UTF_8.decode(bytes);

    if !malformed {
        return result;
    }

    if let Some(name) = hint_encoding
        && let Some(encoding) = encoding_rs::Encoding::for_label(name.as_bytes())
    {
        let (result, _, _) = encoding.decode(bytes);
        return result;
    }

    // Windows-1252 is a superset of ISO-8859-1.
    let (result, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    result
}

// ============================================================================
// Image Dimension Extraction
// ============================================================================

/// `(width, height)` read from the header bytes of PNG, JPEG, GIF and JPEG-XR
/// data. `None` for any other format.
pub fn extract_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 {
        return None;
    }

    // PNG: width/height at bytes 16-23 in IHDR chunk
    if data.len() >= 24 && data[0] == 0x89 && data[1] == 0x50 && data[2] == 0x4E && data[3] == 0x47
    {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }

    // JPEG: dimensions live in the SOF markers.
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return extract_jpeg_dimensions(data);
    }

    // GIF: width/height at bytes 6-9 (little-endian)
    if data.len() >= 10 && data[0] == 0x47 && data[1] == 0x49 && data[2] == 0x46 {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }

    // JPEG XR / HD Photo (II-BC): IMAGE_WIDTH/IMAGE_HEIGHT in the TIFF IFD.
    if data[0] == 0x49 && data[1] == 0x49 && data[2] == 0xBC {
        return jxr::decode::container::parse(data)
            .ok()
            .map(|c| (c.image_width, c.image_height));
    }

    None
}

/// `(width, height)` from a JPEG's SOF marker.
fn extract_jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }

        let marker = data[i + 1];

        // SOF (Start of Frame) markers, one per encoding type.
        if matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        ) && i + 9 < data.len()
        {
            let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((width, height));
        }

        // Skip to next marker
        if i + 3 < data.len() {
            let length = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            i += 2 + length;
        } else {
            break;
        }
    }
    None
}

// ============================================================================
// Resource Format Detection
// ============================================================================

/// Detected resource format.
///
/// This enum represents media formats commonly found in ebooks.
/// Detection is done via file extension or magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFormat {
    /// JPEG image
    Jpeg,
    /// PNG image
    Png,
    /// GIF image
    Gif,
    /// SVG image (vector)
    Svg,
    /// WebP image
    WebP,
    /// JPEG XR image (Microsoft HD Photo)
    Jxr,
    /// TrueType font
    Ttf,
    /// OpenType font
    Otf,
    /// Unknown/binary format
    Binary,
}

impl MediaFormat {
    /// Get the MIME type string for this format.
    pub fn mime_type(self) -> &'static str {
        match self {
            MediaFormat::Jpeg => "image/jpeg",
            MediaFormat::Png => "image/png",
            MediaFormat::Gif => "image/gif",
            MediaFormat::Svg => "image/svg+xml",
            MediaFormat::WebP => "image/webp",
            MediaFormat::Jxr => "image/jxr",
            MediaFormat::Ttf => "font/ttf",
            MediaFormat::Otf => "font/otf",
            MediaFormat::Binary => "application/octet-stream",
        }
    }

    /// Check if this format represents an image.
    pub fn is_image(self) -> bool {
        matches!(
            self,
            MediaFormat::Jpeg
                | MediaFormat::Png
                | MediaFormat::Gif
                | MediaFormat::Svg
                | MediaFormat::WebP
                | MediaFormat::Jxr
        )
    }

    /// Check if this format represents a font.
    pub fn is_font(self) -> bool {
        matches!(self, MediaFormat::Ttf | MediaFormat::Otf)
    }
}

/// The `MediaFormat` of `data`, read from its magic bytes, falling back to the
/// extension of `path`. `Binary` when neither names a format. A transcoded
/// resource keeps its source extension, and the magic bytes carry.
pub fn detect_media_format(path: &str, data: &[u8]) -> MediaFormat {
    if data.len() >= 4 {
        // JPEG: FF D8 FF
        if data[0] == 0xFF && data[1] == 0xD8 {
            return MediaFormat::Jpeg;
        }
        // PNG: 89 50 4E 47 (.PNG)
        if data[0] == 0x89 && data[1] == 0x50 && data[2] == 0x4E && data[3] == 0x47 {
            return MediaFormat::Png;
        }
        // GIF: 47 49 46 (GIF)
        if data[0] == 0x47 && data[1] == 0x49 && data[2] == 0x46 {
            return MediaFormat::Gif;
        }
        // JPEG XR / HD Photo: 49 49 BC (II-BC)
        if data[0] == 0x49 && data[1] == 0x49 && data[2] == 0xBC {
            return MediaFormat::Jxr;
        }
        // WebP: 52 49 46 46 ... 57 45 42 50 (RIFF...WEBP)
        if data.len() >= 12
            && data[0] == 0x52
            && data[1] == 0x49
            && data[2] == 0x46
            && data[3] == 0x46
            && data[8] == 0x57
            && data[9] == 0x45
            && data[10] == 0x42
            && data[11] == 0x50
        {
            return MediaFormat::WebP;
        }
    }

    // SVG, TTF and OTF are named by extension alone, as is an empty `data`.
    let path_lower = path.to_lowercase();

    if path_lower.ends_with(".jpg") || path_lower.ends_with(".jpeg") {
        return MediaFormat::Jpeg;
    }
    if path_lower.ends_with(".png") {
        return MediaFormat::Png;
    }
    if path_lower.ends_with(".gif") {
        return MediaFormat::Gif;
    }
    if path_lower.ends_with(".svg") {
        return MediaFormat::Svg;
    }
    if path_lower.ends_with(".webp") {
        return MediaFormat::WebP;
    }
    if path_lower.ends_with(".jxr") {
        return MediaFormat::Jxr;
    }
    if path_lower.ends_with(".ttf") {
        return MediaFormat::Ttf;
    }
    if path_lower.ends_with(".otf") {
        return MediaFormat::Otf;
    }

    MediaFormat::Binary
}

/// Strip invisible formatting characters used in ebooks.
///
/// Removes:
/// - U+00AD SOFT HYPHEN (hyphenation hints)
/// - U+200B ZERO WIDTH SPACE (word-break hints)
pub fn strip_ebook_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c != '\u{00AD}' && c != '\u{200B}' {
            out.push(c);
        }
    }
    out
}

/// The text of a markup fragment, with every `<…>` span removed and nothing
/// else changed. Entities and script bodies pass through verbatim.
pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Trim the ASCII whitespace markup indentation leaves on `s`. U+3000
/// IDEOGRAPHIC SPACE and U+00A0 NO-BREAK SPACE are typographic content, and
/// [`char::is_whitespace`] covers both.
pub fn trim_markup_space(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

/// The MIME type of `filename` + `data`, `None` where
/// [`detect_media_format`] reads `Binary`.
pub fn detect_mime_type(filename: &str, data: &[u8]) -> Option<&'static str> {
    let format = detect_media_format(filename, data);
    match format {
        MediaFormat::Binary => None,
        other => Some(other.mime_type()),
    }
}

// ============================================================================
// Date Utilities
// ============================================================================

/// The `YYYY-MM-DD` head of a timestamp, cut at the `T` of ISO 8601 or at the
/// space of the SQL-ish form (`2014-12-14 23:00:00+00:00`). A bare date is
/// returned whole.
pub fn truncate_to_date(s: &str) -> String {
    let s = s.trim();
    match s.find(['T', ' ']) {
        Some(pos) => s[..pos].to_string(),
        None => s.to_string(),
    }
}

// ============================================================================
// Encoding Detection
// ============================================================================

/// The encoding name in `<?xml … encoding="…" ?>`, read from the first 100
/// bytes. `None` where those bytes carry no declaration.
pub fn extract_xml_encoding(bytes: &[u8]) -> Option<&str> {
    let check_len = bytes.len().min(100);
    let prefix = &bytes[..check_len];

    let xml_start = prefix.windows(5).position(|w| w == b"<?xml")?;
    let after_xml = &prefix[xml_start..];

    let enc_pos = after_xml
        .windows(9)
        .position(|w| w.eq_ignore_ascii_case(b"encoding="))?;
    let after_enc = &after_xml[enc_pos + 9..];

    if after_enc.is_empty() {
        return None;
    }

    let quote = after_enc[0];
    if quote != b'"' && quote != b'\'' {
        return None;
    }

    let value_start = 1;
    let value_end = after_enc[value_start..].iter().position(|&b| b == quote)? + value_start;

    std::str::from_utf8(&after_enc[value_start..value_end]).ok()
}

// ============================================================================
// URI path helpers
// ============================================================================

/// Percent-decode a URI reference into the literal name a ZIP entry is stored
/// under. Each `%XX` triple becomes one byte and the assembled bytes are read
/// as UTF-8; `s` is returned whole where they are not valid UTF-8.
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// `href` as a valid URL: characters illegal in a path, query or fragment are
/// percent-encoded. `None` where the authority holds a character no encoding
/// makes legal — a space, a control byte, a non-numeric port.
pub fn sanitize_href(href: &str) -> Option<String> {
    fn is_illegal(c: char) -> bool {
        matches!(
            c,
            ' ' | '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '}' | '|' | '[' | ']'
        ) || (c as u32) < 0x20
    }
    // A space or control char in the authority (`host[:port]`) is illegal in
    // every form, `%20` included.
    if let Some((_, after)) = href.split_once("://") {
        let auth = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
        if auth.chars().any(|c| c == ' ' || (c as u32) < 0x20)
            || auth.to_ascii_lowercase().contains("%20")
        {
            return None;
        }
        // The port carries digits alone, and the host is non-empty. Any
        // `userinfo@` is stripped first; an IPv6 literal (`[::1]:80`) is
        // skipped for the colons in its host.
        if !auth.starts_with('[') {
            let host_port = auth.rsplit_once('@').map_or(auth, |(_, hp)| hp);
            if let Some((host, port)) = host_port.split_once(':')
                && (host.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()))
            {
                return None;
            }
        }
    }
    if !href.contains(is_illegal) {
        return Some(href.to_string());
    }
    let mut out = String::with_capacity(href.len() + 8);
    let mut buf = [0u8; 4];
    for c in href.chars() {
        if is_illegal(c) {
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_media_format_by_extension() {
        assert_eq!(detect_media_format("image.jpg", &[]), MediaFormat::Jpeg);
        assert_eq!(detect_media_format("image.JPEG", &[]), MediaFormat::Jpeg);
        assert_eq!(detect_media_format("image.png", &[]), MediaFormat::Png);
        assert_eq!(detect_media_format("image.gif", &[]), MediaFormat::Gif);
        assert_eq!(detect_media_format("image.svg", &[]), MediaFormat::Svg);
        assert_eq!(detect_media_format("image.webp", &[]), MediaFormat::WebP);
        assert_eq!(detect_media_format("font.ttf", &[]), MediaFormat::Ttf);
        assert_eq!(detect_media_format("font.otf", &[]), MediaFormat::Otf);
        assert_eq!(detect_media_format("unknown", &[]), MediaFormat::Binary);
    }

    #[test]
    fn test_detect_media_format_by_magic_bytes() {
        // JPEG magic bytes
        let jpeg_data = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(
            detect_media_format("unknown", &jpeg_data),
            MediaFormat::Jpeg
        );

        // PNG magic bytes
        let png_data = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_media_format("unknown", &png_data), MediaFormat::Png);

        // GIF magic bytes
        let gif_data = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61];
        assert_eq!(detect_media_format("unknown", &gif_data), MediaFormat::Gif);
    }

    #[test]
    fn test_media_format_mime_type() {
        assert_eq!(MediaFormat::Jpeg.mime_type(), "image/jpeg");
        assert_eq!(MediaFormat::Png.mime_type(), "image/png");
        assert_eq!(MediaFormat::Gif.mime_type(), "image/gif");
        assert_eq!(MediaFormat::Svg.mime_type(), "image/svg+xml");
        assert_eq!(MediaFormat::Ttf.mime_type(), "font/ttf");
        assert_eq!(MediaFormat::Binary.mime_type(), "application/octet-stream");
    }

    #[test]
    fn test_media_format_classification() {
        assert!(MediaFormat::Jpeg.is_image());
        assert!(MediaFormat::Png.is_image());
        assert!(!MediaFormat::Ttf.is_image());
        assert!(!MediaFormat::Binary.is_image());

        assert!(MediaFormat::Ttf.is_font());
        assert!(MediaFormat::Otf.is_font());
        assert!(!MediaFormat::Jpeg.is_font());
    }

    #[test]
    fn test_detect_mime_type() {
        assert_eq!(detect_mime_type("image.jpg", &[]), Some("image/jpeg"));
        assert_eq!(detect_mime_type("image.png", &[]), Some("image/png"));
        assert_eq!(detect_mime_type("unknown", &[]), None);
    }

    #[test]
    fn test_truncate_to_date() {
        // Full ISO timestamp -> date only
        assert_eq!(truncate_to_date("2022-05-26T16:26:51Z"), "2022-05-26");
        // A bare date.
        assert_eq!(truncate_to_date("2022-05-26"), "2022-05-26");
        // With timezone offset
        assert_eq!(truncate_to_date("2022-05-26T16:26:51+00:00"), "2022-05-26");
        // Space-separated, the form MOBI EXTH 106 carries.
        assert_eq!(truncate_to_date("2014-12-14 23:00:00+00:00"), "2014-12-14");
        assert_eq!(truncate_to_date("  2014-12-14  "), "2014-12-14");
    }

    #[test]
    fn test_percent_decode() {
        // No escapes: returned verbatim.
        assert_eq!(percent_decode("OEBPS/Text/ch1.html"), "OEBPS/Text/ch1.html");
        // `!` escaped as %21.
        assert_eq!(
            percent_decode("Text/CR%21Z717_split_000.html"),
            "Text/CR!Z717_split_000.html"
        );
        // Space and lowercase hex.
        assert_eq!(percent_decode("a%20b%2fc"), "a b/c");
        // Multi-byte UTF-8 (表) round-trips.
        assert_eq!(percent_decode("%E8%A1%A8.html"), "表.html");
        // A lone, non-escape `%` (and a truncated escape) is preserved.
        assert_eq!(percent_decode("50%25 done"), "50% done");
        assert_eq!(percent_decode("trailing%"), "trailing%");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        // Invalid hex digits are left untouched.
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn trim_markup_space_keeps_typographic_spaces() {
        // Markup indentation around a label.
        assert_eq!(
            trim_markup_space("\n      Chapter One\n    "),
            "Chapter One"
        );
        assert_eq!(trim_markup_space("\tA\r\n"), "A");

        // U+3000 and U+00A0 as content. `str::trim` drops both.
        assert_eq!(
            trim_markup_space("\u{3000}プロローグ\u{3000}潜入"),
            "\u{3000}プロローグ\u{3000}潜入"
        );
        assert_eq!(trim_markup_space("\u{00A0}A"), "\u{00A0}A");

        // Markup padding outside a leading U+3000.
        assert_eq!(
            trim_markup_space("  \u{3000}１\u{3000}序論 \n"),
            "\u{3000}１\u{3000}序論"
        );
    }
}
