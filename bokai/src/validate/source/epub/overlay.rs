//! Media Overlay (SMIL) documents — what the validator reads out of one.
//!
//! An overlay narrates a content document: `<text src>` points at the element
//! being read, `<audio src>` at the recording, and `clipBegin`/`clipEnd` mark
//! the span of it. The rules that key on this content are `MED-005` and
//! `MED-008`…`MED-014`; the overlay's *structure* is the schema engine's job
//! (`media-overlay-30.rnc` / `.sch`), so nothing here re-checks it.

use quick_xml::Reader;
use quick_xml::events::Event;

use super::{attr_by_local, local_name};

/// One `<audio>` element of an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioClip {
    /// The `src` attribute, raw (fragment kept — a fragment is MED-014).
    pub src: String,
    pub clip_begin: Option<String>,
    pub clip_end: Option<String>,
}

/// What one overlay document says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overlay {
    /// Every content-document reference: `<text src>` and the `epub:textref`
    /// of `<body>`/`<seq>`. Raw, fragment kept.
    pub text_refs: Vec<String>,
    pub audio: Vec<AudioClip>,
}

/// Read an overlay. A parse error simply ends the scan — well-formedness is
/// reported elsewhere, and the schema judges the structure.
pub fn parse(smil: &str) -> Overlay {
    let mut out = Overlay::default();
    let mut reader = Reader::from_str(smil);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                match local_name(e.name().as_ref()) {
                    // `epub:textref` is namespaced; matching by local name is
                    // the same conservative choice the rest of the validator
                    // makes for `epub:`-prefixed attributes.
                    b"body" | b"seq" => {
                        if let Some(href) = attr_by_local(&e, b"textref") {
                            out.text_refs.push(href);
                        }
                    }
                    b"text" => {
                        if let Some(href) = attr_by_local(&e, b"src") {
                            out.text_refs.push(href);
                        }
                    }
                    b"audio" => {
                        if let Some(src) = attr_by_local(&e, b"src") {
                            out.audio.push(AudioClip {
                                src,
                                clip_begin: attr_by_local(&e, b"clipBegin"),
                                clip_end: attr_by_local(&e, b"clipEnd"),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// A SMIL clock value in milliseconds, or `None` if it is not one.
///
/// A transcription of epubcheck's `SmilClock`, including the order it tries the
/// three formats in — *timecount first*, which is what makes `10:30` a partial
/// clock (10 min 30 s) rather than anything else, and `1.5h` a time count.
///
/// An unparseable value yields `None` and no finding: the grammar types
/// `clipBegin`/`clipEnd`, so a malformed clock is already an RSC-005.
pub fn clock(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_prefix("npt=").unwrap_or(s);
    // Time count: `<number>` with an optional `h`/`min`/`s`/`ms` unit, where a
    // bare number is seconds.
    if let Some((number, unit)) = split_timecount(s)
        && let Ok(value) = number.parse::<f64>()
    {
        return Some(match unit {
            "h" => value * 60.0 * 60.0 * 1000.0,
            "min" => value * 60.0 * 1000.0,
            "ms" => value,
            // Both "s" and no unit are seconds.
            _ => value * 1000.0,
        });
    }
    let (fields, fraction) = split_clock(s)?;
    let millis = match fields.as_slice() {
        // Full clock: hours may be any number of digits; minutes and seconds
        // are exactly two, under 60.
        [h, m, sec] => {
            let h: u64 = h.parse().ok()?;
            h as f64 * 60.0 * 60.0 * 1000.0
                + two_digit(m)? * 60.0 * 1000.0
                + two_digit(sec)? * 1000.0
        }
        // Partial clock: minutes and seconds only.
        [m, sec] => two_digit(m)? * 60.0 * 1000.0 + two_digit(sec)? * 1000.0,
        _ => return None,
    };
    Some(millis + fraction)
}

/// `"1.5h"` → `("1.5", "h")`. `None` when the string is not a number followed
/// by an optional unit — which is how a clock value falls through to the
/// colon-separated forms.
fn split_timecount(s: &str) -> Option<(&str, &str)> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    // `\d+([.]\d+)?` — at least one digit, at most one dot, digits after it.
    let mut parts = number.split('.');
    let whole = parts.next()?;
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(frac) = parts.next()
        && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    if parts.next().is_some() {
        return None; // more than one dot
    }
    match unit {
        "" | "h" | "min" | "s" | "ms" => Some((number, unit)),
        _ => None,
    }
}

/// Split a colon-separated clock into its integer fields and its fractional
/// milliseconds, e.g. `"1:02:03.5"` → `(["1", "02", "03"], 500.0)`.
fn split_clock(s: &str) -> Option<(Vec<&str>, f64)> {
    let (head, fraction) = match s.split_once('.') {
        None => (s, 0.0),
        Some((head, frac)) => {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (head, format!("0.{frac}").parse::<f64>().ok()? * 1000.0)
        }
    };
    Some((head.split(':').collect(), fraction))
}

/// A two-digit field under 60 (`[0-5]\d`), as a float.
fn two_digit(s: &str) -> Option<f64> {
    match s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) {
        true => s.parse::<f64>().ok().filter(|v| *v < 60.0),
        false => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overlay_yields_its_text_and_audio_references() {
        let smil = r#"<?xml version="1.0" encoding="UTF-8"?>
<smil xmlns="http://www.w3.org/ns/SMIL" xmlns:epub="http://www.idpf.org/2007/ops" version="3.0">
  <body>
    <seq id="s1" epub:textref="c1.xhtml#part1">
      <par id="p1">
        <text src="c1.xhtml#s1"/>
        <audio src="a.mp3" clipBegin="0s" clipEnd="10.5s"/>
      </par>
      <par id="p2">
        <text src="c1.xhtml#s2"/>
        <audio src="a.mp3" clipBegin="00:00:10.500" clipEnd="0:00:20"/>
      </par>
    </seq>
  </body>
</smil>"#;
        let overlay = parse(smil);
        assert_eq!(
            overlay.text_refs,
            ["c1.xhtml#part1", "c1.xhtml#s1", "c1.xhtml#s2"]
        );
        assert_eq!(overlay.audio.len(), 2);
        assert_eq!(overlay.audio[1].clip_begin.as_deref(), Some("00:00:10.500"));
    }

    /// The three SMIL clock formats, in the order epubcheck tries them.
    #[test]
    fn smil_clock_values_parse_the_way_the_java_one_does() {
        // Time count, the format tried first.
        assert_eq!(clock("10"), Some(10_000.0), "a bare number is seconds");
        assert_eq!(clock("3.5s"), Some(3_500.0));
        assert_eq!(clock("250ms"), Some(250.0));
        assert_eq!(clock("2min"), Some(120_000.0));
        assert_eq!(clock("1.5h"), Some(5_400_000.0));
        assert_eq!(
            clock("npt=30s"),
            Some(30_000.0),
            "the npt= prefix is optional"
        );
        // Full clock.
        assert_eq!(clock("0:00:10"), Some(10_000.0));
        assert_eq!(clock("1:02:03"), Some(3_723_000.0));
        assert_eq!(clock("00:00:10.500"), Some(10_500.0));
        assert_eq!(
            clock("124:00:00"),
            Some(446_400_000.0),
            "hours are unbounded"
        );
        // Partial clock.
        assert_eq!(clock("02:30"), Some(150_000.0));
        assert_eq!(clock("02:30.25"), Some(150_250.0));
        // Not clock values — the grammar rejects these, so they yield nothing
        // rather than a wrong comparison.
        for bad in [
            "", "abc", "1:2:3", "00:60:00", "0:99", "1.2.3", "10x", "-5s",
        ] {
            assert_eq!(clock(bad), None, "{bad:?} is not a clock value");
        }
    }

    #[test]
    fn a_malformed_document_yields_what_it_can() {
        let overlay = parse(r#"<smil><body><par><text src="a.xhtml#x"/><audio"#);
        assert_eq!(overlay.text_refs, ["a.xhtml#x"]);
        assert!(overlay.audio.is_empty());
    }
}
