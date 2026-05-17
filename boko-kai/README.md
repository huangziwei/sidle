# boko-kai

A private modified (改, *kai* — "revised/altered") fork of **[boko](https://github.com/zacharydenton/boko)**, customised for [sidle](../README.md). Not published, not synced with upstream.

## Why fork?

Upstream boko is a fast pure-Rust EPUB ↔ KFX converter. We adopted it because nothing else avoids Amazon's Kindle Previewer 3 binary while writing real `.kfx`. But evaluating boko on Japanese vertical-RTL EPUBs (typography features Amazon ships every day) showed several features dropped at import time even though the KFX format-side symbols were already declared in boko's symbol table:

- `writing-mode` (CSS) and `<spine page-progression-direction>` (OPF) — vertical-RTL layout ✅ added
- `text-emphasis-*` — 圏点 / sesame-dot emphasis marks ⏳ planned
- `<ruby><rt>` element preservation — furigana ⏳ planned
- `text-combine-upright` — 縦中横 ⏳ planned
- `@import` rule resolution — most EPUB stylesheets are organised as `book-style.css` → many `@import "..."` ✅ added

Each of these required CSS-parser + cascade + KFX-export wiring. The fix surface grew large enough that contributing back as a single PR would overwhelm upstream review. Carrying a private fork is cheaper for us and lets us iterate on what matters for our library without dragging upstream into Japanese-typography minutiae.

## Differences from upstream

| Area | Change |
|---|---|
| CSS `writing-mode` | Parsed (incl. `-webkit-`, `-epub-` aliases) and emitted to KFX style table |
| OPF `page-progression-direction` | Read from `<spine>` and emitted to `reading_orders` |
| HTML `<html class>` | Cascade now considers the html element's classes (was only `<body>` onward) |
| CSS `@import` | Inlined recursively during EPUB stylesheet load |
| `kfx-dump -f reading_orders` | Now displays `page_progression_direction` |
| Crate name | `boko-kai` (lib import name still `boko`) |
| License | GPL-3.0-or-later — inherited from upstream, unchanged |
| Publication | `publish = false`; not on crates.io |

Forked from upstream commit `a3df89622afa2e413a9b70f71a4822c8262ee22f` (master HEAD as of 2026-05-17). The nested `.git` has been removed; boko-kai is now versioned as part of sidle's git history. To diff against upstream:

```sh
git clone https://github.com/zacharydenton/boko.git /tmp/boko-upstream
diff -r /tmp/boko-upstream/src boko-kai/src
```

## Relationship to upstream

Fully diverged. The nested `.git` has been removed and we no longer track or push to either `zacharydenton/boko` or `huangziwei/boko`. Pulling a future upstream bug-fix is a manual cherry-pick exercise (see the diff snippet above).

## How sidle uses this

In `sidle/server/Cargo.toml`:

```toml
[dependencies]
boko = { package = "boko-kai", path = "../boko-kai" }
```

This way sidle still writes `use boko::*;` everywhere while pulling the kai package.

## Original boko README

The upstream README has been replaced. See [zacharydenton/boko](https://github.com/zacharydenton/boko) for the original project description, browser-app demo, and contribution guidelines.

## License

[GPL-3.0-or-later](LICENSE) — same as upstream boko.
