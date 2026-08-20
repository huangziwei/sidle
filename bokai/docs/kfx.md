# KFX: Kindle Format 10 Structure and Semantics (according to Claude)

- Draft 0.1
- **Status** — Unofficial. Not endorsed by or affiliated with Amazon.
- **Scope** — Container, encoding, and content model. 

## Contents

1. §1 — Introduction
2. §2 — Distribution forms
3. §3 — Container layer
4. §4 — Ion encoding layer
5. §5 — Symbols
6. §6 — Fragment layer
7. §7 — Document model
8. §8 — Style model
9. §9 — Navigation
10. §10 — Reading positions
11. §11 — Resources
12. §12 — Fixed layout
13. §13 — Metadata
14. §14 — Conformance
15. §A — Fragment type registry
16. §B — Worked example
17. §C — What is not established

## 1. Introduction

KFX is the container and document format used by current Kindle devices and applications. It has no published specification. This document is an attempt at one.

### 1.1. Scope

This specification describes how a KFX file is laid out on disk, how its structured data is encoded, and what the encoded structures mean as a book: reading order, text content, styling, navigation, reading positions, images, and metadata. It is written for implementers building readers, converters, validators, or analysis tools.

### 1.2. Relationship to other Kindle formats

KFX is also called *KF10*, the tenth Kindle format generation, and appears internally under the codename *YJ* ("Yellow Jersey") — a name that surfaces directly in the format's shared symbol table, `YJ_symbols`. It supersedes KF8/AZW3, which was a PalmDB-derived container wrapping compressed HTML. KFX shares almost nothing with it: different container, different encoding, different content model. A KFX file is not an archive of markup documents. It is a graph of typed records, and the reading system assembles a book by traversing that graph.

### 1.3. Conformance and terminology

The key words **must**, **must not**, **should**, and **may** are to be interpreted as described in RFC 2119. They apply to implementations, not to Amazon's own producers.

Because the format is defined by numeric symbol identifiers rather than names, this document writes a symbol as `$260` when the number matters and as `section` when the name does. The two are interchangeable: `$260` *is* `section`. Ion values are shown in Ion text notation throughout, though KFX only ever stores binary Ion.

## 2. Distribution forms

A "KFX book" reaches an implementation in one of several packagings. All of them ultimately contain the same container format described in §3.

### 2.1. Monolithic container

A single file, conventionally `.kfx`, holding one container with every fragment the book needs. This is the form this specification describes normatively; the others reduce to it.

### 2.2. Multi-container bundles

Amazon's own distribution splits a book across several containers, delivered either as a directory or as a ZIP archive conventionally named `.kfx-zip`. A bundle typically separates the main content container from a resource container and a metadata container. Each member is an independent, well-formed container with its own symbol table and its own `$419` `container_entity_map`.

To read a bundle, an implementation **must** merge its members into one fragment set. Merging is not concatenation: each container numbers its local symbols independently, so fragment names **must** be resolved to text in each source container and re-interned against a single merged symbol table. The merged container carries exactly one `container_info` and one rebuilt `container_entity_map`.

### 2.3. KDF and KPF

Kindle Previewer and Kindle Create produce an intermediate called *KDF*, packaged as *KPF*. KDF is an SQLite database whose rows hold Ion-encoded fragments — the same fragment vocabulary described here, before container serialisation. It is an authoring intermediate, not a distribution format, and is not specified in this document.

KDF is not confined to desktop authoring. The Scribe carries an on-device converter that turns a side-loaded PDF into a KFX book, writing KDF as its intermediate — the SDK opens it through an SQLite VFS, and the resulting book is handled as a distinct class from a downloaded one. Handwritten annotation over a side-loaded PDF is anchored to that KFX layer, so the converter tracks the pen rather than a hardware generation: a Kindle without a pen carries no converter, newer models included. Where a device converts at all it converts PDF and nothing else; a side-loaded EPUB is never converted.

### 2.4. Encryption

Retail containers signal encryption through `bcDRMScheme` in the container info and a per-entity equivalent in each entity header. A value of `0` in both means no encryption, and every structure in this document is directly readable. Non-zero values indicate a DRM scheme whose key derivation is device-bound and outside this specification's scope. An implementation encountering a non-zero scheme **should** report the file as encrypted rather than attempting to parse the payload.

## 3. Container layer

The outermost layer is a flat, offset-indexed record store. It knows nothing about books: it holds numbered, typed byte ranges and a table telling you where each one begins.

### 3.1. Container header

A container begins with a fixed 18-byte header. All multi-byte integers in the container layer are little-endian and unsigned.

```
"CONT" [0..3]  |  version [4..5]  |  header_len [6..9]  |  info_offset [10..13]  |  info_length [14..17]
```

`header_len` is the size of the whole header region — everything before the first entity payload — and is the base to which entity offsets are relative. `version` is `2`.

An implementation **must** reject a file whose first four bytes are not `CONT`, and **must** treat a file shorter than 18 bytes as malformed.

### 3.2. Container info

At `info_offset` for `info_length` bytes sits a binary Ion struct — the container's directory. Its fields are keyed by symbols from the shared table (§5), so it is readable before any document-local symbols are known.

```
{
  bcContId:               "CR!2V5GMJ5B652W7ED0CNV1210FAXAR",
  bcComprType:            0,
  bcDRMScheme:            0,
  bcChunkSize:            4096,
  bcIndexTabOffset:       18,     // entity index table
  bcIndexTabLength:       2712,
  bcDocSymbolOffset:      2730,   // document-local symbol table
  bcDocSymbolLength:      680,
  bcFCapabilitiesOffset:  3410,   // format capabilities
  bcFCapabilitiesLength:  96
}
```

*Container info fields*

| Symbol | Field | Meaning |
|---|---|---|
| $409 | bcContId | Container identity, `CR!` followed by 28 uppercase alphanumerics. Referenced by `container_entity_map` and echoed in metadata as `asset_id`. |
| $410 | bcComprType | Compression scheme for entity payloads. `0` = none. |
| $411 | bcDRMScheme | Encryption scheme. `0` = none. |
| $412 | bcChunkSize | Chunking granularity, `4096`. No effect on parsing an unencrypted, uncompressed container. |
| $413/$414 | bcIndexTab… | Absolute offset and byte length of the entity index table (§3.3). |
| $415/$416 | bcDocSymbol… | Absolute offset and length of the document-local symbol table (§5.3). |
| $594/$595 | bcFCapabilities… | Absolute offset and length of a `$593` `format_capabilities` Ion value declaring the reader features the file requires. |

> **Note**
>
> These offsets are absolute file offsets, unlike entity offsets in the index table, which are relative to `header_len`. Mixing the two is the most common early parsing error.

### 3.3. Entity index table

The index table is a packed array of fixed 24-byte entries with no count prefix; the entry count is `bcIndexTabLength / 24`.

```
id : u32 [0..3]  |  type : u32 [4..7]  |  offset : u64 [8..15]  |  length : u64 [16..23]
```

`offset` is relative to the container's `header_len`; the absolute position of the entity is `header_len + offset`.

The two symbol identifiers carry the entity's whole identity. `type` names the *kind* of record — `$260` for a section, `$259` for a storyline — and is always drawn from the shared symbol table. `id` names the *instance*, and is normally a document-local symbol: resolving it yields the fragment's name, such as `c0` or `content_14`. Singleton fragments, of which a book has at most one, use the reserved id `$348` (`null`) instead of a name.

### 3.4. Entity wrapper

