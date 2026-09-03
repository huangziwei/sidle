//! `KfxPackage` — a KFX container parsed into entities, editable one at a time.
//! [`parse`](KfxPackage::parse) splits off the passthrough sections and the
//! entity list; `into_bytes` re-serializes, copying untouched entities verbatim.

use crate::formats::kfx::container::{
    self, EntityLoc, SymbolTable, get_field, parse_container_header, parse_index_table, slice_at,
};
use crate::formats::kfx::error::KfxError;
use crate::formats::kfx::ion::{IonParser, IonValue, IonWriter};
use crate::formats::kfx::serialization::{
    SerializedEntity, create_entity_data, create_raw_media_data, serialize_container,
};
use crate::formats::kfx::symbols::KfxSymbol;

/// Ion symbol-table field ids, from the Ion 1.0 system symbols. A doc-symbols
/// section is `$ion_symbol_table::{$6: imports, $7: symbols, $8: max_id}`;
/// interning touches the last two and leaves the imports verbatim.
mod symtab_field {
    /// `symbols`, the local symbol strings in id order.
    pub const SYMBOLS: u64 = 7;
    /// `max_id`, the highest symbol id the table defines, imports included.
    pub const MAX_ID: u64 = 8;
}

/// One entity: its index-table identity plus the ENTY-wrapped bytes backing it.
#[derive(Clone)]
pub struct Entity {
    id: u32,
    type_id: u32,
    data: Vec<u8>,
}

impl Entity {
    /// The entity id — the fragment-name symbol id. Resolve it through
    /// [`KfxPackage::symbols`].
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The fragment type id (e.g. `KfxSymbol::ExternalResource as u32`).
    #[inline]
    pub fn type_id(&self) -> u32 {
        self.type_id
    }

    /// True if this entity has the given KFX type.
    #[inline]
    pub fn is_type(&self, t: KfxSymbol) -> bool {
        self.type_id == t as u32
    }

    /// The full ENTY-wrapped bytes, verbatim.
    #[inline]
    pub fn raw(&self) -> &[u8] {
        &self.data
    }

    /// The payload past the ENTY header — raw media bytes for
    /// `bcRawMedia`/`bcRawFont`, the Ion body for everything else.
    #[inline]
    pub fn media(&self) -> &[u8] {
        container::skip_enty_header(&self.data)
    }

    /// True for the two types whose payload is a media file stored without Ion
    /// encoding.
    #[inline]
    pub fn is_raw_media(&self) -> bool {
        self.type_id == KfxSymbol::Bcrawmedia as u32 || self.type_id == KfxSymbol::Bcrawfont as u32
    }

    /// The payload as an Ion value. Errors on a raw-media entity;
    /// [`media`](Self::media) returns those bytes.
    pub fn parse_ion(&self) -> Result<IonValue, KfxError> {
        IonParser::new(self.media())
            .parse()
            .map_err(|e| KfxError::InvalidKfx(format!("parse entity {}: {e}", self.id)))
    }
}

/// A parsed KFX container.
pub struct KfxPackage {
    container_id: String,
    /// The doc-symbol Ion section, verbatim unless [`intern`](Self::intern)
    /// rewrites it.
    symtab_ion: Vec<u8>,
    /// The format-capabilities Ion section, verbatim.
    format_caps_ion: Vec<u8>,
    symbols: SymbolTable,
    entities: Vec<Entity>,
}

impl KfxPackage {
    /// Parse a container in memory.
    pub fn parse(kfx_bytes: &[u8]) -> Result<Self, KfxError> {
        let layout = parse_layout(kfx_bytes)?;
        let sect = |sec: Option<(usize, usize)>| -> &[u8] {
            sec.and_then(|(o, l)| slice_at(kfx_bytes, o, l))
                .unwrap_or(&[])
        };
        let symtab_ion = sect(layout.symtab);
        let symbols = SymbolTable::from_fragment((!symtab_ion.is_empty()).then_some(symtab_ion));

        let mut entities = Vec::with_capacity(layout.entities.len());
        for e in &layout.entities {
            let raw = slice_at(kfx_bytes, e.offset, e.length)
                .ok_or_else(|| KfxError::InvalidKfx("entity payload out of bounds".into()))?;
            entities.push(Entity {
                id: e.id,
                type_id: e.type_id,
                data: raw.to_vec(),
            });
        }

        Ok(Self {
            container_id: layout.container_id,
            symtab_ion: symtab_ion.to_vec(),
            format_caps_ion: sect(layout.format_caps).to_vec(),
            symbols,
            entities,
        })
    }

    /// The container id (`bcContId`, `CR!…`).
    #[inline]
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// The resolved symbol table — base import plus this container's own
    /// doc symbols.
    #[inline]
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Every entity, in index-table order.
    #[inline]
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// An entity's fragment name, or `""` when the id resolves to nothing.
    pub fn name_of(&self, entity: &Entity) -> &str {
        self.symbols.resolve_opt(entity.id as u64).unwrap_or("")
    }

