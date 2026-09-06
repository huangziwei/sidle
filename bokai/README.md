# bokai = boko + kai (改)

`bokai` is a diverged fork of [boko](https://github.com/zacharydenton/boko) [^1]. `boko` is a fast ebook converter for EPUB, KFX, AZW3, and MOBI, written in Rust. `bokai` improves upon it with the focus on CJK typography.

## Install

```sh
cargo install bokai          # the `bokai` and `kfx-dump` binaries
```

As a library:

```sh
cargo add bokai --no-default-features
```

The default features build the complete CLI. A library consumer wants
`--no-default-features` plus the formats it needs (`aozora`, `pdf`, `validate`,
`nbk`); `kfx` and `epub` are always present.

## Build

```sh
cargo build --release
cargo test

# Kindle: KFX<->EPUB and the subcommands over it, without aozora, pdf, validate.
rustup target add armv7-unknown-linux-musleabihf
cargo build --release \
    --target armv7-unknown-linux-musleabihf \
    --no-default-features --features native \
    --bin bokai
```

## Examples

```sh
bokai convert book.epub book.kfx
bokai validate all book.epub book.kfx     # --direction kfx-to-epub for the reverse
bokai kfx ls book.kfx

kfx-dump -s book.kfx                      # entity counts and sizes
kfx-dump -r -f ruby book.kfx              # one report, entity ids resolved to names
```

## Help

```
Fast ebook converter

Usage: bokai <COMMAND>

Commands:
  info               Show book metadata and structure
  convert            Convert between ebook formats
  sections           Extract hierarchical section tree (JSON)
  dump               Dump the IR (Intermediate Representation) for a book
  validate           Validate a conversion. Works in both directions: EPUB→KFX (default) or KFX→EPUB (via `--direction kfx-to-epub`). The ground truth is always the source format of the named direction
  repair-toc         Rebuild a book's table of contents from its own structure (KFX or EPUB). Prints the chapters the proposer derives; with `output`, writes the repaired book
  reorder-spine      Reorder an EPUB's spine to the order its own navigation reads, for a book whose spine contradicts its TOC. Prints the proposed reading order; with `output`, writes the reordered book
  split              Split a collection (合本版 / 全集 / boxed set) into the volumes it collects. Prints the proposed cuts; with `--out`, writes one EPUB per volume into that directory
  rename-class       Rename a CSS class across an EPUB: every selector, `<style>` block and `class` attribute
  remove-unused-css  Remove stylesheet rules no document in the EPUB can match. Prints the rules; with `output`, writes the trimmed book
  beautify           Re-indent an EPUB's XHTML and CSS members without changing what they render
  split-document     Split a content document in two before the block at a line, moving ids, links, manifest and spine entries with it
  merge-document     Fold the next spine document into this one, retargeting every link
  upgrade            Upgrade an EPUB 2 package to EPUB 3 in place: version, metadata, navigation document, manifest properties, DOCTYPEs
  kfx                Read and compare KFX containers at the entity level
  help               Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

[^1]: Forked from upstream commit `a3df89622afa2e413a9b70f71a4822c8262ee22f`
    (master HEAD as of 2026-05-17). To diff against upstream:

    ```sh
    git clone https://github.com/zacharydenton/boko.git /tmp/boko-upstream
    diff -r /tmp/boko-upstream/src src
    ```