Each byte range identified by the index table begins with its own small header before the payload proper.

```
"ENTY" [0..3]  |  ver [4..5]  |  header_len [6..9]  |  Ion: {bcComprType, bcDRMScheme} [10..20]
```

The per-entity compression and encryption fields are an Ion struct, so the header's length is variable and given by `header_len`; it is 21 bytes when both are `0`. The payload begins at `header_len` bytes into the entity.

Implementations **should** tolerate an entity that does not begin with `ENTY` by treating the whole range as payload; some containers store small entities unwrapped.

### 3.5. Generator trailer

Between the container info and the first entity payload, retail containers carry a short *Ion text* list — not binary — recording the producing toolchain:

```
[{key:"kfxgen_package_version",value:""},
 {key:"kfxgen_application_version",value:"..."},
 {key:"kfxgen_payload_sha1",value:"<40 hex>"},
 {key:"kfxgen_acr",value:"CR!..."}]
```

`kfxgen_payload_sha1` is the SHA-1 of the concatenated entity payload region. Nothing in the reading system reads it, and the same holds for `kfxgen_acr` and `kfxgen_application_version`. The trailer is **provenance, not validation**: a record of the producing toolchain, addressed to Amazon's own pipeline rather than to the reading device. A producer **should** still emit a genuine digest, since what the delivery pipeline does with it is unknown (§C.3), but an implementation **must not** expect a device to reject a container over it, and a reader gains nothing by verifying it.

### 3.6. Reading algorithm

A conforming reader processes a container in this order:

1. Read and validate the 18-byte header; retain `header_len`.
2. Parse the container info at its absolute offset.
3. Parse the document-local symbol table and construct the resolver (§5.4). This **must** precede any entity parsing, because entity identity depends on it.
4. Parse the index table into entries.
5. For each entry: slice `header_len + offset` for `length`, skip the `ENTY` header, and interpret the payload — as raw bytes for media types (§11.2), otherwise as binary Ion.
6. Group the parsed fragments by `type`, keyed within each type by the resolved `id`.

The result is the fragment graph of §6, from which everything else is derived.

## 4. Ion encoding layer

### 4.1. Binary Ion 1.0

Every structured payload in a KFX container is Amazon Ion 1.0 in its binary encoding, beginning with the four-byte version marker `E0 01 00 EA`. Ion is fully specified and public; this section states only which parts KFX exercises and where KFX's use of it is unusual.

A value is a one-byte descriptor — high nibble the type code, low nibble the length — followed by the body. Length nibble `14` means the length follows as a VarUInt; `15` means the value is null.

*Ion type codes and their use in KFX*

| Code | Type | Use in KFX |
|---|---|---|
| 0 | null / NOP | Padding. Skipped. |
| 1 | bool | Feature flags, metadata values. |
| 2 / 3 | int | Element ids, positions, lengths, pixel dimensions. Ubiquitous. |
| 4 | float | Rare; most fractional values are decimals. |
| 5 | decimal | Style magnitudes — `line_height: 0.974286`. Coefficient plus exponent; must not be flattened to binary float by a producer intending byte-identical output. |
| 6 | timestamp | Unused in content fragments. |
| 7 | symbol | The backbone: every name, type, enumerated value and cross-reference. |
| 8 | string | Book text, labels, metadata values. |
| 9 / 10 | clob / blob | Rare. Media does not travel as blobs — see §11.2. |
| 11 | list | Content lists, entry lists, everything ordered. |
| 12 | sexp | Page-template conditions, e.g. `(isPortrait)`. |
| 13 | struct | Every record. Field keys are symbol ids, and field order is significant for byte-identical reproduction though not for meaning. |
| 14 | annotation | Type tagging — see §4.2. |

### 4.2. Annotations as type tags

KFX uses Ion annotations to name a struct's type inline, independently of the index table's `type` field. A navigation container appears in the stream as `nav_container::{ … }` and a navigation entry as `nav_unit::{ … }`. Readers **must** unwrap annotations transparently when reading fields, and **should not** require an annotation to be present: the same logical structure appears annotated in some containers and bare in others. Where an annotation carries information that no other field does, this document says so explicitly.

### 4.3. Notation

Ion text notation in this document is illustrative. A KFX container never stores Ion text except in the generator trailer (§3.5). Where a field is shown by name — `story_name: lV` — the encoded form is a symbol-id field key with a symbol-id value, and both sides require the resolver of §5.

## 5. Symbols

KFX stores almost no names. It stores numbers that index into tables, and getting the table boundary wrong silently produces a plausible, wrong book.

### 5.1. The shared table

The format's entire vocabulary — every field name, every fragment type, every enumerated value such as `vertical_rl` or `justify` — lives in a shared Ion symbol table named *`YJ_symbols`*, imported by every container. It is not embedded in the file. An implementation **must** ship its own copy.

Symbol ids 1–9 are the Ion system symbols (`$ion`, `$ion_symbol_table`, `name`, `version`, `imports`, `symbols`, `max_id`, …). From id 10 the YJ vocabulary begins, with `$10` = `language`, `$11` = `font_family`, and so on.

Amazon does not publish the table. It ships on every Kindle, inside the SDK that encodes and decodes containers, as one NUL-separated run of names in id order, and independent reconstructions of it circulate publicly.

### 5.2. Revisions

Many revisions of `YJ_symbols` are in circulation at once. A book keeps whatever revision its producer had, forever, and declares it as the import `max_id` in its own symbol table; declared sizes span at least 626 through 859. A reading device carries a revision of its own, which may be older or newer than any file it opens.

**The table is append-only.** Each revision is a strict prefix of every later one: differences are a contiguous block at the tail, and no id below that block changes meaning. Ids `$828`–`$833` added `rendition_flow`, `continue_rendition_flow`, `scrollable`, `paginated`, `standalone_entities` and `document_regions`; `$834`–`$851` added a vector-shape block — `ellipse`, `rectangle`, `line`, `polygon`, `polyline`, `shape_dimensions`, `vertex_list` and their geometry fields.

Two consequences follow, and together they are the whole argument for §5.4.

**An id below a revision's `max_id` means the same thing in every revision.** A reader therefore needs no knowledge of *which* revision a file was built against beyond the boundary itself. It does not need every table. It needs the newest table it can obtain, and the file's declared boundary to know where the shared ids stop.

**The declared import is a local-symbol base and nothing more.** It is not a supported-version check. Amazon routinely delivers a container declaring a revision newer than the receiving device carries, including revisions past every table any shipping reader holds, and such a book opens and renders normally. An implementation **must not** reject a container for declaring a revision it does not recognise.

Appending is also what lets one build of a title serve every generation of hardware. A reader with an older table meets no renumbered id, and the constructs it has no name for are ones its own era's books do not contain.

The vocabulary a book *uses* tracks its content, not any device. The highest shared id a trade book names is `$756` `ruby_content`, in Japanese books with ruby; a book without ruby stops at `$621` `yj.location_pid_map`, and one without a location map at `$597` `auxiliary_data`. The declared revision predicts none of this: books declaring an import in the 600s already name `$621`, and books declaring in the 850s still name nothing above `$756`. The tail of the table — everything past `$757` — names constructs (vector shapes, document regions, scrollable and paginated renditions) that trade books do not contain.

### 5.3. Document-local symbols

Everything specific to one book — section names, storyline names, style names, anchor names, resource names — is a document-local symbol. These live in the container's own symbol table, located by `bcDocSymbolOffset`, encoded as a standard Ion local symbol table:

