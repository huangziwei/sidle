//! Ion AST + binary parser/writer for the merge pipeline.
//!
//! Mirrors calibre's `ion_binary.py` model: symbols are strings (canonical
//! form `"$N"` for catalog symbols, custom strings for locals), struct field
//! names are strings, annotations are lists of strings. The symbol-string
//! ↔ ID conversion happens at I/O boundaries via the [`LocalSymbolTable`].
//!
//! Only the subset of Ion calibre actually emits in KFX is implemented:
//! null, bool, posint, negint, blob, list, struct, symbol, string,
//! annotation. Floats/decimals/timestamps/clobs/s-exps are not produced by
//! KFX and are not handled here.

use std::io;

use super::symtab::LocalSymbolTable;

pub const ION_BVM: [u8; 4] = [0xe0, 0x01, 0x00, 0xea];

/// Ion AST node, calibre-faithful (string-symbol form).
///
/// `Struct` preserves insertion order via `Vec`, matching calibre's
/// `IonStruct = OrderedDict`. Duplicate field names are allowed at the
/// AST level — calibre logs an error on read but keeps the first value;
/// our parser keeps both entries.
///
/// Float/Decimal/Timestamp are stored as their raw payload bytes
/// (`type_code` is the high nibble of the Ion type descriptor: 4, 5, or 6).
/// We never need to introspect their magnitudes during merge — calibre and
/// Kindle parse them downstream — so round-tripping the wire bytes verbatim
/// is faster and avoids precision drift on the format we don't otherwise use.
#[derive(Debug, Clone)]
pub enum IonNode {
    Null,
    Bool(bool),
    Int(i64),
    Blob(Vec<u8>),
    Symbol(String),
    String(String),
    List(Vec<IonNode>),
    Struct(Vec<(String, IonNode)>),
    Annotated(Vec<String>, Box<IonNode>),
    /// Round-tripped opaque payload for type codes 4 (float), 5 (decimal),
    /// 6 (timestamp). Stored as `(type_code, bytes)`.
    Raw(u8, Vec<u8>),
}

