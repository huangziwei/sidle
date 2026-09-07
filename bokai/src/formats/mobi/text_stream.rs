//! Text records decompressed and concatenated into one stream, and the byte
//! ranges of that stream a reading axis addresses.

use std::io;
use std::ops::Range;

use super::headers::{Compression, Encoding, MobiHeader, NULL_INDEX};
use super::huffcdic::HuffCdicReader;
use super::palmdoc;
use super::parser::strip_trailing_data;
use crate::model::AxisSlice;

/// The whole stream.
pub fn whole(
    read_record: impl Fn(usize) -> io::Result<Vec<u8>>,
    mobi: &MobiHeader,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    walk(&read_record, mobi, &mut |_, bytes| {
        out.extend_from_slice(bytes);
        true
    })?;
    Ok(out)
}

/// The stream over a range of the reading axis, which is a byte offset into
/// it. `end` of `None` returns the text record holding `start`. Records
/// decompress only as far as the range reaches.
pub fn axis_slice(
    read_record: impl Fn(usize) -> io::Result<Vec<u8>>,
    mobi: &MobiHeader,
    start: i64,
    end: Option<i64>,
) -> io::Result<AxisSlice> {
    let declared = mobi.text_length as i64;
    let empty = |extent| AxisSlice {
        from: start,
        to: start,
        extent,
        text: String::new(),
        mark: None,
    };
    let stop = end.map(|e| e.clamp(0, declared) as usize);
    if start < 0 || start >= declared || stop.is_some_and(|stop| stop <= start as usize) {
        return Ok(empty(declared));
    }

    let at = start as usize;
    let mut covered: Option<Range<usize>> = None;
    let mut bytes = Vec::new();
    let walked = walk(&read_record, mobi, &mut |covers, chunk| match stop {
        Some(stop) => {
            let from = covers.start.max(at);
            let to = covers.end.min(stop);
            if from < to {
                bytes.extend_from_slice(&chunk[from - covers.start..to - covers.start]);
                covered = Some(covered.clone().map_or(from..to, |had| had.start..to));
            }
            covers.end < stop
        }
        None => {
            if covers.contains(&at) {
                bytes.extend_from_slice(chunk);
                covered = Some(covers);
                return false;
            }
            true
        }
    })?;

    // No record covers `start`.
    let Some(covered) = covered else {
        return Ok(empty(walked as i64));
    };
    Ok(AxisSlice {
        from: covered.start as i64,
        to: covered.end as i64,
        extent: declared,
        mark: end.is_none().then(|| {
            decode(&bytes[..at - covered.start], mobi.encoding)
                .chars()
                .count()
        }),
        text: decode(&bytes, mobi.encoding),
    })
}

/// Decompress the text records in order, handing each one's bytes and the
/// stream range it covers to `visit`, which returns false to stop the walk.
/// Returns where the walk stopped.
fn walk(
    read_record: &dyn Fn(usize) -> io::Result<Vec<u8>>,
    mobi: &MobiHeader,
    visit: &mut dyn FnMut(Range<usize>, &[u8]) -> bool,
) -> io::Result<usize> {
    let mut huff =
        if mobi.compression == Compression::Huffman && mobi.huff_record_index != NULL_INDEX {
            let table = read_record(mobi.huff_record_index as usize)?;
            let mut dictionaries = Vec::new();
            for i in 0..mobi.huff_record_count.saturating_sub(1) {
                if let Ok(cdic) = read_record(mobi.huff_record_index as usize + 1 + i as usize) {
                    dictionaries.push(cdic);
                }
            }
            let borrowed: Vec<&[u8]> = dictionaries.iter().map(|c| c.as_slice()).collect();
            Some(HuffCdicReader::new(&table, &borrowed)?)
        } else {
            None
        };

    let mut at = 0usize;
    for i in 1..=mobi.text_record_count as usize {
        let record = read_record(i)?;
        let stripped = strip_trailing_data(&record, mobi.extra_data_flags);
        let decompressed = match mobi.compression {
            Compression::PalmDoc => palmdoc::decompress(stripped)?,
            Compression::Huffman => match huff {
                Some(ref mut reader) => reader.decompress(stripped)?,
                None => stripped.to_vec(),
            },
            Compression::None | Compression::Unknown(_) => stripped.to_vec(),
        };
        let covers = at..at + decompressed.len();
        at = covers.end;
        if !visit(covers, &decompressed) {
            break;
        }
    }
    Ok(at)
}