```
$ion_symbol_table::{
  max_id: 881,
  imports: [ { name: "YJ_symbols", version: 10, max_id: 785 } ],
  symbols: [ "c0","c9","cR","c16","c2A","c5S","cGP","c179","c18J",
             "d1A9","n19W", /* … 96 in this file … */ ]
}
```

Local names are generated, not authored, and follow a producer convention: a type-letter prefix plus a compact counter — `c…` for sections, `l…` for storylines, `s…` for styles, `a…` for anchors, `e…` for external resources, `b…` for ruby content, `d…` for auxiliary data. Implementations **must not** depend on this; it is a habit, not a rule, and names such as `content_14` or a bare UUID also occur.

### 5.4. Resolution

This is the single most error-prone rule in the format.

The first document-local symbol id equals **one plus the sum of the declared import `max_id` values**. In the table above, the sole import declares `max_id: 785`, so local symbols occupy ids 786 and upward, and `symbols[0]` — `"c0"` — is id 786.

Note what this implies: the import's `max_id` is the *highest symbol id it occupies*, counting from 1 and subsuming the nine Ion system ids — not a count of the symbols it contributes. Resolution is therefore:

```
resolve(id):
    if id < local_base:   YJ_symbols[id]        // shared table
    else:                 local_symbols[id - local_base]
```

> **Interoperability trap**
>
> The local base **must** be read from the file's declared `max_id`. It **must not** be assumed equal to the length of the implementation's own copy of `YJ_symbols`.
>
> Containers are built against whichever revision of the shared table their producer had. A file declaring `max_id: 785` read by an implementation carrying 852 entries will, if it seats local symbols at 852, resolve every local id into the shared table's tail instead. The failure is silent and superficially plausible: section names come back as real words like `snap_block` or `end_x`, the book still opens, and only the structure is wrong. Nothing in the file is malformed, so no validator catches it; the symptom is structural names that read as style vocabulary.

The same rule governs writing. A producer emitting local symbols **must** declare an import `max_id` matching the shared-table revision it actually used, and **must not** renumber shared ids.

## 6. Fragment layer

### 6.1. Fragments

Each entity in the container is a *fragment*: a typed, named Ion value. Its type is the index entry's `type` symbol; its name is the resolved `id` symbol. Fragments reference each other *by name*, never by container offset, which is what makes multi-container distribution (§2.2) possible — a fragment does not know or care which container its referent lives in.

A fragment type is either *singleton* — at most one per book, addressed by type alone, carrying the reserved id `$348` — or *named*, with one instance per local symbol. `document_data` and `book_metadata` are singletons; `section`, `storyline` and `style` are named.

> **Note**
>
> A name is unique per type, not globally. A section and its position map legitimately share the name `c0`, as do a storyline and its ruby content. An implementation keying fragments by name alone will lose records; the key **must** be the pair `(type, name)`.

### 6.2. Container entity map

`$419` `container_entity_map` lists, per container id, the fragments that container holds. In a monolithic file it is redundant with the index table. In a bundle it is the manifest a reader uses to know what to expect, and merging (§2.2) requires rebuilding it. Its dependency-ordering fields are unresolved (§C.2).

### 6.3. Fragment ordering

Retail containers order entities by a stable type ranking — styles, then storylines, then sections, then maps, then resources — rather than by name or dependency. Nothing requires this ordering for correctness, and readers **must not** depend on it. Producers aiming for output resembling Amazon's **should** adopt it.

A complete inventory of fragment types is given in §A.

## 7. Document model

A book is assembled by descending five levels: document → reading order → section → page template → storyline → content element → text.

### 7.1. Document data

`$538` `document_data` is the root. It carries book-wide layout defaults and, critically, the reading orders.

```
{
  column_count:  auto,
  direction:     ltr,
  font_size:     { value: 1,   unit: em },
  line_height:   { value: 1.2, unit: em },
  writing_mode:  vertical_rl,
  selection:     enabled,
  max_id:        1354,           // highest element id in the book
  reading_orders: [
    { reading_order_name: default,
      sections: [ c0, c9, cR, c16, c2A, c5S, cGP, c179, c18J ] }
  ]
}
```

`max_id` here is the element-id ceiling (§7.5), unrelated to symbol table `max_id`. The two namespaces overlap numerically and must not be conflated.

> **Note**
>
> Older containers carry `reading_orders` in the flat `$258` `metadata` fragment instead. A reader **should** consult `document_data` first and fall back to `metadata`, taking the first fragment type that yields any order.

### 7.2. Reading orders and sections

A *reading order* is a named sequence of section names — the spine. Books normally carry one, named `default`. Multiple orders are structurally permitted; how a device chooses between them is unknown (§C.1).

`$260` `section` is a spine item. It contributes no content of its own; it lists `$141` `page_templates`, each of which names the storyline that fills it.

```
{ section_name: c0,
  page_templates: [
    { id: 857, story_name: l4, type: container,
      fixed_width: 1098, fixed_height: 1648,
      layout: scale_fit, float: center }
  ] }
```

For reflowable text a page template is minimal — `{ id, story_name, type: text }`. The dimensional and layout fields above appear on fixed-layout pages (§12) and on cover pages, where `layout: scale_fit` asks the reader to fit an image to the viewport.

### 7.3. Storylines

`$259` `storyline` holds the content tree. Its `$146` `content_list` is an ordered list of content elements, each of which may nest a further `content_list`.

```
{ story_name: lD,
  content_list: [
    { id: 888, style: sH, type: text,
      content_list: [
        { id: 894, style: sF, type: text,
          word_boundary_list: [0, 4],
          style_events: [ { offset: 0, length: 4, style: s1A8 } ],
          content: { name: content_1, index: 0 } } ] } ] }
```

Storylines may reference other storylines through `$176` `story_name`. Traversal **must** be cycle-guarded.

### 7.4. Content elements

Every node in a content list is a struct with a small common shape:

- **$155 id** — The *element id* (eid). Unique per book, and the coordinate every position and anchor addresses (§10).
- **$159 type** — A symbol naming the element kind — `text`, `container`, `image`, `table`, `list` and others. Absent means `container`.
- **$157 style** — Name of a `$157` `style` fragment applied to the whole element (§8).
- **$146 content_list** — Child elements, in document order.
- **$145 content** — The element's own text (§7.5). Mutually exclusive with children in practice.
- **$142 style_events** — Ranged inline styling over that text (§8.4).
- **$696 word_boundary_list** — Offsets at which words begin, used for selection and hyphenation. Present chiefly where whitespace does not delimit words, as in Japanese.
- **$175 resource_name** — On `image` elements, the external resource to draw (§11).
- **$179 link_to** — An anchor name; the element is a hyperlink (§9.3).
- **$761 layout_hints** — Semantic hints such as `heading`, `figure`, `caption` that survive the loss of markup structure.

A bare Ion string inside a `content_list` is a literal inline text run, interleaved with sibling structs — the shape produced when a paragraph embeds an inline image.

The element vocabulary in use is ten types: `text`, `image`, `table_row`, `container`, `horizontal_rule`, `listitem`, `table`, `body`, `list` and `header`. A `type: text` holding block children is how a `<div>` of paragraphs is expressed — KFX has no div/paragraph distinction — and a `type: text` wrapping a single image is the `<p><img></p>` shape. `caption` and `figure` arrive as `layout_hints`, never as types.

### 7.5. Text content

Text is not stored inside the storyline. The `$145` `content` field is a reference:

```
content: { name: content_1, index: 0 }
```

