# boko-kai

A diverged, modified (改, *kai* — "revised/altered") fork of **[boko](https://github.com/zacharydenton/boko)**, customised for [sidle](../README.md).

Forked from upstream commit `a3df89622afa2e413a9b70f71a4822c8262ee22f` (master HEAD as of 2026-05-17). The nested `.git` has been removed; boko-kai is now versioned as part of sidle's git history. To diff against upstream:

```sh
git clone https://github.com/zacharydenton/boko.git /tmp/boko-upstream
diff -r /tmp/boko-upstream/src boko-kai/src
```

## What sidle needs from this fork

sidle's job is to get books onto a Kindle as KFX, and back off it as EPUB. boko-kai is the conversion engine. Every path either ends at KFX (for the device) or at EPUB (which then re-enters the EPUB → KFX path):

| Pipeline | Input | Output | Implementation |
|---|---|---|---|
| **EPUB → KFX** | publisher EPUB | KFX for Kindle | IR spine (upstream) + kai CJK extensions |
| **KFX → EPUB** | KFX (incl. from a `.kfx-zip`) | EPUB for any reader | mechanical port of calibre's `kfxlib/yj_to_epub_*.py` (`kfx_to_epub/`) |
| **KFX-ZIP → KFX** | DRM-stripped multi-container bundle | single `.kfx` | fragment-level merge (`kfx/merge/`) |
| **AZW3 → EPUB** | AZW3 / KF8 | EPUB | IR spine (upstream importer + kai vertical-text metadata) |
| **MOBI → EPUB** | MOBI6 | EPUB | IR spine (upstream importer + kai vertical-text metadata) |
| **Aozora ZIP → EPUB** | 青空文庫 `.zip` | EPUB | dedicated `aozora/` pipeline (port of a standalone JS tool) |

The `boko convert` CLI routes by extension (and one content sniff), special-casing the non-IR paths and falling through to the IR spine for the rest. In order:

- `in.kfx → out.epub` → `kfx_to_epub::convert_to_epub` (extension-detected).
- `in.zip → out.epub` → Aozora pipeline, but only if the zip's `.txt` sniffs as an Aozora source (`底本` / `［＃` markers); otherwise it falls through.
- `in.kfx-zip → out.kfx` → `kfx::merge` (extension-detected; `--mode fast|mechanical`).
- `in.epub / in.azw3 / in.mobi → out.epub / out.kfx`, plus `Book::open` for any inspection → upstream Importer → IR → Exporter spine. The KFX importer auto-detects a `.kfx-zip` and merges it first, so `Book::open` works on a bundle too.

## What's from upstream boko

The architecture and most of the format machinery is inherited untouched:

- **Importer → IR → Exporter trait spine** (`import/`, `model/`, `export/`) with lazy random-access IO (`io/`: `ByteSource`, `FileSource`, `MemorySource`).
- **Format readers**: EPUB, AZW3/KF8, MOBI6, KFX.
- **CSS pipeline**: html5ever HTML parser, cssparser, selectors-based cascade with UA defaults (`style/`, `style/parse/`), plus the DOM optimize passes (`dom/optimize/`: prune, merge, fuse, wrap, vacuum, table).
- **KFX format core**: Ion binary parser/writer (`kfx/ion.rs`), container parser (`kfx/container.rs`), the full KFX symbol table (`kfx/symbols.rs`), schema-driven storyline import (`kfx/storyline.rs`), and the KFX exporter and its supporting schema/registry/serialization modules (`export/kfx.rs`, `kfx/{schema,tokens,transforms,metadata,serialization,style_schema,style_registry,cover,auxiliary,context}.rs`).
- **`kfx-dump` CLI** (`src/bin/kfx-dump.rs`) — pretty-printer for KFX containers (the only consumer of the `ion-rs` dependency). Extended by kai to dump the `ruby_content` and reading-order sections.

## What kai changes

### Tightened to sidle's surface (removals)

Upstream can also *write* AZW3, MOBI, plain text, and Markdown. sidle needs none of that, so kai dropped it: `export/azw3.rs`, `export/text.rs`, the whole `markdown/` module, and the KF8 write-side helpers (`mobi/{skeleton,tbs,writer_transform}.rs`). Net result:

- **Reads**: EPUB, AZW3/KF8, MOBI6, KFX (+ `.kfx-zip`).
- **Writes**: EPUB and KFX only (`Format::can_export`).

### CJK typography (EPUB → KFX)

The reason for the fork. Each item is wired end-to-end: CSS parser → IR field → cascade → KFX schema entry → KFX symbol emission, with the reverse direction also wired for `boko validate`. Upstream's `style/` parses none of these (the symbol *names* live in `kfx/symbols.rs` because it mirrors Amazon's full table, but nothing reads them); kai adds the parsing, cascade, and emission.

