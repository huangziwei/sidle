# boko-kai

A private modified (改, *kai* — "revised/altered") fork of **[boko](https://github.com/zacharydenton/boko)**, customised for [sidle](../README.md). Not published, not synced with upstream.

Forked from upstream commit `a3df89622afa2e413a9b70f71a4822c8262ee22f` (master HEAD as of 2026-05-17). The nested `.git` has been removed; boko-kai is now versioned as part of sidle's git history. To diff against upstream:

```sh
git clone https://github.com/zacharydenton/boko.git /tmp/boko-upstream
diff -r /tmp/boko-upstream/src boko-kai/src
```

## What sidle needs from this fork

Two directional pipelines plus a container-merge step:

| Pipeline | Input | Output | Implementation |
|---|---|---|---|
| **EPUB → KFX** | publisher EPUB | KFX for Kindle | IR-based (upstream architecture + kai CJK extensions) |
| **KFX → EPUB** | KFX (possibly from kfx-zip) | EPUB for any reader | Mechanical port of calibre's `kfxlib/yj_to_epub_*.py` |
| **KFX-ZIP → KFX** | DRM-stripped multi-container bundle | single .kfx | Fragment-level merge (port of calibre's `convert_to_single_kfx`) |

The CLI auto-routes two pairs by extension and falls through to the IR spine for the rest:

- `boko convert in.kfx-zip out.kfx` → `kfx::merge` (special-cased in `main.rs`).
- `boko convert in.kfx out.epub` → `kfx_to_epub::convert_to_epub` (special-cased in `main.rs`).
- `boko convert in.epub out.kfx`, AZW3 / MOBI / Markdown directions, plus `Book::open` for any inspection use → upstream Importer → IR → Exporter spine. `Book::open` on a `.kfx-zip` path transparently runs the merge first (`src/model/book.rs:266`).

## What's from upstream boko

Untouched architecture and capabilities we inherit and rely on:

- **Importer → IR → Exporter** trait spine (`src/import/`, `src/model/`, `src/export/`).
- **Formats**: EPUB read+write, AZW3 read+write, MOBI read, Markdown write.
- **CSS pipeline**: html5ever HTML parser, cssparser, selectors-based cascade with UA defaults, optimize passes.
- **KFX format core**: Ion binary parser/writer (`src/kfx/ion.rs`), container parser (`src/kfx/container.rs`), full KFX symbol table (`src/kfx/symbols.rs`, 1725 LOC), schema-driven storyline import (`src/kfx/storyline.rs`), KFX exporter (`src/export/kfx.rs`, 3384 LOC).
- **`kfx-dump` CLI tool** (`src/bin/kfx-dump.rs`) — pretty printer for KFX containers. Extended by kai for ruby_content and reading_orders sections, but the scaffold is upstream.
- **WASM build target** (`cfg(target_arch = "wasm32")` paths).

## What kai adds on top

### CJK typography (IR-side, EPUB → KFX)

Every item below is end-to-end: CSS parser → IR field → cascade → KFX schema entry → KFX symbol emission, with the reverse (KFX → IR for `boko validate`) also wired.

| Feature | Status | Where |
|---|---|---|
| CSS `writing-mode` (incl. `-webkit-`, `-epub-` aliases) | done | `style/properties.rs:284`, `style/types.rs:50`, `kfx/style_schema.rs` |
| OPF `<spine page-progression-direction>` | done | `epub/parser.rs:181`, `kfx/context.rs`, `export/kfx.rs` |
| CSS `@import` (recursive inlining at load time) | done | `import/epub.rs:328` |
| `<html class>` participates in cascade (was `<body>` onward) | done | `dom/transform.rs:55-70` |
| `<html xml:lang>` propagated to body for inheritance | done | `dom/transform.rs:40-83` |
| Ruby (`<ruby>`/`<rb>`/`<rt>` ↔ KFX `ruby_content` fragments) | done | `model/node.rs:96`, `dom/role_map.rs:97`, `kfx/auxiliary.rs:73`, `kfx/context.rs:694` (RubyContentRegistry) |
| `text-emphasis-style` / `text-emphasis-color` / `text-emphasis-position` (圏点) | done | `style/properties.rs:336-389`, `style/types.rs:59-61`, `kfx/style_schema.rs:670-700` |
| `text-combine-upright` (縦中横) | done | `style/properties.rs:324`, `style/types.rs:56`, `kfx/style_schema.rs:704` |
| `text-align-last` (full keyword set) | done | `style/declaration.rs`, `style/parse/keywords.rs`, `kfx/style_schema.rs` |
| `lh` length unit (≈1.2em) | done | property parser |
| Drops `-kfx-attrib-xml-lang` sentinel (not real CSS; xml:lang on `<html>` covers intent) | done | YJ_PROPERTY_INFO table |

### KFX-ZIP → KFX merge (`src/kfx/merge/`)

DRM-stripped bundles (e.g. from `dedrm`) ship as multi-container `.kfx-zip` archives. calibre's `convert_to_single_kfx` merges them into a single in-memory KFX that the rest of its toolchain can consume; we needed the same.

- **`mechanical`** mode — faithful port of calibre's pipeline. Every entity parsed → walked → re-encoded. Correctness reference.
- **`fast`** mode (default) — byte-passthrough merge. Skips entity-body parse + re-encode, synthesizes only the merged `$270` (container_info) and `$419` (kfxgen_info) fragments. 3-6× the throughput of mechanical, produces calibre-accepted output verified to round-trip to identical EPUBs.
- Auto-falls-back to mechanical if fast-path preconditions don't hold (e.g. multiple sources carry `doc_symbols`).
- Uses `sha1_smol` rather than RustCrypto's `sha1` crate — `features=["asm"]` produced wrong digests on Apple Silicon that calibre rejected with "Incorrect kfxgen_payload_sha1".
- CLI: `boko convert in.kfx-zip out.kfx [--mode fast|mechanical]`.
- Public API: `boko::kfx::merge::merge_kfx_zip_with_mode(path, mode)`.

### KFX → EPUB mechanical port (`src/kfx_to_epub/`)

Parallel pipeline to the upstream IR path because KFX's data model (position maps with `(eid, offset)` anchors, ruby_index by name+id, layout_hints, classifications, JXR raw media) projects too lossily through boko's generic IR. Mirrors calibre's `kfxlib/yj_to_epub_*.py` (≈7.5K LOC Python). Modules:

- `loader.rs` — KFX → `BookData` (entity-by-type index, metadata, doc_symbols).
- `content.rs` (1631 LOC) — storyline → XHTML body. Recursive `process_content` over text / container / image / list / table / horizontal_rule / kvg_container / excerpt content types. Inline-ruby emission via `$142 style_events` + `$757 ruby_name` / `$758 ruby_id` → `<ruby><rb>...</rb><rt>...</rt></ruby>`. Post-passes: `consolidate_html` (strip empty spans, promote leaf-text `<div>` → `<p>`), `simplify_styles` (drop spec-default declarations), `fixup_styles_and_classes` (inline-style → generated `g<N>` classes).
- `navigation.rs` — NCX + anchor table extraction from `$266 anchors`. `process_position` registers `id="anchor-N"` on elements at `(location_id, 0)`.
- `properties.rs` — KFX style struct → CSS Declaration. `YJ_PROPERTY_INFO` table covers writing-mode, font, text-align, margins, padding, borders, ruby-*, text-decoration, text-emphasis-*, text-combine-upright.
- `resources.rs` — raw-media extraction. JXR → JPEG transcode runs in parallel across images via `std::thread::scope`.
- `jxr/` — **pure-Rust JPEG-XR decoder**. Line-by-line port of calibre's `jxr_image.py` + `jxr_container.py` + `jxr_misc.py`. Avoids C FFI / libjxr / ImageMagick deps so sidle's Tauri bundle stays self-contained.
- `output.rs` — EPUB zip assembly (OPF 2.0, NCX, manifest, spine, titlepage SVG wrapper for cover).
- CLI: `boko convert in.kfx out.epub` (auto-detected by extension).
- Public API: `boko::kfx_to_epub::convert_to_epub(&kfx_bytes) -> Result<Vec<u8>, ConvertError>`.

Status: beats calibre on HTML semantic % (70.35% vs 68.25% on the horror corpus; +2.4 to +6.7 pp on the full 30-book test set), parity on ruby / text / writing-mode / page-progression / metadata, +1 image/book (titlepage SVG wrapper). Throughput on the 30-book corpus: boko 35.6 s vs calibre 126.1 s (3.5× aggregate, 5.3× mean). Deferred items pending fixtures: `bcRawFont` → `@font-face`, partial-offset anchors (`offset > 0`), dropcap, inline per-run font/color style_events. See `.claude/plans/kfx-to-epub-port.md`.

### Conversion validators (`src/validate/`)

New `boko validate --direction {epub-to-kfx | kfx-to-epub} <check>` subcommand with 10 checks:

| Check | What it verifies |
|---|---|
| `ruby` | Every `<ruby>` pair preserved across the conversion |
| `text` | Visible text characters preserved |
| `style` | CSS property coverage + class-system richness |
| `tags` | Which HTML tag names get a semantic role vs fall through to generic Container |
| `links` | `<a href>` resolves to a real anchor on the other side |
| `images` | `<img src>` survives and bytes are renderable (magic-byte check) |
| `nav` | TOC + headings round-trip |
| `metadata` | OPF metadata (title, language, authors, cover, identifiers) round-trip |
| `writing-mode` | Book-level `horizontal-tb` / `vertical-rl` / `vertical-lr` preserved |
| `page-progression` | OPF `<spine page-progression-direction>` matches KFX |

Each validator has independent EPUB and KFX extractors so parser-side bugs surface here rather than being mirrored on both sides. Ground truth is whichever side is the source of the named direction (never calibre).

### Other additions

- **`src/trace.rs`** — env-gated wall-time tracing. `BOKO_MERGE_TRACE=1` for the kfx-zip merge, `BOKO_KFX2EPUB_TRACE=1` for the mechanical port. Marks print cumulative wall-time per stage.
- **`Format::Kfx`** recognizes `.kfx-zip` as input (`src/model/book.rs:216`).
- **`Format::Markdown`** exists upstream as export-only; remains export-only.
- **`jpeg-encoder`** crate dependency — pure-Rust JPEG encoder for JXR transcode output.
- **`ion-rs`** crate dependency — used only by `kfx-dump` for human-readable Ion dumps.
- **Tests** (`boko-kai/tests/`): `document_writing_mode.rs`, `css_import_utf8.rs`, `kfx_zip_import.rs` are kai-specific regression tests; the rest are upstream.

### Crate-level changes

| Change | Reason |
|---|---|
| `name = "boko-kai"`, lib `name = "boko"` | Crate renamed for clarity; lib import name unchanged so dependents still write `use boko::...` |
| `version = "0.3.0+kai.1"` | Kai version tag |
| `edition = "2024"` | Matches upstream |
| `publish = false` | Private fork, not on crates.io |
| `LICENSE` GPL-3.0-or-later | Inherited from upstream, unchanged |

## How sidle uses this

In `sidle/server/Cargo.toml`:

```toml
[dependencies]
boko = { package = "boko-kai", path = "../boko-kai" }
```

So sidle still writes `use boko::*;` everywhere while pulling the kai package.

Sidle invokes the three pipelines either through the `boko` CLI or through the public APIs:

```rust
// EPUB → KFX (IR path, with kai CJK extensions)
let mut book = boko::Book::open("in.epub")?;
book.export(boko::Format::Kfx, &mut writer)?;

// KFX → EPUB (mechanical calibre port)
let epub_bytes = boko::kfx_to_epub::convert_to_epub(&kfx_bytes)?;

// KFX-ZIP → KFX (fragment merge)
let kfx_bytes = boko::kfx::merge::merge_kfx_zip_with_mode(path, mode)?;
```

## Relationship to upstream

Fully diverged. The nested `.git` has been removed and we no longer track or push to either `zacharydenton/boko` or `huangziwei/boko`. Pulling a future upstream bug-fix is a manual cherry-pick exercise (see the diff snippet above).

## Original boko README

The upstream README has been replaced. See [zacharydenton/boko](https://github.com/zacharydenton/boko) for the original project description, browser-app demo, and contribution guidelines.

## License

[GPL-3.0-or-later](LICENSE) — same as upstream boko.
