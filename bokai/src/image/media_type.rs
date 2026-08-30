//! What an image's bytes actually are, from its leading bytes.

/// A raster format identifiable by its magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Gif,
    Webp,
    Bmp,
    /// JPEG-XR (the `II\xBC` WMPhoto container).
    Jxr,
}

impl ImageFormat {
    /// Identify by leading bytes; 12 bytes decide every case here. `None` for
    /// anything unrecognised, including SVG (text, no magic number).
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
            return Some(Self::Png);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::Webp);
        }
        if bytes.starts_with(b"BM") {
            return Some(Self::Bmp);
        }
        if bytes.starts_with(&[0x49, 0x49, 0xBC]) {
            return Some(Self::Jxr);
        }
        None
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Bmp => "image/bmp",
            Self::Jxr => "image/jxr",
        }
    }

    /// Lowercase extension without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Bmp => "bmp",
            Self::Jxr => "jxr",
        }
    }
}

/// Media type of these bytes, or `"application/octet-stream"` when nothing
/// matches — the honest answer for a payload we can't name.
pub fn media_type_of(bytes: &[u8]) -> &'static str {
    ImageFormat::sniff(bytes).map_or("application/octet-stream", ImageFormat::media_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_format_is_identified_by_its_magic() {
        let cases: &[(&[u8], ImageFormat)] = &[
            (&[0xFF, 0xD8, 0xFF, 0xE0], ImageFormat::Jpeg),
            (
                &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0],
                ImageFormat::Png,
            ),
            (b"GIF89a....", ImageFormat::Gif),
            (b"RIFF\0\0\0\0WEBPVP8 ", ImageFormat::Webp),
            (b"BM\0\0\0\0", ImageFormat::Bmp),
            (&[0x49, 0x49, 0xBC, 0x01], ImageFormat::Jxr),
        ];
        for (bytes, want) in cases {
            assert_eq!(ImageFormat::sniff(bytes), Some(*want), "sniffing {want:?}");
        }
    }

    #[test]
    fn truncated_and_unknown_payloads_are_not_guessed() {
        // Short prefixes of a real magic must not match — a 2-byte read that
        // happens to start `RIFF` is not a WEBP.
        assert_eq!(ImageFormat::sniff(b"RIFF"), None);
        assert_eq!(ImageFormat::sniff(&[0xFF, 0xD8]), None);
        assert_eq!(ImageFormat::sniff(b""), None);
        assert_eq!(ImageFormat::sniff(b"<svg xmlns="), None);
        assert_eq!(media_type_of(b"<svg xmlns="), "application/octet-stream");
    }

    #[test]
    fn a_riff_container_that_is_not_webp_is_not_claimed() {
        assert_eq!(ImageFormat::sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
    }
}