/// Decode stream bytes by `encoding`. A boundary falling inside a character
/// decodes to U+FFFD.
fn decode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        _ => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uncompressed records with `text_length` set to `declared`.
    fn book(records: &[&str], encoding: Encoding, declared: u32) -> (Vec<Vec<u8>>, MobiHeader) {
        let bytes: Vec<Vec<u8>> = records.iter().map(|r| r.as_bytes().to_vec()).collect();
        let header = MobiHeader {
            compression: Compression::None,
            text_length: declared,
            text_record_count: bytes.len() as u16,
            text_record_size: 10,
            encryption: 0,
            mobi_type: 2,
            encoding,
            mobi_version: 6,
            first_image_index: NULL_INDEX,
            title: String::new(),
            language: 0,
            exth_flags: 0,
            extra_data_flags: 0,
            huff_record_index: NULL_INDEX,
            huff_record_count: 0,
            skel_index: NULL_INDEX,
            div_index: NULL_INDEX,
            oth_index: NULL_INDEX,
            fdst_index: NULL_INDEX,
            fdst_count: 0,
            ncx_index: NULL_INDEX,
            header_length: 0,
        };
        (bytes, header)
    }

    /// Text records are read from index 1; record 0 holds the header.
    fn reader(records: &[Vec<u8>]) -> impl Fn(usize) -> io::Result<Vec<u8>> + '_ {
        move |idx: usize| {
            records
                .get(idx - 1)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such record"))
        }
    }

    #[test]
    fn a_position_returns_the_record_it_lands_in() {
        let (records, mobi) = book(
            &["0123456789", "abcdefghij", "ABCDEFGHIJ"],
            Encoding::Utf8,
            30,
        );
        let slice = axis_slice(reader(&records), &mobi, 15, None).expect("15 is on the axis");
        assert_eq!((slice.from, slice.to), (10, 20));
        assert_eq!(slice.text, "abcdefghij");
        assert_eq!(slice.mark, Some(5));
        assert_eq!(slice.extent, 30);
    }

    #[test]
    fn a_span_takes_only_the_bytes_it_covers() {
        let (records, mobi) = book(
            &["0123456789", "abcdefghij", "ABCDEFGHIJ"],
            Encoding::Utf8,
            30,
        );
        let slice = axis_slice(reader(&records), &mobi, 8, Some(23)).expect("both ends are on it");
        assert_eq!((slice.from, slice.to), (8, 23));
        assert_eq!(
            slice.text, "89abcdefghijABC",
            "the span crosses three records"
        );
        assert_eq!(slice.mark, None, "a span marks nothing");
    }

    /// `text_length` ends the axis, whatever the records hold past it.
    #[test]
    fn the_declared_length_ends_the_axis() {
        let (records, mobi) = book(&["0123456789", "abcdefghij"], Encoding::Utf8, 14);
        let slice = axis_slice(reader(&records), &mobi, 12, Some(20)).expect("12 is on the axis");
        assert_eq!((slice.from, slice.to), (12, 14));
        assert_eq!(slice.text, "cd");

        let past = axis_slice(reader(&records), &mobi, 14, None).expect("past the end");
        assert!(past.text.is_empty());
        assert_eq!(past.extent, 14);
    }

    /// A `text_length` past the end of the records reports `extent` as the
    /// offset the walk reached.
    #[test]
    fn records_ending_early_report_the_end_they_reached() {
        let (records, mobi) = book(&["0123456789"], Encoding::Utf8, 40);
        let slice = axis_slice(reader(&records), &mobi, 25, None).expect("within the declaration");
        assert!(slice.text.is_empty());
        assert_eq!(
            slice.extent, 10,
            "the records stop at 10, not the declared 40"
        );
    }

    #[test]
    fn an_empty_span_reads_nothing() {
        let (records, mobi) = book(&["0123456789"], Encoding::Utf8, 10);
        let slice = axis_slice(reader(&records), &mobi, 4, Some(4)).expect("an empty range");
        assert_eq!((slice.from, slice.to), (4, 4));
        assert!(slice.text.is_empty());
    }

    /// The same bytes read as different text under each `Encoding`.
    #[test]
    fn bytes_decode_by_the_declared_encoding() {
        // 0xE9 is one character in cp1252 and no character at all in UTF-8.
        let raw: Vec<Vec<u8>> = vec![vec![b'a', b'b', 0xE9, b'c', b'd']];
        let (_, cp1252) = book(&["abcde"], Encoding::Cp1252, 5);
        let slice = axis_slice(reader(&raw), &cp1252, 0, Some(5)).expect("the whole record");
        assert_eq!(slice.text, "ab\u{e9}cd");

        let (_, utf8) = book(&["abcde"], Encoding::Utf8, 5);
        let slice = axis_slice(reader(&raw), &utf8, 0, Some(5)).expect("the whole record");
        assert_eq!(slice.text, "ab\u{fffd}cd", "a lone 0xE9 is not UTF-8");
    }

    /// A boundary inside a character loses it from that slice alone.
    #[test]
    fn a_boundary_inside_a_character_decodes_lossily() {
        let raw: Vec<Vec<u8>> = vec!["あい".as_bytes().to_vec()];
        let (_, mobi) = book(&["123456"], Encoding::Utf8, 6);
        let slice = axis_slice(reader(&raw), &mobi, 0, Some(4)).expect("a split range");
        assert_eq!(slice.text, "あ\u{fffd}");
        let whole = axis_slice(reader(&raw), &mobi, 0, Some(6)).expect("the whole record");
        assert_eq!(whole.text, "あい");
    }

    #[test]
    fn the_whole_stream_is_every_record_in_order() {
        let (records, mobi) = book(&["0123456789", "abcdefghij"], Encoding::Utf8, 20);
        assert_eq!(
            whole(reader(&records), &mobi).expect("read"),
            b"0123456789abcdefghij"
        );
    }
}
