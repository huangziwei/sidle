//! Fetch the color cover for an ASIN.

use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Anything smaller than this is Amazon's no-cover placeholder, not a real
/// cover. Real covers are tens of KB at minimum.
const PLACEHOLDER_THRESHOLD: usize = 2048;

/// Returns the cover bytes on success, `None` for any failure mode (network
/// error, non-2xx response, placeholder-sized payload, missing or
/// non-catalogue ASIN).
pub fn fetch_color_cover(asin: &str, language: &str) -> Option<Vec<u8>> {
    if asin.is_empty() {
        eprintln!("[sidle/cover-fetch] skip: empty ASIN");
        return None;
    }
    if !looks_like_real_amazon_asin(asin) {
        // bokai stamps a 32-char Crockford-Base32 identifier on
        eprintln!("[sidle/cover-fetch] skip: not a real ASIN ({asin:?})");
        return None;
    }
    let locale = locale_for_language(language);
    eprintln!("[sidle/cover-fetch] asin={asin} language={language:?} locale={locale}");

    let client = match reqwest::blocking::Client::builder()
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

    // Full-res source render first, capped legacy size as fallback. See the
    // module docs for why `_SCRM_` wins and when it has to fall through.
    let base = format!("https://images-na.ssl-images-amazon.com/images/P/{asin}.{locale}");
    for suffix in ["_SCRM_", "LZZZZZZZ"] {
        if let Some(bytes) = fetch_variant(&client, &format!("{base}.{suffix}.jpg")) {
            return Some(bytes);
        }
    }
    None
}

/// Fetch one cover-URL variant. Returns the bytes only when the response is a
fn fetch_variant(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    eprintln!("[sidle/cover-fetch] GET {url}");
    let resp = match client.get(url).send() {
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
    let bytes = match resp.bytes() {
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
    eprintln!("[sidle/cover-fetch] OK: {} bytes", bytes.len());
    Some(bytes.to_vec())
}

/// Real Amazon catalogue ASINs are 10 chars, uppercase alphanumeric. bokai-
/// kai's fabricated fallback (stamped on EPUB→KFX so Kindle's ingestion is
/// happy) is 32-char Crockford-Base32. Length alone distinguishes them.
pub use bokai::formats::kfx::metadata::looks_like_real_amazon_asin;

/// Map a book language (BCP-47 — "ja", "ja-JP", "en-US", …) to Amazon's
/// numeric locale segment in the `/images/P/` URL.
fn locale_for_language(lang: &str) -> &'static str {
    let prefix: String = lang.chars().take(2).flat_map(char::to_lowercase).collect();
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

/// Map a book language to the Amazon marketplace hostname to search for its
pub fn amazon_search_domain(lang: &str) -> &'static str {
    let prefix: String = lang.chars().take(2).flat_map(char::to_lowercase).collect();
    match prefix.as_str() {
        "ja" => "amazon.co.jp",
        "de" => "amazon.de",
        "fr" => "amazon.fr",
        "es" => "amazon.es",
        "it" => "amazon.it",
        "pt" => "amazon.com.br",
        _ => "amazon.com",
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
        // Fabricated 32-char Crockford-Base32 — the shape bokai EPUB→KFX bakes.
        assert!(!looks_like_real_amazon_asin(
            "J3AHLRDVFTGEMNBWMPPYB6CCANPXNWH6"
        ));
        // Off-by-one + casing edge cases.
        assert!(!looks_like_real_amazon_asin("B07PXGQC1")); // 9 chars
        assert!(!looks_like_real_amazon_asin("B07PXGQC1QQ")); // 11 chars
        assert!(!looks_like_real_amazon_asin("b07pxgqc1q")); // lowercase
        assert!(!looks_like_real_amazon_asin("B07PXGQC-Q")); // symbol
        assert!(!looks_like_real_amazon_asin(""));
    }
}
