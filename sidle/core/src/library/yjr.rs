//! Reading a Kindle's `.sdr` sidecars into the shapes the library works in.

use std::path::Path;

pub use bokai::formats::krds::{Anchor, Annotation, Kind, Object, Store, Value};

/// Annotation records in a `.yjr`.
pub fn parse(bytes: &[u8]) -> Vec<Annotation> {
    match Store::parse(bytes) {
        Ok(store) => store.annotations(),
        Err(e) => {
            eprintln!("[sidle/yjr] unreadable sidecar, skipping its annotations: {e}");
            Vec::new()
        }
    }
}

/// Read and parse a `.yjr` file.
pub fn parse_file(path: &Path) -> std::io::Result<Vec<Annotation>> {
    Ok(parse(&std::fs::read(path)?))
}

/// A named position out of a `.yjf` — `lpr` (last page read) or `fpr` (first).
/// `None` when the key is absent or the sidecar doesn't parse.
pub fn position(bytes: &[u8], key: &str) -> Option<Anchor> {
    Store::parse(bytes).ok()?.position(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a sidecar the way a device would, through the codec, so this seam
    /// is tested against real encoding rather than hand-built bytes.
    fn sidecar(anns: &[Annotation]) -> Vec<u8> {
        let mut store = Store::empty();
        store.merge_annotations(anns);
        store.encode()
    }

    #[test]
    fn reads_annotations_back_out_of_a_sidecar() {
        let bytes = sidecar(&[
            Annotation::highlight(Anchor::new(897, 0, 911), Anchor::new(897, 55, 966), 5, None),
            Annotation::highlight(
                Anchor::new(902, 0, 1586),
                Anchor::new(902, 104, 1690),
                7,
                Some("blue"),
            ),
        ]);
        let anns = parse(&bytes);
        assert_eq!(anns.len(), 2);
        assert_eq!(anns[0].start().unwrap().eid, 897);
        assert_eq!(anns[1].color.as_deref(), Some("blue"));
    }

    #[test]
    fn an_unreadable_sidecar_yields_nothing_rather_than_failing() {
        assert!(parse(b"not a sidecar").is_empty());
        assert!(parse(&[]).is_empty());
        assert!(position(b"junk", "lpr").is_none());
    }

    #[test]
    fn reads_a_last_read_position() {
        let store = Store {
            version: 1,
            roots: vec![Object {
                name: "lpr".into(),
                values: vec![
                    Value::Byte(2),
                    Value::Utf8(Some(Anchor::new(978, 170, 12345).encode())),
                    Value::Long(1),
                ],
            }],
        };
        let bytes = store.encode();
        let lpr = position(&bytes, "lpr").expect("lpr");
        assert_eq!((lpr.eid, lpr.offset, lpr.position), (978, 170, 12345));
        assert!(position(&bytes, "fpr").is_none());
    }
}