meaning entry `0` of the `$145` `content` fragment named `content_1`, itself a fragment whose `content_list` is a flat list of strings. Text is thus pooled into a handful of large fragments — a couple of dozen in a typical novel, each around 8 KB — rather than scattered across the tree. A `content` field **may** alternatively hold a literal string; readers **must** accept both.

The string reached this way is the element's *base text*. It is the exact substrate that positions and annotations index into: an offset in an anchor or a highlight counts characters in this string, before any styling, ruby, or generated content is applied. A newline inside a content string is a line break; there is no discrete line-break element.

### 7.6. Tables

The table model is flatter than HTML's: there is a table element and a row element, and **no cell element at all**.

```
{ type: table, id: 3419,
  column_format: [ { sizing_bounds: content_bounds,
                     width: { unit: percent, value: 40. } },
                   { is_empty: false } ],   // placeholder — holds column 2's place
  content_list: [
    { id: 3418, type: body,              // or an untyped group; `header` for a thead
      content_list: [
        { type: table_row, id: 3339,
          content_list: [
            { type: text, id: 3337, style: …, content: {…} },   // cell
            { type: text, id: 3338, style: … } ] },             // empty cell
        … ] } ] }
```

A row's children *are* its cells, positionally. They carry no cell-specific type and no cell-specific fields beyond the ordinary element vocabulary of §7.4 plus `$156` `layout`: a cell uses exactly `content`, `content_list`, `id`, `layout`, `style`, `style_events`, `type` and `word_boundary_list`.

Two consequences follow from the missing cell element, and both are traps.

**A cell's span is a style property, not an element field.** Because a cell has no element of its own to hang attributes on, `$148` `table_column_span` and `$149` `table_row_span` live in the `$157` `style` the cell references, alongside its padding and alignment:

```
{ style_name: s1VY,          // the style a spanning cell references
  table_column_span: 2,
  yj.vertical_align: center,
  padding_top: { value: 0.03125, unit: lh }, … }
```

A reader that scans only element fields for a span will find none anywhere and silently produce a mis-shaped grid.

**Header rows are declared structurally.** A table's rows may be grouped under `type: header` and `type: body` sections — the `<thead>`/`<tbody>` analogue. When they are absent the row list is ungrouped and nothing marks a header. In particular `important_cells` is *not* a header mechanism (§C.2).

*Table vocabulary on `table` and `table_row` elements*

| Field | Meaning and values |
|---|---|
| column_format | The table's column geometry, one entry per column, read **positionally** — the `<colgroup>` analogue. An entry states `width` (`{ value: 40, unit: percent }`), optionally `sizing_bounds` (`content_bounds`), and optionally `$118` `column_span` to cover several columns at once. A column with nothing to say still holds its place, spelled `{ is_empty: false }`. Note this `column_span` belongs to the *column entry*, not to a cell; it is unrelated to `table_column_span`. |
| important_cells | A short list of `[row, column]` coordinate pairs, never more than about ten per table however large the table. Not a header marker; purpose unresolved (§C.2). |
| yj.table_features | Reader affordances: `[pan_zoom, scale_fit]`, or `[yj.disable_stacking]`, which suppresses the device's habit of restacking a wide table into vertical blocks on a small screen. |
| yj.table_selection_mode | `yj.regional` — selection is by rectangular region rather than by reading order. |
| table_border_collapse | As CSS `border-collapse`. |
| border_spacing_horizontal / _vertical | The two halves of CSS `border-spacing`. |
| fit_width | Boolean; fit the table to the available measure. |

A converter to HTML **must** synthesise `<td>` boundaries from row-child position; **must** read each cell's span from the style it references, not from its element; and **must** emit one `<col>` per `column_format` entry including the empty ones, since dropping a placeholder slides every later width one column to the left. Header cells **may** be recovered from a `type: header` section where one exists; nothing else in the fragment distinguishes a header.

### 7.7. Lists

A list is a `type: list` containing `type: listitem` children. Beyond the ordinary element fields, list elements carry only `$100` `list_style`, `$102` `list_indent`, `$104` `list_start_offset` and `$551` `list_style_position` — all direct CSS analogues.

Ordering is not a separate element type. Whether a list is ordered is decided by `list_style`: the alphabetic, roman and decimal values make an ordered list, and everything else — including KFX's own `numeric`, whose numbering rides the style rather than the structure — is unordered.

## 8. Style model

### 8.1. Style fragments

`$157` `style` fragments are a deduplicated stylesheet. Each is a flat struct of property symbols; there is no inheritance mechanism in the format beyond the cascade the reader applies from ancestor elements.

```
{ font_size:      { value: 1.2, unit: rem },
  line_height:    { value: 0.982286, unit: lh },
  outline_weight: { value: 0, unit: pt },
  baseline_style: normal,
  font_weight:    bold,
  style_name:     s1A5 }
```

Property names map closely — often exactly — onto CSS: `font_size`, `text_alignment`, `margin_top`, `letterspacing`, `list_style`, `border_color_left`. The mapping is mechanical enough that a converter can be driven by a table rather than by code.

Three properties are easy to overlook. `box_align` positions a *box*, not text (§8.3). `baseline_style` is where superscript and subscript live — dropping it flattens footnote markers and ordinals. `link_unvisited_style` and `link_visited_style` each hold a nested style struct for a link state, an inline stylesheet-within-a-stylesheet with no single-property CSS equivalent.

### 8.2. Length values

A dimensional property is a struct of magnitude and unit, not a string: `{ value: 1.2, unit: rem }`. The declarable vocabulary is the CSS one — `ch`, `cm`, `em`, `ex`, `in`, `lh`, `mm`, `percent`, `pt`, `px`, `rem`, `vh`, `vmax`, `vmin`, `vw` — with `percent` spelled out because `%` is not a symbol. Magnitudes are Ion decimals; a reader **must not** assume integer. Some fields accept a bare integer as shorthand for pixels; readers **should** accept both forms.

Six of those units carry essentially all real styling:

*Units in use*

| Unit | Role |
|---|---|
| lh | Multiples of the document line height (§7.1). The dominant vertical unit. |
| em | Relative to the element's own type size — indents, spacing, tracking. |
| percent | Relative to the containing block. How a box is sized. |
| rem | Relative to the root type size. Nearly every `font_size`. |
| pt | The one absolute unit in reflowable use. |
| px | Device dots on a fixed-layout page canvas. |

**An absolute length is a point read at 160 dpi.** A CSS pixel is `0.45pt` in this file format — `72/160`, the pixel of the Android density baseline the firmware inherits, rather than the `1/96 in` pixel of CSS. `border-width: 1px` is `border_weight: { value: 0.45, unit: pt }`; point magnitudes accordingly fall on multiples of `0.225pt`, half a pixel, with `0.9` (2px) and `0.45` (1px) the common values. The `max_width` caps Amazon's image-layout pass applies — `216`, `270`, `324`, `360pt` — are 480, 600, 720 and 800 pixels on the same scale.

**`px` belongs to fixed layout**, where it carries the `top`, `left`, `width` and `height` of a page canvas measured in device dots, and the `font_size` of text laid over that canvas. Reflowable content does not use it.

> **Unit trap**
>
> A `px` length on a reflowable element does not produce the size CSS asks for. An image styled `width: 220px` occupies 220 device dots, not the 220 CSS pixels an HTML renderer would give it — on a 300 dpi panel roughly a third of the intended width. A converter from CSS **should** scale absolute lengths by 0.45 and emit `pt`. `font_size` is the exception and stays in `rem`, which is why type tracks the reader's font slider and box geometry does not.