    /// The position of the entity with this type and name, if any.
    pub fn find(&self, type_id: u32, name: &str) -> Option<usize> {
        self.entities
            .iter()
            .position(|e| e.type_id == type_id && self.name_of(e) == name)
    }

    /// Every position holding an entity of this type, in index-table order.
    pub fn positions_of_type(&self, type_id: u32) -> Vec<usize> {
        self.entities
            .iter()
            .enumerate()
            .filter(|(_, e)| e.type_id == type_id)
            .map(|(i, _)| i)
            .collect()
    }

    /// Replace an entity's payload with a fresh Ion value.
    pub fn set_ion(&mut self, at: usize, value: &IonValue) -> Result<(), KfxError> {
        self.entity_mut(at)?.data = create_entity_data(value);
        Ok(())
    }

    /// Replace a raw-media entity's payload (an image or a font).
    pub fn set_media(&mut self, at: usize, bytes: &[u8]) -> Result<(), KfxError> {
        self.entity_mut(at)?.data = create_raw_media_data(bytes);
        Ok(())
    }

    /// Replace an entity with ENTY-wrapped bytes, framing included.
    pub fn set_raw(&mut self, at: usize, framed: Vec<u8>) -> Result<(), KfxError> {
        self.entity_mut(at)?.data = framed;
        Ok(())
    }

    /// Append an Ion entity, returning its position.
    pub fn push_ion(&mut self, type_id: u32, id: u32, value: &IonValue) -> usize {
        self.entities.push(Entity {
            id,
            type_id,
            data: create_entity_data(value),
        });
        self.entities.len() - 1
    }

    /// Append a raw-media entity, returning its position.
    pub fn push_media(&mut self, type_id: u32, id: u32, bytes: &[u8]) -> usize {
        self.entities.push(Entity {
            id,
            type_id,
            data: create_raw_media_data(bytes),
        });
        self.entities.len() - 1
    }

    /// Drop an entity. Later positions shift down by one.
    pub fn remove(&mut self, at: usize) -> Result<Entity, KfxError> {
        if at >= self.entities.len() {
            return Err(KfxError::InvalidKfx(format!("no entity at {at}")));
        }
        Ok(self.entities.remove(at))
    }

    /// The symbol id for `name`, appending a doc symbol when the table has none.
    /// Rewrites the doc-symbol section in place, keeping its imports verbatim.
    pub fn intern(&mut self, name: &str) -> Result<u32, KfxError> {
        if let Some(id) = container::symbol_id_for_name(name) {
            return Ok(id as u32);
        }
        if let Some(id) = self.symbols.local_symbol_id(name) {
            return Ok(id as u32);
        }

        let mut table = IonParser::new(&self.symtab_ion)
            .parse()
            .map_err(|e| KfxError::InvalidKfx(format!("parse doc symbols: {e}")))?;
        let annotations = match &table {
            IonValue::Annotated(a, _) => a.clone(),
            _ => vec![3], // $ion_symbol_table
        };
        let IonValue::Struct(fields) = table.unwrap_annotated().clone() else {
            return Err(KfxError::InvalidKfx("doc symbols is not a struct".into()));
        };
        let mut fields = fields;

        let mut symbols =
            match get_field(&fields, symtab_field::SYMBOLS).and_then(IonValue::as_list) {
                Some(list) => list.to_vec(),
                None => Vec::new(),
            };
        symbols.push(IonValue::String(name.to_string()));
        let count = symbols.len();
        set_field(&mut fields, symtab_field::SYMBOLS, IonValue::List(symbols));

        // The table's own `max_id` counts imports plus locals, so it moves with
        // every symbol appended.
        let base_len = self.symbols.base_len();
        set_field(
            &mut fields,
            symtab_field::MAX_ID,
            IonValue::Int((base_len - 1 + count as u64) as i64),
        );

        table = IonValue::Struct(fields);
        let mut writer = IonWriter::new();
        writer.write_bvm();
        writer.write_annotated(&annotations, &table);
        self.symtab_ion = writer.into_bytes();

        let id = base_len + count as u64 - 1;
        self.symbols = SymbolTable::from_fragment(Some(&self.symtab_ion));
        Ok(id as u32)
    }

    /// Re-serialize the container.
    pub fn into_bytes(self) -> Vec<u8> {
        let entities: Vec<SerializedEntity> = self
            .entities
            .into_iter()
            .map(|e| SerializedEntity {
                id: e.id,
                entity_type: e.type_id,
                data: e.data,
            })
            .collect();
        serialize_container(
            &self.container_id,
            &entities,
            &self.symtab_ion,
            &self.format_caps_ion,
        )
    }

    fn entity_mut(&mut self, at: usize) -> Result<&mut Entity, KfxError> {
        self.entities
            .get_mut(at)
            .ok_or_else(|| KfxError::InvalidKfx(format!("no entity at {at}")))
    }
}

/// Set a struct field, appending it when absent.
fn set_field(fields: &mut Vec<(u64, IonValue)>, key: u64, value: IonValue) {
    match fields.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => fields.push((key, value)),
    }
}