impl IonNode {
    pub fn as_struct(&self) -> Option<&[(String, IonNode)]> {
        match self {
            IonNode::Struct(f) => Some(f),
            _ => None,
        }
    }
    pub fn as_struct_mut(&mut self) -> Option<&mut Vec<(String, IonNode)>> {
        match self {
            IonNode::Struct(f) => Some(f),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[IonNode]> {
        match self {
            IonNode::List(l) => Some(l),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            IonNode::Int(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_string(&self) -> Option<&str> {
        match self {
            IonNode::String(s) => Some(s),
            _ => None,
        }
    }
    #[cfg(test)]
    pub fn as_symbol(&self) -> Option<&str> {
        match self {
            IonNode::Symbol(s) => Some(s),
            _ => None,
        }
    }
    pub fn get_field(&self, key: &str) -> Option<&IonNode> {
        self.as_struct()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

// =========================================================================
// Parser
// =========================================================================

/// Parse a single Ion value (BVM-prefixed) using the given symtab to resolve
/// symbol IDs to their canonical names.
pub fn parse_single_value(data: &[u8], symtab: &LocalSymbolTable) -> io::Result<IonNode> {
    let mut p = Parser { data, pos: 0 };
    p.expect_bvm()?;
    p.parse_value(symtab)
}

/// Parse all values in a multi-value stream (BVM at start, embedded BVMs
/// allowed). Used by calibre for kfxgen-info-style streams (not used here
/// today but kept symmetrical with `serialize_multiple_values`).
#[allow(dead_code)]
pub fn parse_multiple_values(data: &[u8], symtab: &LocalSymbolTable) -> io::Result<Vec<IonNode>> {
    let mut p = Parser { data, pos: 0 };
    p.expect_bvm()?;
    let mut out = Vec::new();
    while p.pos < p.data.len() {
        if p.data[p.pos] == 0xe0 {
            p.expect_bvm()?;
            continue;
        }
        out.push(p.parse_value(symtab)?);
    }
    Ok(out)
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn expect_bvm(&mut self) -> io::Result<()> {
        if self.data.len() < 4 || self.data[..4] != ION_BVM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing Ion BVM",
            ));
        }
        self.pos = 4;
        Ok(())
    }

    fn parse_value(&mut self, symtab: &LocalSymbolTable) -> io::Result<IonNode> {
        if self.pos >= self.data.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Ion EOF mid-value",
            ));
        }
        let td = self.data[self.pos];
        self.pos += 1;
        let type_code = td >> 4;
        let len_code = td & 0x0f;

        if len_code == 15 {
            // null of type_code — we only see type=0 null in practice.
            return Ok(IonNode::Null);
        }

        let len = if len_code == 14 {
            self.read_varuint()? as usize
        } else {
            len_code as usize
        };

        match type_code {
            0 => {
                // NOP pad
                self.pos += len;
                Ok(IonNode::Null)
            }
            1 => Ok(IonNode::Bool(len_code != 0)),
            2 => {
                let v = self.read_uint_be(len)?;
                if v > i64::MAX as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "posint > i64::MAX",
                    ));
                }
                Ok(IonNode::Int(v as i64))
            }
            3 => {
                let v = self.read_uint_be(len)?;
                if v > (i64::MAX as u64) + 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "negint > |i64::MIN|",
                    ));
                }
                if v == (i64::MAX as u64) + 1 {
                    Ok(IonNode::Int(i64::MIN))
                } else {
                    Ok(IonNode::Int(-(v as i64)))
                }
            }
            7 => {
                let id = self.read_uint_be(len)? as u32;
                Ok(IonNode::Symbol(symtab.get_symbol(id)))
            }
            8 => {
                let s = std::str::from_utf8(self.take(len)?)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
                    .to_string();
                Ok(IonNode::String(s))
            }
            9 | 10 => {
                // 9 = clob, 10 = blob — calibre treats both as bytes
                let bytes = self.take(len)?.to_vec();
                Ok(IonNode::Blob(bytes))
            }
            11 | 12 => {
                let end = self.pos + len;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.parse_value(symtab)?);
                }
                Ok(IonNode::List(items))
            }
            13 => {
                let end = self.pos + len;
                let mut fields = Vec::new();
                while self.pos < end {
                    let key_id = self.read_varuint()?;
                    let key = symtab.get_symbol(key_id);
                    let value = self.parse_value(symtab)?;
                    fields.push((key, value));
                }
                Ok(IonNode::Struct(fields))
            }
            14 => {
                let end = self.pos + len;
                let ann_len = self.read_varuint()? as usize;
                let ann_end = self.pos + ann_len;
                let mut anns = Vec::new();
                while self.pos < ann_end {
                    let id = self.read_varuint()?;
                    anns.push(symtab.get_symbol(id));
                }
                let inner = if self.pos < end {
                    self.parse_value(symtab)?
                } else {
                    IonNode::Null
                };
                Ok(IonNode::Annotated(anns, Box::new(inner)))
            }
            4..=6 => {
                let bytes = self.take(len)?.to_vec();
                Ok(IonNode::Raw(type_code, bytes))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reserved Ion type code {}", type_code),
            )),
        }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Ion truncated"));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn read_uint_be(&mut self, n: usize) -> io::Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        if n > 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ion uint > 8 bytes",
            ));
        }
        let mut v = 0u64;
        for &b in self.take(n)? {
            v = (v << 8) | b as u64;
        }
        Ok(v)
    }

    fn read_varuint(&mut self) -> io::Result<u32> {
        let mut v = 0u32;
        loop {
            if self.pos >= self.data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "VarUInt truncated",
                ));
            }
            let b = self.data[self.pos];
            self.pos += 1;
            v = (v << 7) | (b & 0x7f) as u32;
            if b & 0x80 != 0 {
                return Ok(v);
            }
        }
    }
}

// =========================================================================
// Writer
// =========================================================================

/// Serialize a single Ion value (without the BVM prefix). Mirrors calibre's
/// `IonBinary.serialize_value`.
pub fn serialize_value(value: &IonNode, symtab: &LocalSymbolTable) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(&mut out, value, symtab);
    out
}

/// BVM + single value.
pub fn serialize_single_value(value: &IonNode, symtab: &LocalSymbolTable) -> Vec<u8> {
    let mut out = Vec::from(ION_BVM);
    write_value(&mut out, value, symtab);
    out
}

fn write_value(out: &mut Vec<u8>, value: &IonNode, symtab: &LocalSymbolTable) {
    match value {
        IonNode::Null => out.push(0x0f),
        IonNode::Bool(b) => out.push(if *b { 0x11 } else { 0x10 }),
        IonNode::Int(n) => write_int(out, *n),
        IonNode::Blob(data) => write_typed(out, 10, data),
        IonNode::Symbol(name) => write_symbol(out, name, symtab),
        IonNode::String(s) => write_typed(out, 8, s.as_bytes()),
        IonNode::Raw(type_code, payload) => write_typed(out, *type_code, payload),
        IonNode::List(items) => {
            let mut inner = Vec::new();
            for it in items {
                write_value(&mut inner, it, symtab);
            }
            write_typed(out, 11, &inner);
        }
        IonNode::Struct(fields) => {
            let mut inner = Vec::new();
            for (k, v) in fields {
                let kid = symtab.get_id(k);
                write_varuint(&mut inner, kid as u64);
                write_value(&mut inner, v, symtab);
            }
            write_typed(out, 13, &inner);
        }
        IonNode::Annotated(anns, inner_val) => {
            // annot_block = varuint(annot_bytes_len) ++ annot_bytes ++ value_bytes
            let mut ann_buf = Vec::new();
            for ann in anns {
                let id = symtab.get_id(ann);
                write_varuint(&mut ann_buf, id as u64);
            }
            let mut value_buf = Vec::new();
            write_value(&mut value_buf, inner_val, symtab);
            let mut payload = Vec::new();
            write_varuint(&mut payload, ann_buf.len() as u64);
            payload.extend_from_slice(&ann_buf);
            payload.extend_from_slice(&value_buf);
            write_typed(out, 14, &payload);
        }
    }
}

