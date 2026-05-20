//! Fetch the color cover for an ASIN.
//!
//! Why this exists: KOA2 / Paperwhite / other monochrome Kindles get
//! grayscale-baked `.kfx` builds from Amazon. The cover image embedded in
//! the KFX (and therefore in the EPUB we synthesize from it) is already
//! desaturated — there's nothing in the file we can recover to colorize it.
//!
//! The cover Amazon shows on the *product page*, however, is the original
//! color art. We grab it from the public `/images/P/<ASIN>.<locale>...`
//! pattern (same approach calibre uses for its Amazon metadata source).
//!
//! Locale code is picked from the book's `language` field — there's no
//! marketplace marker inside KFX itself (verified against kfxlib's
//! `yj_metadata.py`), so language is the strongest proxy we have for
//! "which Amazon store served this book". Per project decision: try one
//! locale only; on miss, keep the grayscale fallback.
//!
//! The endpoint sometimes 200s with Amazon's "no image" placeholder
//! (~hundreds of bytes); we reject anything under `PLACEHOLDER_THRESHOLD`.

use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Anything smaller than this is Amazon's no-cover placeholder, not a real
/// cover. Real covers are tens of KB at minimum.
const PLACEHOLDER_THRESHOLD: usize = 2048;

/// Returns the cover bytes on success, `None` for any failure mode (network
/// error, non-2xx response, placeholder-sized payload, missing ASIN).
pub async fn fetch_color_cover(asin: &str, language: &str) -> Option<Vec<u8>> {
    if asin.is_empty() {
        eprintln!("[sidle/cover-fetch] skip: empty ASIN");
        return None;
    }
    let locale = locale_for_language(language);
    let url = format!(
        "https://images-na.ssl-images-amazon.com/images/P/{asin}.{locale}.LZZZZZZZ.jpg"
    );
    eprintln!("[sidle/cover-fetch] GET {url} (language={language:?})");

    let client = match reqwest::Client::builder()
        .user_agent("sidle/0.1")
        .timeout(TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sidle/cover-fetch] client build failed: {e}");
            return None;
        }
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[sidle/cover-fetch] request failed: {e}");
            return None;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        eprintln!("[sidle/cover-fetch] non-2xx status: {status}");
        return None;
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[sidle/cover-fetch] body read failed: {e}");
            return None;
        }
    };
    if bytes.len() < PLACEHOLDER_THRESHOLD {
        eprintln!(
            "[sidle/cover-fetch] sub-threshold payload ({} bytes) — treating as placeholder",
            bytes.len()
        );
        return None;
    }
    eprintln!(
        "[sidle/cover-fetch] OK: {} bytes from locale={locale}",
        bytes.len()
    );
    Some(bytes.to_vec())
}

/// Map a book language (BCP-47 — "ja", "ja-JP", "en-US", …) to Amazon's
/// numeric locale segment in the `/images/P/` URL.
///
/// Defaults to `01` (amazon.com / US) for unknown languages; that's the
/// catalog that covers the broadest set of titles when in doubt.
fn locale_for_language(lang: &str) -> &'static str {
    let prefix: String = lang
        .chars()
        .take(2)
        .flat_map(char::to_lowercase)
        .collect();
    match prefix.as_str() {
        "ja" => "09", // amazon.co.jp
        "en" => "01", // amazon.com
        "de" => "03", // amazon.de
        "fr" => "08", // amazon.fr
        "es" => "13", // amazon.es
        "it" => "29", // amazon.it
        "pt" => "28", // amazon.com.br
        _ => "01",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_mapping() {
        assert_eq!(locale_for_language("ja"), "09");
        assert_eq!(locale_for_language("ja-JP"), "09");
        assert_eq!(locale_for_language("JA"), "09");
        assert_eq!(locale_for_language("en"), "01");
        assert_eq!(locale_for_language("en-US"), "01");
        assert_eq!(locale_for_language("de-DE"), "03");
        // Unknown / empty fall back to US.
        assert_eq!(locale_for_language(""), "01");
        assert_eq!(locale_for_language("xx"), "01");
    }
}
