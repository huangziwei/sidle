//! Fetch the color cover for an ASIN.
//!
//! Why this exists: KOA2 / Paperwhite / other monochrome Kindles get
//! grayscale-baked `.kfx` builds from Amazon. For those, the cover embedded
//! in the KFX (and therefore in the EPUB we synthesize from it) is already
//! desaturated and there's no way to recover color from the file itself —
//! we have to refetch from the product page.
//!
//! The cover Amazon shows on the product page is the original color art.
//! We grab it from the public `/images/P/<ASIN>.<locale>...` pattern (same
//! approach calibre uses for its Amazon metadata source).
//!
//! Locale code is picked from the book's `language` field — there's no
//! marketplace marker inside KFX itself (verified against kfxlib's
//! `yj_metadata.py`), so language is the strongest proxy we have for
//! "which Amazon store served this book". Per project decision: try one
//! locale only; on miss, keep whatever cover was embedded in the KFX
//! (which may be color if the KFX was produced from a color EPUB, or
//! grayscale if it came from Amazon's monochrome-device build).
//!
//! The endpoint sometimes 200s with Amazon's "no image" placeholder
//! (~hundreds of bytes); we reject anything under `PLACEHOLDER_THRESHOLD`.

use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Anything smaller than this is Amazon's no-cover placeholder, not a real
/// cover. Real covers are tens of KB at minimum.
const PLACEHOLDER_THRESHOLD: usize = 2048;

/// Returns the cover bytes on success, `None` for any failure mode (network
/// error, non-2xx response, placeholder-sized payload, missing or
/// non-catalogue ASIN).
pub async fn fetch_color_cover(asin: &str, language: &str) -> Option<Vec<u8>> {
    if asin.is_empty() {
        eprintln!("[sidle/cover-fetch] skip: empty ASIN");
        return None;
    }
    if !looks_like_real_amazon_asin(asin) {
        // boko-kai stamps a 32-char Crockford-Base32 identifier on
        // EPUB→KFX conversions to satisfy the Kindle ingestion path. That
        // value isn't a catalogue ASIN, so hitting `/images/P/<it>` always
        // 404s or returns the placeholder; the request itself burns a
        // round-trip and an Amazon log line for nothing. Real ASINs are
        // 10 chars `[A-Z0-9]` — gate on that shape.
        eprintln!("[sidle/cover-fetch] skip: not a real ASIN ({asin:?})");
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

/// Real Amazon catalogue ASINs are 10 chars, uppercase alphanumeric. boko-
/// kai's fabricated fallback (stamped on EPUB→KFX so Kindle's ingestion is
/// happy) is 32-char Crockford-Base32. Length alone distinguishes them.
/// Same predicate boko uses internally (see `boko-kai/src/export/kfx.rs`).
pub fn looks_like_real_amazon_asin(s: &str) -> bool {
    s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
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

    #[test]
    fn real_asin_predicate() {
        // Real catalogue ASINs (KDP "B0..." + older digit-leading).
        assert!(looks_like_real_amazon_asin("B07PXGQC1Q"));
        assert!(looks_like_real_amazon_asin("4087718654"));
        // Fabricated 32-char Crockford-Base32 from boko EPUB→KFX (the
        // exact shape from the bug report).
        assert!(!looks_like_real_amazon_asin("J3AHLRDVFTGEMNBWMPPYB6CCANPXNWH6"));
        // Off-by-one + casing edge cases.
        assert!(!looks_like_real_amazon_asin("B07PXGQC1"));    // 9 chars
        assert!(!looks_like_real_amazon_asin("B07PXGQC1QQ"));  // 11 chars
        assert!(!looks_like_real_amazon_asin("b07pxgqc1q"));   // lowercase
        assert!(!looks_like_real_amazon_asin("B07PXGQC-Q"));   // symbol
        assert!(!looks_like_real_amazon_asin(""));
    }
}