fn write_int(out: &mut Vec<u8>, n: i64) {
    if n == 0 {
        out.push(0x20);
        return;
    }
    let (type_code, mag) = if n >= 0 {
        (2u8, n as u64)
    } else if n == i64::MIN {
        (3u8, 0x8000_0000_0000_0000u64)
    } else {
        (3u8, (-n) as u64)
    };
    let bytes = uint_be_minimal(mag);
    write_type_descriptor(out, type_code, bytes.len());
    out.extend_from_slice(&bytes);
}

fn write_symbol(out: &mut Vec<u8>, name: &str, symtab: &LocalSymbolTable) {
    let id = symtab.get_id(name);
    if id == 0 {
        // Calibre raises an exception for undefined-on-serialize; we
        // emit symbol ID 0 to match the wire behavior of calibre's
        // get_id (which returns 0 for unknown).
        out.push(0x70);
        return;
    }
    let bytes = uint_be_minimal(id as u64);
    write_type_descriptor(out, 7, bytes.len());
    out.extend_from_slice(&bytes);
}

fn write_typed(out: &mut Vec<u8>, type_code: u8, data: &[u8]) {
    write_type_descriptor(out, type_code, data.len());
    out.extend_from_slice(data);
}

fn write_type_descriptor(out: &mut Vec<u8>, type_code: u8, len: usize) {
    if len < 14 {
        out.push((type_code << 4) | (len as u8));
    } else {
        out.push((type_code << 4) | 14);
        write_varuint(out, len as u64);
    }
}

pub(crate) fn write_varuint(out: &mut Vec<u8>, v: u64) {
    if v == 0 {
        out.push(0x80);
        return;
    }
    let mut groups: Vec<u8> = Vec::new();
    let mut t = v;
    while t > 0 {
        groups.push((t & 0x7f) as u8);
        t >>= 7;
    }
    let last = groups.len() - 1;
    for (i, g) in groups.iter().rev().enumerate() {
        if i == last {
            out.push(g | 0x80);
        } else {
            out.push(*g);
        }
    }
}

fn uint_be_minimal(v: u64) -> Vec<u8> {
    if v == 0 {
        return vec![];
    }
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count();
    bytes[skip..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_struct_with_symbol() {
        let mut t = LocalSymbolTable::new();
        t.import_shared_symbol_table("YJ_symbols", 10, 823);
        let local_id = t.create_local_symbol("my_local");
        assert_eq!(local_id, 833);

        let v = IonNode::Struct(vec![
            ("$258".into(), IonNode::Symbol("my_local".into())),
            ("max_id".into(), IonNode::Int(42)),
            ("$165".into(), IonNode::String("resource/rsrc55Z".into())),
        ]);
        let bytes = serialize_single_value(&v, &t);
        let back = parse_single_value(&bytes, &t).unwrap();
        assert_eq!(
            back.get_field("$258").and_then(|n| n.as_symbol()),
            Some("my_local")
        );
        assert_eq!(back.get_field("max_id").and_then(|n| n.as_int()), Some(42));
        assert_eq!(
            back.get_field("$165").and_then(|n| n.as_string()),
            Some("resource/rsrc55Z")
        );
    }

    #[test]
    fn roundtrip_annotated_symtab_fragment() {
        let mut t = LocalSymbolTable::new();
        t.import_shared_symbol_table("YJ_symbols", 10, 823);
        let v = IonNode::Annotated(
            vec!["$ion_symbol_table".into()],
            Box::new(IonNode::Struct(vec![(
                "symbols".into(),
                IonNode::List(vec![IonNode::String("a".into()), IonNode::String("b".into())]),
            )])),
        );
        let bytes = serialize_single_value(&v, &t);
        let back = parse_single_value(&bytes, &t).unwrap();
        if let IonNode::Annotated(anns, inner) = back {
            assert_eq!(anns, vec!["$ion_symbol_table"]);
            let strs: Vec<&str> = inner
                .get_field("symbols")
                .and_then(|l| l.as_list())
                .unwrap()
                .iter()
                .filter_map(|n| n.as_string())
                .collect();
            assert_eq!(strs, vec!["a", "b"]);
        } else {
            panic!("not annotated");
        }
    }
}