### 8.3. Box placement

`box_align` positions a box within its container; `text_alignment` positions text within the box. They are separate properties and a style may carry either, both or neither — collapsing the first onto the second gets both wrong at once, losing the box's placement and centring text the source never centred.

**`box_align` is a picture property.** It rides `image` elements, alongside a `percent` or `em` `width` and a `max_width` cap. It reaches other element types rarely, and on a `type: text` it has no effect: a text element carrying `width: 75%` and `box_align: center` lays out 75% wide and *flush to the inline start*. The width is honoured; the alignment is not.

**A text block is narrowed and placed by its margins**, not by a width. `margin_left` and `margin_right` are how a block that occupies part of its container's measure states the leftover. In vertical Japanese typesetting the block-start side is the right, and `margin_right` carries the indent.

> **Placement trap**
>
> KFX has no `auto` margin, so CSS `margin: auto` cannot be carried across as written. A producer **should** resolve the split before emitting: for a block of width `w` percent, write `margin_left` and `margin_right` of `(100 − w) / 2` percent beside the width. Emitting `box_align: center` instead leaves the block flush to the inline start.

### 8.4. Style events

Inline styling does not nest elements. A run of differently styled text within a paragraph is expressed as a *style event* — a range over the element's base text:

```
style_events: [ { offset: 0, length: 4, style: s1A8 } ]
```

Offsets and lengths count characters of the base text (§7.5). Events may overlap. This is the format's substitute for `<em>`, `<strong>` and `<span>`, and a converter to markup **must** reconstruct nesting from ranges, splitting where they cross.

Block-level properties do not ride a style event. `box_align` in particular is consumed on block elements only.

### 8.5. Typography: hyphenation, kerning, ligatures

KFX's public reputation rests on "Enhanced Typesetting" — hyphenation, kerning and ligatures, the things KF8/AZW3 could not do. Where those live is not symmetrical.

*Typography vocabulary*

| Sym | Property | Role |
|---|---|---|
| $127 | hyphens | `auto`, `none`. Maps to CSS `hyphens`. The one typography control books set. |
| $32 | letterspacing | A length. Tracking, not kerning — an author-set offset applied on top of the font's own pair adjustments. |
| $33 | wordspacing | A length. |
| $128 | min_hyphen_word_length | Shortest word eligible for hyphenation. Value grammar unknown (§C.2). |
| $268 | hyphen_dictionary | Names a hyphenation dictionary. Value grammar unknown (§C.2). |
| $18 | ot_features | OpenType feature selection. Value grammar unknown (§C.2). |
| $562 | ligatures | Value grammar unknown (§C.2). |
| $563 | kerning | Value grammar unknown (§C.2). |

The format can name kerning, ligatures and OpenType features, and books do not set them. That reflects the division of labour:

- **Kerning and ligatures are the renderer's, not the file's.** Enhanced Typesetting is a property of the reading system's layout engine and the fonts it ships. The engine applies pair kerning and standard ligatures because it is a real text shaper working with real OpenType fonts — no book has to ask. The `ligatures` and `kerning` properties exist to *suppress or override* that default.
- **Hyphenation is genuinely shared.** The engine carries the dictionaries; the book decides where hyphenation is allowed, per style. This is the one place the format meaningfully participates.
- **The capability gate is elsewhere.** Whether a book is eligible for Enhanced Typesetting at all is declared in `content_features` (§12.1), not in any style property — the `reflow-style` key is the marker of a book converted through the modern pipeline.

> **Note**
>
> The practical consequence for a converter is small but real: `hyphens` **must** be carried across, because it is authored data. The absence of `kerning` and `ligatures` costs nothing to translate, because the target renderer makes the same default choice for the same reason.

### 8.6. Writing modes

`$560` `writing_mode` takes `horizontal_tb`, `vertical_rl` or `vertical_lr`, matching CSS. It appears on `document_data` as the book default and may be overridden per element. `horizontal_tb` is the CSS initial value, so horizontal books declare nothing and a per-element override is the only writing-mode declaration such a book carries.

Vertical Japanese typesetting is a first-class case in KFX, not an add-on, and vertical books also exercise `word_boundary_list` and ruby.

A `type: container`'s `$156` `layout` is the block-progression axis of its *children*, keyed to the box's own resolved writing mode: vertical writing runs block flow horizontally, so its boxes take `layout: horizontal`, and `horizontal_tb` text takes `layout: vertical`. `box_align` cannot repair a wrong axis — the symptom of one is a box inflated to full block width with its content pinned to the inline start.

### 8.7. Ruby

Ruby annotations — furigana — live in their own `$756` `ruby_content` fragments rather than inline. A style event carrying `$757` `ruby_name` and `$758` `ruby_id` selects annotation number `ruby_id` from the named fragment's ordered list. Ids are one-based.

## 9. Navigation

### 9.1. Book navigation

`$389` `book_navigation` holds one entry per reading order, each listing `$392` `nav_containers`.

```
[ { reading_order_name: default,
    nav_containers: [
      nav_container::{ nav_type: landmarks, nav_container_name: n19W,
        entries: [
          nav_unit::{ landmark_type: cover_page,
                      representation: { label: "cover-nav-unit" },
                      target_position: { id: 857, offset: 0 } },
          nav_unit::{ landmark_type: toc,
                      representation: { label: "目次" },
                      target_position: { id: 858, offset: 0 } } ] },
      nav_container::{ nav_type: toc, nav_container_name: sS8,
        entries: [
          nav_unit::{ representation: { label: "はしがき" },
                      target_position: { id: 849, offset: 0 } } ] } ] } ]
```

A nav container appears in two forms: inline, as above, or as a bare symbol naming a separate `$391` `nav_container` fragment. Readers **must** handle both — the referenced form is required by some devices for fixed-layout and personal- document books.

### 9.2. Navigation types

*nav_type values*

| Value | Role |
|---|---|
| toc | Table of contents. Entries may nest to form a hierarchy. |
| landmarks | Structural landmarks. Each entry carries `$238` `landmark_type` — `cover_page`, `toc`, `srl` (start reading location), and others. |
| page_list | Print-edition page numbers mapped to positions; the source of "page 214 of 300". |
| headings | Heading positions with levels, letting a reader reconstruct heading rank that the content tree does not carry directly. |

A `$393` `nav_unit` carries an optional `$241` `representation` holding the display `$244` `label`, and a `$246` `target_position`. A unit whose representation is empty is still meaningful — the `srl` landmark commonly has no label.

### 9.3. Anchors

`$266` `anchor` fragments name positions so that links can target them.

```
{ anchor_name: a19X, position: { id: 849, offset: 0 } }
```

An anchor either carries a `position` — an internal target — or a `$186` `uri`, making it an external link target. An element's `$179` `link_to` names an anchor, and the reader resolves it to a position or a URI accordingly.

Several anchors **may** share one position. A converter assigning identifiers **must** therefore map every co-located anchor name onto the *same* generated identifier, or links naming the non-first anchor at a position will dangle.

### 9.4. Positions as coordinates

Every navigational target is the pair `{ id, offset }`: an element id and a character offset into that element's base text (§7.5). This one coordinate system addresses table-of-contents entries, anchors, landmarks, bookmarks and highlights alike.

## 10. Reading positions

The "Location 1,247" a Kindle displays is the end of a three-stage chain: element id → position id → location.

### 10.1. Element id to position id

