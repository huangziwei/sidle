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
Output formatting covers the deep and exotic formats too — 16/32-bit integer
and fixed point, half/full float (emitted as bit patterns), RGBE, CMYK(A) —
each verified sample-exact against the reference decoder, including two real
Windows HDR screen captures (3440×1440 scRGB 128bppRGBAFloat, bit-exact). The
**container** parser knows the full pixel-format GUID table (~70 formats),
tolerates unknown tags and extra IFDs, exposes the ICC profile / XMP packet
when present, handles separate planar-alpha codestreams (merged by
`decode::decode_image`), and exposes the orientation tag —
`decode::apply_orientation` implements all 8 transforms (not auto-applied,
matching libjxr). Decoded output is raw `i32` planes plus layout fields, or
use `DecodedImage::to_pixel_buffer()` for interleaved little-endian samples
with an explicit layout (sample type, color model, alpha mode).

**Encoder** — 8-bit grayscale (`8bppGray`/YONLY), 8-bit color
(`24bppRGB`) at **4:4:4, 4:2:2, 4:2:0 or luma-only** chroma sampling
(`ChromaSampling` via `encode_with_options`; the 42x downsampler is the
libjxr 5-tap even-centered filter, centering declared 0/0), and 8-bit
color + alpha (`32bppBGRA`/`32bppPBGRA`: a T.832 alpha image plane, per-MB
interleaved, its own per-band QPs, premultiplied property bit; the primary
may be subsampled): full ALL_BANDS (DC + LP + HP + flexbits) with multi-MB
prediction and adaptive VLC/scan state — plus the full 8-bit structural
envelope: **band truncation** (`BandsPresent`) and trimmed flexbits,
**tiling** (uniform grids via `tile_cols`/`tile_rows`; index table;
non-uniform grids internally), **explicit window margins**
(`window_top`/`window_left`), **overlap pre-filtering** modes 1 and 2
(`Overlap`; exact inverses of the decoder's post-filters, so lossless stays
bit-exact), **frequency order** (`frequency`: per-band tile packets, libjxr's
default order), the short *and* long header (auto-selected past 2¹⁶ px, up
to 2²⁸), **scaled or unscaled arithmetic** (`scaled_flag` both ways; scaled
is libjxr's lossy mode — its chroma half-step floors, so it is not
bit-lossless for color), and the complete T.832 quantization syntax:
per-band QPs (`QpSet::LOSSLESS` at 4:4:4 round-trips bit-exact; higher QP =
lossy; subsampled chroma is lossy by construction), separate chroma
quantizers (`chroma_qp` → `COMP_SEPARATE`), and — as crate-internal
capability — `COMP_INDEPENDENT`, per-tile QP sets, and per-MB DQUANT index
maps. Arbitrary non-16-aligned dimensions. Interleaved source buffers in the
common memory orders (gray, RGB, BGR, RGBA, BGRA — premultiplied = BGRA/RGBA
plus the `premultiplied_alpha` flag) normalize to the planar input via
`deinterleave`; output always uses the canonical GUID for its channel count.
Gray+alpha input is rejected: JPEG XR defines no grayscale-with-alpha
container pixel format — expand to RGBA. Separate-codestream alpha (the
container-level arrangement libjxr calls *planar*, `-a 2`) is decoded but
deliberately never emitted — the in-codestream alpha image plane covers the
capability in one codestream. Every feature is verified two ways: in-crate
round-trip/equivalence invariants against this crate's decoder, and
JxrDecApp-exact readback (with jxrencapp PSNR/size parity wherever the
reference encoder can mint a counterpart).

The encoder's 8-bit envelope is complete; deep/HDR input formats remain
encoder-side future work. The roadmap lives in the repo at
`.claude/plans/jxr-general-codec.md`.

## Usage

```rust
use jxr::{encode, ColorMode, ImageInput, QpSet};

// Encode a 4×4 grayscale gradient losslessly…
let pixels: Vec<u8> = (0..16).map(|i| (i * 16) as u8).collect();
let planes = vec![pixels.clone()];
let input = ImageInput { width: 4, height: 4, planes: &planes, premultiplied_alpha: false };
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
