# jxr

A pure-Rust JPEG XR (ITU-T T.832 / ISO/IEC 29199-2) codec: decoder + encoder,
no `unsafe`, `std` only.

KFX stores its bundled images as JPEG XR, so every route that reads or writes
one goes through here: `import::kfx` decodes raw media on the way to the IR,
and `export::kfx` encodes interior plates on the way back.

The decoder covers the full T.832 codestream syntax — all band modes, planar
alpha, every overlap mode, tiling, frequency and spatial order, trimmed
flexbits, windowing — with YONLY, YUV 4:4:4 and YUV 4:2:0/4:2:2 reconstruction,
and output formats from 8-bit gray through 16/32-bit integer, fixed point,
half/full float, RGBE and CMYK(A). The container parser knows the pixel-format
GUID table, tolerates unknown tags and extra IFDs, and exposes the ICC profile,
XMP packet and orientation tag when present. The encoder writes 8bppGray and
24bppRGB, lossless or lossy.

## Usage

```rust
use bokai::jxr::{ColorMode, ImageInput, QpSet, encode};

// Encode a 4×4 grayscale gradient losslessly…
let pixels: Vec<u8> = (0..16).map(|i| (i * 16) as u8).collect();
let planes = vec![pixels.clone()];
let input = ImageInput { width: 4, height: 4, planes: &planes, premultiplied_alpha: false };
let file = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).unwrap();

// …and decode it back: TIFF-like container, then WMPHOTO codestream.
let container = bokai::jxr::decode::container::parse(&file).unwrap();
let image = bokai::jxr::decode::decoder::Decoder::new(container.image_data)
    .decode()
    .unwrap();
assert_eq!((image.width, image.height), (4, 4));
let round_trip: Vec<u8> = image.image_plane[0].iter().map(|&v| v as u8).collect();
assert_eq!(round_trip, pixels);
```

Lossy encoding takes a `QpSet { dc, lp, hp }` directly, or use the
`quality_to_qp(0..=100)` knob.

## Sources

These sources are the `jxr` crate, which lives at `jxr/` in this repository and
is not published to crates.io. `src/` here symlinks that directory, so the two
build from one copy and the published `bokai` carries it as ordinary files.
Its own README covers the verification matrix and the fuzz targets.