`$265` `position_id_map` maps element ids to *position ids* (pids), a monotonic fine-grained scale over the whole book. In reflowable books it is a direct list of `{eid, pid}` pairs.

### 10.2. Section position maps

In fixed-layout books the same fragment instead partitions the book into per-section spans:

```
{ contains: [ { section_name: c0,  pid: 0,    length: 2 },
              { section_name: c9,  pid: 2,    length: 12 },
              { section_name: cR,  pid: 14,   length: 39 },
              { section_name: c16, pid: 53,   length: 1898 } ] }
```

and each section carries a `$609` `section_position_id_map` holding a compact delta-encoded walk. Its `contains` list mixes two element shapes:

- `[advance, eid]` — advance the running pid, then assign it to `eid`. An entry whose `eid` is `0` terminates the walk, and the pid then equals the section's length.
- `advance` alone — the element id is the previous one plus one. This is the compression: consecutive element ids, which are the common case, cost one integer each.

A reader replays each section's walk starting from that section's base pid to rebuild the same `eid → pid` mapping the reflowable form gives directly.

### 10.3. Locations

Pids are too fine to show a reader. `$550` `location_map` divides the book into displayed Locations by listing boundary positions as `{id, offset}` pairs, resolved through the pid map. `$621` `yj.location_pid_map` is the newer alternative, listing boundary pids directly and needing no resolution. Where both are present, `$550` takes precedence.

> **Note**
>
> A book with positions but no location map is not positionless: the device spaces Locations evenly at a fixed interval of **110 pids per Location**. An implementation reporting raw pids as Locations will inflate every number by roughly two orders of magnitude and will fail to match positions synced from a device.

### 10.4. Stability

Position ids are assigned at conversion time and are stable for a given file, which is what allows highlights to survive re-download. They are not stable across Amazon re-issuing a title. Nothing in the format records a position-map version, so an implementation cannot detect that a book's positions have been renumbered.

## 11. Resources

### 11.1. External resources

`$164` `external_resource` describes a media object without containing it.

```
{ resource_name:   e6,
  format:          jpg,
  mime:            "image/jpg",
  location:        "resource/rsrc7",
  resource_width:  333,
  resource_height: 500 }
```

`resource_name` is what image elements reference; `location` is the key of the fragment holding the bytes. Declared dimensions **should** be treated as advisory and verified against the decoded image.

`$479` `background_image` is a symbol naming an `external_resource` rather than a URL, and the reader draws it.

### 11.2. Raw media

The bytes live in `$417` `bcRawMedia` fragments, and fonts in `$418` `bcRawFont`. These are the one exception to "every payload is Ion": the payload after the `ENTY` header is the media file verbatim. A parser that assumes Ion will fail on exactly these entities, and **must** branch on the type before parsing.

The fragment's name is its `location` string, such as `resource/rsrc7`, resolved from the index entry's `id` symbol like any other name.

### 11.3. Image formats

`format` values are `jpg`, `png`, `gif`, `webp`, `bmp`, `svg`, `jxr`, plus `pdf` and `kvg` for non-raster page content. The declared format is not always truthful; a reader **should** sniff magic bytes and prefer the sniffed result.

What a Kindle can decode is a fixed and modest set. Two codecs are separate libraries — **JPEG** (both a baseline and a SIMD implementation) and **JPEG XR**, which has a dedicated library of its own rather than riding the graphics stack. The rest come from the bundled Skia build: **PNG**, **WebP**, **GIF**, **BMP** (including its mask and RLE variants), **ICO** and **WBMP**. There is no HEIF, AVIF or camera-raw support, and PDF page content goes to a separate rendering library rather than to an image codec. This set is a floor on what a producer may safely emit: a raster format outside it is a risk on every current device.

`jxr` is **JPEG XR** (ITU-T T.832), wrapped in a TIFF-style container. Amazon adopted it for its compression ratio at Kindle's bit depths. It is decodable but poorly supported outside Microsoft's ecosystem, so any converter targeting general readers **must** transcode it — and **should** pass the original through unchanged rather than failing if decoding does not succeed.

### 11.4. Fonts

Embedded fonts appear as `$262` `font` fragments, with the bytes in a `bcRawFont` blob named by `location`, exactly as images are named by `external_resource`. Typefaces never travel as `bcRawMedia`.

```
{ font_family:  "cover-Charis",
  font_style:   italic,
  font_weight:  bold,
  font_stretch: normal,
  location:     "resource/rsrcPWZ" }
```

The descriptor is a direct CSS `@font-face` analogue: five fields, no more. One fragment describes one face, so a family shipped in four weights is four fragments sharing a `font_family` and differing in `font_style` / `font_weight`.

Subsetting is visible in the naming rather than declared in a field. Families such as `cover-Charis`, `cover-Roboto-Medium` and `Kafk_9780307829481_epub_cvi_r1-Charis` show the producer prefixing the family with its use site, so the same typeface used on a cover and in body text becomes two independently subsetted faces under two family names. A converter **must** preserve the mangled family name, since the style fragments reference it verbatim.

A book's `font_family` may instead be a stack headed by `default` — the name of whatever face the reader has chosen. A named stack pins the typeface; a `default`-headed one defers to the reader's setting.

## 12. Fixed layout

### 12.1. Feature signalling

`$585` `content_features` declares the reader capabilities a book requires, as namespaced key/version triples:

```
{ features: [
    { namespace: "SDK.Marker", key: "CanonicalFormat",
      version_info: { version: { major_version: 2, minor_version: 0 } } },
    { namespace: "com.amazon.yjconversion", key: "jpvertical-reflow-language",
      version_info: { version: { major_version: 6, minor_version: 0 } } } ] }
```

Two keys change how a book must be interpreted structurally. Any key containing `fixed_layout` — `yj_fixed_layout`, `yj_non_pdf_fixed_layout` — puts the book on the fixed-layout path. `yj_double_page_spread` marks a spread comic, where facing pages pair. A book **may** carry several `content_features` fragments, one standalone and one nested in metadata; a reader **must** take the union.

Two namespaces exist: `com.amazon.yjconversion`, and `SDK.Marker`, whose only key is `CanonicalFormat`.

*content_features keys*

| Key | Meaning |
|---|---|
| reflow-style | Reflowable text through the modern pipeline. The practical marker of an Enhanced Typesetting book (§8.5). |
| CanonicalFormat | Carries a major/minor version — the container generation, at 2.0. |
| yj_jpegxr_sd | Images are JPEG XR (§11.3). |
| jp-reflow-language | Japanese text handling. |
| jpvertical-reflow-language | Japanese vertical writing. |
| yj_table | The book contains tables (§7.6). |
| reflow-section-size | Section-size constraints on reflow. |
| yj_jpg_rst_marker_present | JPEG restart markers present — a decoder hint. Declared inconsistently: a book carrying it may have JPEGs with no restart marker. |
| yj_table_viewer | Tables get the interactive viewer; pairs with `yj.table_features: [pan_zoom, scale_fit]`. |
| tcn-reflow-language | Traditional Chinese. |
| yj_thumbnails_present | Page thumbnails bundled. |
| yj_non_pdf_fixed_layout | Image-based fixed layout (§12.3). |
| yj_double_page_spread | Facing-page spreads. Co-occurs with the previous key. |
| yj_ruby | Ruby annotations (§8.7). Not a reliable ruby detector — many books carry `ruby_content` without declaring it. |
| yj_hdv | A build-time declaration no reader consults (§12.2). What it declares is unknown (§C.2). |
| yj_mixed_writing_mode | Horizontal and vertical text in one book. |
| cn-reflow-language | Simplified Chinese. |