| Feature | Where |
|---|---|
| CSS `writing-mode` (incl. `-webkit-` / `-epub-` aliases) | `style/properties.rs`, `style/types.rs`, `kfx/style_schema.rs` |
| OPF `<spine page-progression-direction>` | `epub/parser.rs`, `kfx/context.rs`, `export/kfx.rs` |
| `text-emphasis-style` / `-color` / `-position` (圏点) | `style/properties.rs`, `style/parse/keywords.rs`, `kfx/style_schema.rs` |
| `text-combine-upright` (縦中横 / tate-chu-yoko) | `style/properties.rs`, `kfx/style_schema.rs` |
| `text-align-last` (full keyword set) | `style/declaration.rs`, `style/parse/keywords.rs`, `kfx/style_schema.rs` |
| Ruby (`<ruby>`/`<rb>`/`<rt>` ↔ KFX `ruby_content`) | `dom/role_map.rs` (`Role::Ruby`/`RubyText`), `kfx/auxiliary.rs`, `kfx/context.rs` (`RubyContentRegistry`) |
| CSS `@import` (recursive inlining at load time) | `import/epub.rs` |
| `lh` length unit (≈ 1.2em) | `style/parse/values.rs` |

### Vertical-text metadata for AZW3 / MOBI

The AZW3 and MOBI importers are upstream, but neither carried `primary-writing-mode` or `page-progression-direction` into the IR, so Japanese books lost their vertical-text intent at the format boundary. kai propagates both EXTH fields (and derives PPD from the writing mode when EXTH 527 is absent, matching calibre) in `import/azw3.rs` and `import/mobi.rs`, so an AZW3/MOBI → EPUB → KFX round-trip keeps the book vertical.

### KFX-ZIP → KFX merge (`kfx/merge/`)

DRM-stripped bundles (e.g. from KFXArchiver) ship as multi-container `.kfx-zip` archives. calibre's `convert_to_single_kfx` merges them into one in-memory KFX; we needed the same.

- **`fast`** mode (default) — byte-passthrough merge. Copies entity bodies verbatim and synthesizes only the merged `$270` (container_info) and `$419` (kfxgen_info) fragments. ~3–6× the throughput of mechanical; produces a different byte stream that calibre still accepts and that round-trips to identical EPUBs.
- **`mechanical`** mode — faithful port of calibre's pipeline; every entity is parsed → walked → re-encoded. Kept as the correctness reference that the fast path is validated against.
- `fast` auto-falls-back to `mechanical` when its preconditions don't hold (e.g. multiple sources carry `doc_symbols`).
- The `kfxgen_payload_sha1` is computed with `sha1_smol`, not RustCrypto's `sha1` with `features=["asm"]` — the latter produced wrong digests on Apple Silicon that calibre rejected.
- CLI: `boko convert in.kfx-zip out.kfx [--mode fast|mechanical]`. API: `boko::kfx::merge::merge_kfx_zip_with_mode(path, mode)`.

### KFX → EPUB mechanical port (`kfx_to_epub/`)

A parallel pipeline rather than the generic IR path, because KFX's data model (position maps with `(eid, offset)` anchors, ruby by name+id, layout hints, JXR raw media) projects too lossily through boko's IR. Mirrors calibre's `kfxlib/yj_to_epub_*.py` as closely as Rust allows. Modules:

- `loader.rs` — KFX → `BookData` (entity-by-type index, metadata, doc_symbols).
- `content.rs` — storyline → XHTML body. Recursive walk over text / container / image / list / table / rule / kvg / excerpt content; inline ruby via `$142 style_events` + `$757`/`$758` → `<ruby><rb>…</rb><rt>…</rt></ruby>`. Post-passes: `consolidate_html`, `replace_eol_with_br`, `simplify_styles`, `fixup_styles_and_classes` (inline style → generated `g<N>` classes).
- `navigation.rs` — NCX + OPF guide from `book_navigation`; anchor registration from `$266 anchors`.
- `properties.rs` — KFX style struct → CSS Declaration (writing-mode, font, text-align, box model, ruby, text-emphasis, text-combine-upright, …).
- `resources.rs` — raw-media extraction; JXR → JPEG transcode runs in parallel via `std::thread::scope`.
- JPEG-XR decode lives in the **standalone top-level `jxr` crate** (`../jxr`, re-exported as `boko::jxr`): pure Rust, zero deps, so the Tauri bundle stays free of C / libjxr / ImageMagick. `image/jxr_transcode.rs` is the JXR→JPEG glue.
- `output.rs` — EPUB zip assembly (OPF 2.0, NCX, manifest, spine, titlepage SVG wrapper for the cover).
- CLI: `boko convert in.kfx out.epub`. API: `boko::kfx_to_epub::convert_to_epub(&kfx_bytes)`.