/// The passthrough sections and entity table of a parsed container.
pub(crate) struct ContainerLayout {
    pub container_id: String,
    /// `(offset, length)` of the doc-symbol Ion section, if declared.
    pub symtab: Option<(usize, usize)>,
    /// `(offset, length)` of the format-capabilities Ion section, if declared.
    pub format_caps: Option<(usize, usize)>,
    pub entities: Vec<EntityLoc>,
}

/// Walk the container header + container-info to recover the passthrough
/// section ranges, the container id, and the entity index table.
pub(crate) fn parse_layout(kfx_bytes: &[u8]) -> Result<ContainerLayout, KfxError> {
    let header =
        parse_container_header(kfx_bytes).map_err(|e| KfxError::InvalidKfx(e.to_string()))?;
    let ci_bytes = slice_at(
        kfx_bytes,
        header.container_info_offset,
        header.container_info_length,
    )
    .ok_or_else(|| KfxError::InvalidKfx("container info out of bounds".into()))?;
    let ci_fields = {
        let mut p = IonParser::new(ci_bytes);
        p.parse()
            .ok()
            .and_then(|v| v.as_struct().map(<[_]>::to_vec))
            .ok_or_else(|| KfxError::InvalidKfx("container info is not a struct".into()))?
    };
    let geti = |sym: KfxSymbol| {
        get_field(&ci_fields, sym as u64)
            .and_then(IonValue::as_int)
            .map(|n| n as usize)
    };
    let section = |off: KfxSymbol, len: KfxSymbol| geti(off).zip(geti(len));

    let idx_off = geti(KfxSymbol::Bcindextaboffset)
        .ok_or_else(|| KfxError::InvalidKfx("no index table offset".into()))?;
    let idx_len = geti(KfxSymbol::Bcindextablength)
        .ok_or_else(|| KfxError::InvalidKfx("no index table length".into()))?;
    let container_id = get_field(&ci_fields, KfxSymbol::Bccontid as u64)
        .and_then(IonValue::as_string)
        .unwrap_or("")
        .to_string();
    let symtab = section(KfxSymbol::Bcdocsymboloffset, KfxSymbol::Bcdocsymbollength);
    let format_caps = section(
        KfxSymbol::Bcfcapabilitiesoffset,
        KfxSymbol::Bcfcapabilitieslength,
    );

    let index_bytes = slice_at(kfx_bytes, idx_off, idx_len)
        .ok_or_else(|| KfxError::InvalidKfx("index table out of bounds".into()))?;
    let entities = parse_index_table(index_bytes, header.header_len);

    Ok(ContainerLayout {
        container_id,
        symtab,
        format_caps,
        entities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";

    fn fixture() -> Vec<u8> {
        std::fs::read(FIXTURE).expect("read fixture")
    }

    /// Parse → `into_bytes` with nothing touched reproduces a container that
    /// holds the same entities, in the same order, with the same bytes.
    #[test]
    fn untouched_round_trip_keeps_every_entity() {
        let kfx = fixture();
        let before = KfxPackage::parse(&kfx).expect("parse");
        let ids: Vec<(u32, u32)> = before
            .entities()
            .iter()
            .map(|e| (e.id(), e.type_id()))
            .collect();
        let bodies: Vec<Vec<u8>> = before.entities().iter().map(|e| e.raw().to_vec()).collect();

        let out = before.into_bytes();
        let after = KfxPackage::parse(&out).expect("re-parse");

        assert_eq!(
            ids,
            after
                .entities()
                .iter()
                .map(|e| (e.id(), e.type_id()))
                .collect::<Vec<_>>(),
            "entity identity and order preserved"
        );
        for (i, e) in after.entities().iter().enumerate() {
            assert_eq!(bodies[i], e.raw(), "entity {i} byte-identical");
        }
    }

    /// A name already in the base table interns to its base id without touching
    /// the doc symbols; a fresh name appends one and resolves back.
    #[test]
    fn intern_reuses_base_and_appends_local() {
        let kfx = fixture();
        let mut pkg = KfxPackage::parse(&kfx).expect("parse");

        let base = pkg.intern("content").expect("intern base symbol");
        assert!(
            (base as u64) < pkg.symbols().base_len(),
            "a base symbol keeps its base id"
        );

        let symtab_before = pkg.symtab_ion.clone();
        let fresh = pkg.intern("bokai_test_symbol").expect("intern new symbol");
        assert!((fresh as u64) >= pkg.symbols().base_len(), "a local id");
        assert_eq!(pkg.symbols().resolve(fresh as u64), "bokai_test_symbol");
        assert_ne!(symtab_before, pkg.symtab_ion, "doc symbols rewritten");

        // Interning it again is idempotent.
        assert_eq!(pkg.intern("bokai_test_symbol").expect("re-intern"), fresh);

        // And it survives a serialize/parse cycle.
        let out = pkg.into_bytes();
        let after = KfxPackage::parse(&out).expect("re-parse");
        assert_eq!(after.symbols().resolve(fresh as u64), "bokai_test_symbol");
    }

    #[test]
    fn non_kfx_bytes_error() {
        assert!(KfxPackage::parse(b"not a kfx container").is_err());
    }
}