### 12.2. Which component acts on a feature

A key's presence in a file does not mean anything acts on it. Three components of the reading system act on three different sets.

*content_features keys by consuming component*

| Component | Keys it names |
|---|---|
| **Layout engine** reads | device_converted, nested_span, graphical_highlights, continuous_popup_progression, periodicals_generation_V2, yj_facing_page, yj_double_page_spread, yj_guided_view_native, yj_illustrated_layout, yj_has_text_popups, yj_publisher_panels, yj_textbook, yj_fixed_layout, yj_forced_continuous_scroll, yj_forced_continuous_scroll_v1 |
| **Reader application** reads | yj_audio, yj_video, yj_interactive_image, yj_mixed_writing_mode, yj_scroll_capability, yj_fixed_layout, yj_double_page_spread, yj_publisher_panels, yj_textbook, yj_asin |
| **On-device converter** writes | yj_arabic_fixed_format, yj_audio, yj_custom_word_iterator, yj_direction_rtl, yj_double_page_spread, yj_fixed_layout, yj_graphical_highlights, yj_hd_support, yj_hdv, yj_interactive_image, yj_jpegxr_sd, yj_jpg_rst_marker_present, yj_pdf_links, yj_pdf_support, yj_publisher_panels, yj_rotated_pages, yj_textbook, yj_thumbnails_present, yj_video, yj_webview |

The two reader vocabularies are stable across firmware generations.

**Some keys are declarations no reader consults.** `yj_hdv`, `yj_jpegxr_sd` and `yj_hd_support` appear only on the writing side. They describe how the file was built, addressed to the delivery pipeline, and a reading implementation gains nothing by honouring them.

`yj_table` and `yj_table_viewer` are read by nothing either: the table viewer keys on the element's own `yj.table_features` instead.

> **Note**
>
> The asymmetry cuts both ways for a producer. A key no reader names is inert however faithfully it mirrors Amazon's own output; a key the reader names but books rarely carry — `yj_mixed_writing_mode` — is nonetheless read on every device, and is the correct signal for the case it describes.

### 12.3. Pages and spreads

In a fixed-layout book each section's page template carries `$66` `fixed_width` and `$67` `fixed_height` in pixels, defining the viewport. A `$156` `layout` value of `page_spread` or `facing_page` marks a container whose storyline holds the two per-page containers rather than page content itself. `scale_fit` asks for the content to be fitted to the viewport.

### 12.4. PDF-backed books

Some KFX files carry a PDF as the rendered content, with the KFX layer supplying navigation, positions and text geometry over it. These are identifiable by an `external_resource` whose `format` is `pdf` and whose section templates reference it. Cover detection **must** exclude them: their first section renders a PDF page, not a cover image.

The KFX layer over a PDF supplies one section per page, a section list and its navigation units, a text layer with each resource's dimensions stamped onto its container, and anchors derived from the PDF's link annotations — per URI, per section, and per container — with a baseline for each text container and vector paths attached to style events where the source drew them.

Two rules a reading or converting implementation also needs:

- **Spread detection is validated, not assumed.** Candidate pages qualify as a facing-page spread only if they come from the same source PDF and agree on height and rotation.
- **A page's image container is sized by stamping the resource's own dimensions onto it**, then verifying the storyline's scale-fit container against them, rather than by trusting a declared viewport.

## 13. Metadata

### 13.1. Categorised metadata

The current shape is `$490` `book_metadata`, holding `$491` `categorised_metadata` — a list of categories, each a list of key/value pairs. The category that matters is `kindle_title_metadata`.

```
{ categorised_metadata: [
    { category: kindle_title_metadata,
      metadata: [
        { key: "title",               value: "人間失格" },
        { key: "title_pronunciation", value: "にんげんしっかく" },
        { key: "author",              value: "太宰 治" },
        { key: "language",            value: "ja" },
        { key: "issue_date",          value: "2012-09-27" },
        { key: "ASIN",                value: "B009IXASIE" },
        { key: "cde_content_type",    value: "PDOC" },
        { key: "is_sample",           value: false },
        { key: "asset_id",            value: "CR!2V5GMJ5B652W7ED0CNV1210FAXAR" } ] } ] }
```

Keys are Ion strings, not symbols. Repeated keys are meaningful: `author` appears once per author in order, and `author_pronunciation` appears positionally alongside. Sort keys for Japanese titles ride in `title_pronunciation`.

There is no series field. A book's membership in a series is not recorded anywhere in the format; only the title string hints at it.

### 13.2. Flat metadata

Older containers carry metadata in `$258` `metadata` as a plain struct keyed by symbol id — `$424` `cover_image`, `$222` `author`, and so on. A reader **should** read `$490` first and consult `$258` only for fields still unset. The cover in particular is often declared *only* in the flat fragment, and a reader that skips the fallback will report a coverless book.

### 13.3. Cover resolution

The cover is named by `cover_image`, whose value may be a string, a bare symbol, or a single-element list — all three occur, and all resolve to an `external_resource` name. Where no `cover_image` exists at all, the cover **may** be recovered by walking the first section of the first reading order to its storyline's first `resource_name`, accepting it only if it names a raster image resource.

## 14. Conformance

### 14.1. Conforming reader

A conforming reader **must**:

- Validate the `CONT` signature and reject non-KFX input.
- Resolve symbols using the container's declared import `max_id`, never its own table length (§5.4).
- Accept a declared import `max_id` higher than its own table (§5.2). The declared value locates the local-symbol base; it is not a supported-version check.
- Key fragments by `(type, name)`, not name alone (§6.1).
- Treat `bcRawMedia` and `bcRawFont` payloads as opaque bytes, not Ion (§11.2).
- Unwrap Ion annotations transparently and not require their presence (§4.2).
- Accept both the inline and referenced forms of a nav container (§9.1).
- Accept both the reflowable and section-partitioned forms of the position map (§10).
- Report a container with a non-zero DRM scheme as encrypted rather than mis-parsing it (§2.4).

A conforming reader **should** tolerate unknown symbol ids, unknown fragment types, and unknown style properties by ignoring them, rather than failing. The format is extended by appending vocabulary (§5.2), and strictness here breaks forward compatibility for no benefit.

A reader carrying its own `YJ_symbols` **should** carry the newest revision it can obtain rather than attempting to match a file's revision. Because the table is append-only, one sufficiently new table resolves every id in every older file correctly; matching revisions per file buys nothing.

### 14.2. Conforming producer

A conforming producer **must** emit a symbol table whose declared import `max_id` matches the shared-table revision actually used, **must** assign every content element a unique element id and record the ceiling in `document_data`'s `max_id`, and **must** emit a genuine SHA-1 in the generator trailer (§3.5).

A producer **should** emit a position map. A book without one has no addressable reading positions: no Locations, no syncing, no highlights.

A producer converting from CSS **should not** emit a `px` length in a reflowable book. An absolute length is a point read at 160 dpi — scale by `0.45` and emit `pt` — and `px` is the fixed-layout canvas unit (§8.2). It **should** likewise resolve an `auto` horizontal margin into the split it stands for and state that as `margin_left` and `margin_right`: `box_align` is a picture property and does not place a text block (§8.3).

A producer **should not** attempt to bound the symbol ids it emits by any device's table. The ids a book names follow from the constructs it contains; a producer that needs a construct emits the symbol that names it (§5.2).

## A. Fragment type registry

Ids are symbols in the shared table; "cardinality" distinguishes singletons from named fragments (§6.1).