Output is validated against the source KFX (via `boko validate --direction kfx-to-epub`) and against strict EPUB-3 readers like Apple Books, never against calibre's output. Deferred, pending real fixtures: `bcRawFont` → `@font-face` (raw font bytes are currently passed through as media), partial-offset anchors (`offset > 0`), dropcap, and inline per-run font/colour `style_events`.

### Aozora Bunko ZIP → EPUB (`aozora/`)

Faithful port of a standalone JS tool (`aozora-epub.html`). Detected by the CLI when a `.zip` contains a `.txt` with Aozora markup markers. Pipeline: `parser_txt.rs` (Shift-JIS decode + `parseTxt`/`convertAozoraLine`, ~40 regex patterns) → `Document` → `cover.rs` (`buildCoverSvg` rasterised to JPEG via `resvg` with the system Mincho font) → `epub_builder.rs` → EPUB bytes. The output is gated through the standalone EPUB-3 validator before being written, then re-enters the normal EPUB → KFX path.

### Conversion validators (`validate/`)

`boko validate [--direction epub-to-kfx | kfx-to-epub | azw3-to-epub] <check>` with these checks:

| Check | What it verifies |
|---|---|
| `ruby` | Every `<ruby>` pair preserved across the conversion |
| `text` | Visible text characters preserved |
| `style` | CSS property coverage + class-system richness |
| `tags` | Which HTML tag names get a semantic role vs. fall through to generic Container |
| `links` | `<a href>` resolves to a real anchor on the other side |
| `images` | `<img src>` survives and bytes are renderable (magic-byte check) |
| `nav` | TOC + headings round-trip |
| `metadata` | OPF metadata (title, language, authors, cover, identifiers) round-trip |
| `writing-mode` | Book-level `horizontal-tb` / `vertical-rl` / `vertical-lr` preserved |
| `page-progression` | OPF `<spine page-progression-direction>` matches the KFX |
| `all` | Run everything and print a scorecard |

Each check has independent EPUB and KFX extractors (the EPUB side does *not* go through boko's IR), so a parser-side bug surfaces here instead of being mirrored on both sides. Ground truth is always the source format of the named direction — never calibre's output.

Separately, `validate/epub3/` is a standalone EPUB-3 spec checker (mimetype, container, manifest/spine consistency, nav doc, `linear="no"` reachability, no `..` escapes above the OPF root) used to gate the Aozora output before it ships.

### Other additions

- **`trace.rs`** — env-gated wall-time tracing: `BOKO_MERGE_TRACE=1` for the kfx-zip merge, `BOKO_KFX2EPUB_TRACE=1` for the KFX → EPUB port. Marks print cumulative wall-time per stage.
- **Dependencies** not in upstream: `jpeg-encoder` (JXR re-encode + Aozora cover), `image` (decode non-JPEG images to JPEG for the EPUB → KFX path), `regex` and `resvg` (Aozora parsing + cover rasterisation).
- **Tests** (`tests/`): `document_writing_mode.rs`, `css_import_utf8.rs`, `css_transparent_background.rs`, `kfx_zip_import.rs`, `kfx_font_extraction.rs`, `aozora_parse.rs`, `link_resolution.rs`, `normalized_export.rs`.

### Crate-level changes

| Change | Reason |
|---|---|
| `name = "boko-kai"`, lib `name = "boko"` | Crate renamed for clarity; lib import name unchanged so dependents still write `use boko::…` |
| `version = "0.3.0+kai.3"` | Kai version tag |
| `publish = false` | Private fork, not on crates.io |
| `LICENSE` GPL-3.0-or-later | Inherited from upstream, unchanged |

## How sidle uses this

In `sidle/server/Cargo.toml`:

```toml
[dependencies]
boko = { package = "boko-kai", path = "../boko-kai" }
```

So sidle keeps writing `use boko::*;` everywhere while pulling the kai package. It drives the pipelines either through the `boko` CLI or the public APIs:

```rust
// EPUB → KFX (IR path, with kai CJK extensions)
let mut book = boko::Book::open("in.epub")?;
book.export(boko::Format::Kfx, &mut writer)?;

// AZW3 / MOBI → EPUB (IR path) — then feed the EPUB back through the line above
let mut book = boko::Book::open("in.azw3")?;
book.export(boko::Format::Epub, &mut writer)?;

// KFX → EPUB (mechanical calibre port)
let epub_bytes = boko::kfx_to_epub::convert_to_epub(&kfx_bytes)?;

// KFX-ZIP → KFX (fragment merge)
let kfx_bytes = boko::kfx::merge::merge_kfx_zip_with_mode(path, mode)?;
```
