# jxr

A pure-Rust JPEG XR (ITU-T T.832 / ISO/IEC 29199-2) codec: decoder + encoder.
Zero dependencies, no `unsafe`, `std` only.

It lives inside the sidle repo and exists because Kindle KFX media is JPEG XR
and sidle's app bundle must stay free of C FFI (no libjxr, no ImageMagick). It
is **not published to crates.io and carries no maintenance promises** — but it
is deliberately kept liftable: the crate is fully self-contained (no workspace
inheritance, no dependencies), so copying this directory into your own tree
gives you a working codec. If you find it useful, take it (license below).

## What it supports today

**Decoder** — the full T.832 codestream *syntax* (header, entropy coding,
transforms): all band modes (DCONLY → ALL_BANDS), planar alpha, overlap modes
0/1/2, tiling (uniform + non-uniform), frequency and spatial order, trimmed
flexbits, windowing, short/long headers — verified bit-exact against the
libjxr reference decoder across a minted matrix. *Reconstruction* covers
YONLY, YUV 4:4:4, and YUV 4:2:0 / 4:2:2 internal color (joint-coded chroma
entropy, the 2×2/2-pt chroma transforms and dedicated chroma overlap
filtering, and centering-aware upsampling — all oracle-verified pixel-exact);
the RGBE / some float output paths remain explicit gaps inherited from the
Python source, scheduled in the roadmap. The **container** parser accepts the
seven pixel-format GUIDs Kindle media uses (8bppGray, 16bppGray, 24bppBGR,
24bppRGB, 32bppRGBA, 24bpp3Channels, 32bpp4Channels); decoded output is raw
`i32` planes plus layout fields. Orientation tags are parsed but not yet
applied.

**Encoder** — 8-bit grayscale (`8bppGray`/YONLY) and 8-bit color
(`24bppRGB`/YUV 4:4:4): full ALL_BANDS (DC + LP + HP + flexbits) with multi-MB
prediction and adaptive VLC/scan state, spatial order, single tile, per-band
quantization (`QpSet::LOSSLESS` round-trips bit-exact; higher QP = lossy),
arbitrary non-16-aligned dimensions up to 65 536 px.

Both halves are being pushed toward full-spec general-purpose coverage; the
roadmap lives in the repo at `.claude/plans/jxr-general-codec.md`.

## Usage

```rust
use jxr::{encode, ColorMode, ImageInput, QpSet};

// Encode a 4×4 grayscale gradient losslessly…
let pixels: Vec<u8> = (0..16).map(|i| (i * 16) as u8).collect();
let planes = vec![pixels.clone()];
let input = ImageInput { width: 4, height: 4, planes: &planes };
let file = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).unwrap();

// …and decode it back: TIFF-like container, then WMPHOTO codestream.
let container = jxr::decode::container::parse(&file).unwrap();
let image = jxr::decode::decoder::Decoder::new(container.image_data)
    .decode()
    .unwrap();
assert_eq!((image.width, image.height), (4, 4));
let round_trip: Vec<u8> = image.image_plane[0].iter().map(|&v| v as u8).collect();
assert_eq!(round_trip, pixels);
```

Lossy encoding takes a `QpSet { dc, lp, hp }` directly, or use the
`quality_to_qp(0..=100)` knob.

## Provenance & license

**GPL-3.0** — see `LICENSE` in this directory; it travels with any copy.

- The **decoder** is a line-by-line Rust port of `jxr_image.py`
  (© 2016–2025 John Howell, GPL v3, from the calibre *KFX Input* plugin),
  itself implemented from the ITU-T T.832 (08/2016) pseudo-code with
  corrections.
- The **encoder** is original work built as the decoder's forward mirror, with
  jxrlib (BSD-2-Clause, Microsoft) consulted as the algorithm reference for
  quantization and the forward transforms.