*Fragment types*

| Type | Name | Cardinality | Role |
|---|---|---|---|
| $145 | content | named | Pooled text strings, addressed by index (§7.5). |
| $157 | style | named | Deduplicated style declarations (§8.1). |
| $164 | external_resource | named | Media descriptor (§11.1). |
| $258 | metadata | singleton | Legacy flat metadata; also a fallback home for reading orders (§13.2). |
| $259 | storyline | named | Content tree (§7.3). |
| $260 | section | named | Spine item with page templates (§7.2). |
| $262 | font | named | Embedded font descriptor (§11.4). |
| $264 | position_map | singleton | Section-level position summary. |
| $265 | position_id_map | singleton | Element id to position id (§10.1). |
| $266 | anchor | named | Named position or external URI (§9.3). |
| $270 | container_info | singleton | The container directory (§3.2). |
| $389 | book_navigation | singleton | TOC, landmarks, page list (§9.1). |
| $391 | nav_container | named | Referenced navigation container (§9.1). |
| $395 | resource_path | singleton | Base path for resource locations. |
| $417 | bcRawMedia | named | Verbatim media bytes (§11.2). |
| $418 | bcRawFont | named | Verbatim font bytes (§11.2). |
| $419 | container_entity_map | singleton | Per-container fragment manifest (§6.2). |
| $490 | book_metadata | singleton | Categorised metadata (§13.1). |
| $538 | document_data | singleton | Root: defaults and reading orders (§7.1). |
| $550 | location_map | singleton | Location boundaries by position (§10.3). |
| $585 | content_features | singleton | Required reader capabilities (§12.1). |
| $593 | format_capabilities | singleton | Container-level capability declaration (§3.2). |
| $597 | auxiliary_data | named | Per-section key/value data, e.g. `IS_TARGET_SECTION`. |
| $608 | structure | — | Named by the shared vocabulary; no fragment of this type is known. Listed so implementers do not go looking for it (§C.1). |
| $609 | section_position_id_map | named | Delta-encoded position walk (§10.2). |
| $621 | yj.location_pid_map | singleton | Location boundaries by pid (§10.3). |
| $756 | ruby_content | named | Ruby annotation text (§8.7). |

## B. Worked example

A 465 KB Japanese novel of 113 entities decomposes as follows. The proportions are typical: text dominates, structure is small, and the position machinery costs more than the section definitions.

*Entity census of one container*

| Type | Count | Bytes | Notes |
|---|---|---|---|
| content | 26 | 218 KB | The book's text, pooled. |
| storyline | 9 | 168 KB | One per section; structure plus style references. |
| bcRawMedia | 1 | 31.6 KB | The cover image. |
| location_map | 1 | 16.8 KB | 1,703 Locations. |
| ruby_content | 5 | 13.0 KB | Furigana. |
| yj.location_pid_map | 1 | 5.2 KB | Redundant with the above. |
| section_position_id_map | 9 | 2.6 KB | Delta-encoded walks. |
| style | 27 | 2.2 KB | The whole stylesheet. |
| auxiliary_data | 8 | 787 B | Navigation-target markers. |
| book_metadata | 1 | 650 B | |
| section | 9 | 495 B | 55 bytes each — sections are pure indirection. |
| book_navigation | 1 | 439 B | 3 landmarks, 7 TOC entries. |
| anchor | 7 | 301 B | |
| document_data | 1 | 138 B | The root of everything. |

### B.1. Resolving one sentence

To recover the text at the second table-of-contents entry:

1. `book_navigation` → the `toc` container's second `nav_unit` → `target_position: { id: 850, offset: 0 }`.
2. Element id 850 is found by walking `document_data` → `reading_orders[0].sections` → each `section` → its page templates' `story_name` → that `storyline`'s content tree.
3. The element carrying `id: 850` has `content: { name: content_1, index: 1 }`.
4. Fragment `content` named `content_1`, entry 1 of its `content_list`, is the string — and offset 0 of that string is where the chapter begins.
5. The same element id, looked up in `position_id_map`, gives the pid; the pid, located among `location_map`'s boundaries, gives the Location the device displays.

## C. What is not established

Everything below is unknown or unresolved. An implementation **must not** read silence here as permission to guess.

### C.1. Structures named but never seen

The shared vocabulary and Amazon's authoring model both name constructs no known book exercises. They are structurally permitted and their encodings are unknown.

- **Page-template conditions** — The `$171` `condition` field and Ion's s-expression type both exist. Whatever selects templates by device state is exercised by content outside trade publishing, likely interactive or educational titles. The predicate grammar is unknown.
- **Multi-rendition books** — Multiple reading orders are structurally permitted by `document_data`. How a device chooses between them is unknown.
- **Interactive and media elements** — The authoring model names audio, video, slideshow, button, scrollable and interactive elements, each with a pop-up variant, alongside plugin containers and guided-view target panels. The vocabulary carries matching symbols. No element-level encoding for them is specified here.
- **The vector-shape block** — `$834`–`$851`: `ellipse`, `rectangle`, `line`, `polygon`, `polyline`, `shape_dimensions`, `vertex_list` and their geometry fields. Which producer emits them, and how they relate to the vector-path containers in the authoring model, is unknown.
- **`$608 structure`** — Named by the shared vocabulary; no fragment of this type is known, and its shape is unknown.

### C.2. Fields whose meaning is unresolved

- **important_cells** — Not header cells: it appears on tables with no header section, and where a `type: header` section exists the coordinates fall outside it. It co-occurs with `yj.table_features: [pan_zoom, scale_fit]` and `yj.table_selection_mode: yj.regional`, which suggests a small set of cells to keep anchored when a wide table is panned or restacked. That reading is a hypothesis.
- **yj_hdv** — Written by Amazon's converter and read by nothing (§12.2), so it is a build-time declaration rather than a rendering switch. What it declares is unknown; it is not an image-size threshold, since books with 1804×2560 covers do not carry it.
- **The reserved typography properties** — `ot_features`, `ligatures`, `kerning`, `hyphen_dictionary` and `min_hyphen_word_length` are named by the vocabulary and set by no book (§8.5). Their value grammars are unknown. They may be authoring-side only, or reserved and never shipped.
- **container_entity_map dependency ordering** — The manifest's per-container fragment lists are understood; its dependency-ordering fields are not.
- **Default property values and inheritance** — This document specifies which properties exist, not what a property means when absent. Nothing here states a default for any property.

### C.3. Behaviour no file settles

- **kfxgen_payload_sha1 enforcement** — Whether a device rejects a container whose digest does not match is unknown. A container re-serialised with a recomputed digest opens, so the digest is not checked against a signed manifest; whether it is checked against the payload at all is unresolved.
- **What a reader does with an unresolvable symbol** — Unknown, and of doubtful importance. No known book names a shared id above even the oldest shipping table (§5.2). Since Amazon ships one build of a title to every generation of KFX-capable hardware, a reader old enough to lack a symbol some other reader has must already tolerate what it does not recognise. Answering the question means authoring a file to provoke it, and the answer would describe one firmware's error handling rather than the format.

---

**KFX: Kindle Format 10 Structure and Semantics** — Draft 0.1. An unofficial specification produced independently of Amazon.com, Inc. "Kindle", "KFX" and "Amazon" are trademarks of their respective owner; their use here is descriptive. Amazon Ion is separately and publicly specified at amazon-ion.github.io/ion-docs.

Nothing in this document describes or enables circumvention of access controls. The structures specified are those of unencrypted containers.
