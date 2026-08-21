//! JPEG-XR encoder: the decoder's forward mirror, covering every format
//! the decoder reads (the full T.832 envelope — see the crate README).
//!
//! Built bottom-up against [`crate::decode`] as a round-trip oracle.
//! Entry points:
//! [`encode`] / [`encode_with_alpha_qp`] (classic 8-bit),
//! [`encode_with_options`] (the full 8-bit option surface), and
//! [`encode_typed`] (deep/HDR/exotic [`SamplePlanes`]).
//! `QpSet::LOSSLESS` round-trips bit-exact wherever the format family is
//! exact (see the per-family notes on [`encode_typed`]); `QP > 0` is lossy.

// Encoder machinery: crate-visible only. The public encode surface is the
// re-exports below plus the `encode*` functions — narrowed deliberately so the
// API rustdoc shows the contract, not the internals.
pub(crate) mod bitstream;
pub(crate) mod codestream;
pub(crate) mod coeff;
pub(crate) mod color;
pub(crate) mod container;
pub(crate) mod convert;
pub(crate) mod entropy;
pub(crate) mod gray;
pub(crate) mod hp;
mod multi;
mod overlap;
pub(crate) mod quant;
pub(crate) mod transform;

pub use convert::SamplePlanes;
pub use quant::{BandQp, QpPlan, QpSet, TileQps};

/// How the input planes are interpreted as color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Single luma plane (`8bppGray`). Default: every Kindle we target is B&W
    /// e-ink, and the source EPUB retains the color master, so color is only
    /// dropped from the device copy and is recoverable by reconverting.
    Grayscale,
    /// Full color via the internal YUV 4:4:4 transform + chroma planes
    /// (`24bppRGB`). For desktop/Sidle-reader color; the device is still B&W so
    /// grayscale stays the pipeline default. A 1-plane input encodes as
    /// grayscale even in this mode (no chroma to synthesize).
    Color,
    /// Four ink planes C,M,Y,K (`32bppCMYK`/`64bppCMYK`; a 5th plane is the
    /// alpha image plane, `40/80bppCMYKAlpha`) coded through the internal
    /// YUVK transform. 8/16-bit unsigned families only; chroma stays 4:4:4
    /// (T.832 has no subsampled YUVK).
    Cmyk,
    /// [`ColorMode::Cmyk`] without the ink→YUVK lifting (`OUT_CMYKDIRECT`):
    /// the four channels code directly (a channel reorder is the only
    /// conversion). Same container formats; the reference toolchain cannot
    /// mint it, so its gates are readback/self-loop strength.
    CmykDirect,
    /// `n` independent channels (3–8, matching the container GUID family;
    /// 8/16-bit unsigned), coded per-component
    /// (`INT_NCOMPONENT`/`OUT_NCOMPONENT`).
    NComponent,
}

/// Errors from the encoder. The split is by WHO can fix the call:
///
/// - [`Invalid`](EncodeError::Invalid) — the input is internally inconsistent
///   or outside what the JPEG XR format can express at all. No encoder could
///   satisfy it; fix the call site.
/// - [`Unsupported`](EncodeError::Unsupported) — a format-expressible request
///   this encoder deliberately does not implement (a capability stance,
///   usually mirroring the reference encoder's own refusal). The message says
///   which.
#[derive(Debug)]
pub enum EncodeError {
    /// Caller-contract violation: plane count/length mismatches, out-of-range
    /// knobs, a malformed [`QpPlan`], or a request the format itself cannot
    /// express (e.g. grayscale-with-alpha — no such container pixel format
    /// exists).
    Invalid(String),
    /// A format-expressible request this encoder takes a stance against
    /// implementing (e.g. chroma-subsampled float input, which the reference
    /// encoder also refuses).
    Unsupported(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Invalid(s) => write!(f, "invalid input: {s}"),
            EncodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Chroma sampling of the encoded codestream for color (3-/4-plane) inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromaSampling {
    /// Full-resolution chroma (4:4:4) — the only lossless-capable choice.
    #[default]
    Yuv444,
    /// Horizontally halved chroma (4:2:2). Lossy by construction.
    Yuv422,
    /// Chroma halved both ways (4:2:0). Lossy by construction.
    Yuv420,
    /// Luma only (`-d 0` analog): chroma dropped; decoders reconstruct
    /// gray R=G=B from the transform's luma.
    YOnly,
}

/// Overlap pre-filtering (T.832 `overlap_mode`, the JxrEncApp `-l` knob):
/// smooths block boundaries before the forward transform; decoders undo it
/// exactly, so lossless stays lossless. Reduces blocking at low bitrates at
/// some high-frequency cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlap {
    /// No overlap filtering (`overlap_mode = 0`) — the byte-stable default.
    #[default]
    None,
    /// One-level (`overlap_mode = 1`): sample-domain filtering only.
    One,
    /// Two-level (`overlap_mode = 2`): sample-domain + block-DC-domain.
    Two,
}

impl Overlap {
    fn code(self) -> u8 {
        use crate::decode::consts::{
            FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING, NO_OVERLAP_FILTERING,
            SECOND_LEVEL_OVERLAP_FILTERING,
        };
        match self {
            Overlap::None => NO_OVERLAP_FILTERING,
            Overlap::One => SECOND_LEVEL_OVERLAP_FILTERING,
            Overlap::Two => FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING,
        }
    }
}

/// Which subbands the codestream carries (T.832 `bands_present`): trailing
/// bands are DROPPED at encode time — a precision/size trade entirely
/// decided by the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandsPresent {
    /// DC + LP + HP + flexbits (everything).
    #[default]
    All,
    /// DC + LP + HP without the flexbits refinement.
    NoFlexbits,
    /// DC + LP only.
    NoHighpass,
    /// DC only.
    DcOnly,
}

impl BandsPresent {
    fn code(self) -> u8 {
        use crate::decode::consts::{ALL_BANDS, DCONLY, NOFLEXBITS, NOHIGHPASS};
        match self {
            BandsPresent::All => ALL_BANDS,
            BandsPresent::NoFlexbits => NOFLEXBITS,
            BandsPresent::NoHighpass => NOHIGHPASS,
            BandsPresent::DcOnly => DCONLY,
        }
    }
}

/// Encoder options beyond the plain [`encode`] surface — the one place a new
/// knob belongs, so the `encode*` signatures stay fixed. `Default` reproduces
/// `encode(…, QpSet::LOSSLESS)` behavior.
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    /// Primary-plane per-band quantizers.
    pub qp: QpSet,
    /// Alpha-plane quantizers (4-plane input); `None` = same as `qp`.
    pub alpha_qp: Option<QpSet>,
    /// Chroma sampling for color inputs (ignored for 1-plane grayscale).
    pub chroma: ChromaSampling,
    /// Subband truncation (`bands_present`).
    pub bands: BandsPresent,
    /// `trim_flexbits` (0–15): drop the low flexbits on emission. Only
    /// meaningful with `bands == All`; ignored otherwise.
    pub trim_flexbits: u8,
    /// Scaled arithmetic (`scaled_flag = 1`): 3 extra fraction bits through
    /// the transforms; chroma DC-LP coded at half amplitude (floor — lossy);
    /// the mode libjxr uses for everything lossy. Exactly invertible for
    /// gray/zero-chroma content; NOT bit-lossless for color at q1.
    pub scaled: bool,
    /// Explicit top window margin (0–63): the coded image gets `window_top`
    /// extra rows above the content (edge-replicated) and the header carries
    /// explicit T.832 window margins (`windowing_flag = 1`) so decoders crop
    /// them. With both 0 (default) the classic derived windowing is used.
    /// Margins are a coded-domain placement knob (e.g. aligning content to
    /// the MB grid); pixels are unaffected.
    pub window_top: u8,
    /// Explicit left window margin (0–63). See [`Self::window_top`].
    pub window_left: u8,
    /// Uniform tile columns (the JxrEncApp `-U` analog): the MB grid splits
    /// into this many near-equal tile columns, each independently entropy-
    /// coded (random access / error resilience; mildly worse compression).
    /// 0 or 1 = single column. More than one tile in either dimension adds
    /// the T.832 index table.
    pub tile_cols: u16,
    /// Uniform tile rows. See [`Self::tile_cols`].
    pub tile_rows: u16,
    /// Overlap pre-filtering level ([`Overlap`]; the JxrEncApp `-l` knob).
    pub overlap: Overlap,
    /// Frequency order (T.832 `frequency_mode`, libjxr's DEFAULT order — its
    /// `-f` turns it off): each tile's bands go into separate byte-aligned
    /// packets (DC/LP/HP/flexbits) addressed by the index table, enabling
    /// progressive/partial decode. Same coefficients, re-segmented stream.
    pub frequency: bool,
    /// Chroma per-band quantizers distinct from the luma `qp` (T.832
    /// `COMP_SEPARATE` component mode — the classic quantize-chroma-harder
    /// rate trick). Applies to the chroma-carrying plane: 3-plane color and
    /// the 4-plane (alpha) path's primary; ignored for grayscale (no chroma
    /// to quantize). `None` = same as `qp` (`COMP_UNIFORM`, byte-stable).
    /// Mutually exclusive with [`qp_plan`](Self::qp_plan).
    pub chroma_qp: Option<QpSet>,
    /// The full T.832 quantization syntax for the primary plane: per-tile
    /// [QP sets](TileQps) and per-MB LP/HP DQUANT index maps ([`QpPlan`]).
    /// `None` (the default) derives the classic single-set plan from
    /// `qp`/`chroma_qp`. `Some` becomes THE quantizer source — `qp` is
    /// unused and `chroma_qp` must stay `None`
    /// ([`Invalid`](EncodeError::Invalid) otherwise).
    ///
    /// Honored on the color-coded paths: 3-plane RGB at any sample depth,
    /// packed RGB, and RGBE, at 4:4:4/4:2:2/4:2:0 chroma. Grayscale,
    /// `YOnly`, alpha-plane, CMYK/N-component and bi-level paths reject it
    /// ([`Unsupported`](EncodeError::Unsupported)) — their QP generality is
    /// the uniform `qp`/`chroma_qp`/`alpha_qp` surface. A plan also
    /// suppresses the auto-gray collapse (identical R==G==B channels encode
    /// as the declared color format rather than silently dropping the
    /// plan's chroma bytes and tile structure).
    ///
    /// Component mode (`COMP_UNIFORM`/`SEPARATE`/`INDEPENDENT`) is derived
    /// per band from each [`BandQp`]'s bytes on emission. More than one
    /// LP/HP set makes that band per-MB DQUANT: each macroblock picks its
    /// set by the index map (empty map = all set 0).
    pub qp_plan: Option<QpPlan>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            qp: QpSet::LOSSLESS,
            alpha_qp: None,
            chroma: ChromaSampling::Yuv444,
            bands: BandsPresent::All,
            trim_flexbits: 0,
            scaled: false,
            window_top: 0,
            window_left: 0,
            tile_cols: 0,
            tile_rows: 0,
            overlap: Overlap::None,
            frequency: false,
            chroma_qp: None,
            qp_plan: None,
        }
    }
}

/// Split `mb` macroblocks into `n` near-equal tile spans (remainder spread
/// over the leading tiles), the uniform-tiling distribution.
fn uniform_tiles(mb: usize, n: usize) -> Vec<usize> {
    let n = n.max(1);
    let (base, rem) = (mb / n, mb % n);
    (0..n).map(|i| base + usize::from(i < rem)).collect()
}

/// Interleaved 8-bit channel orders accepted by [`deinterleave`] — the common
/// memory layouts (`24bppBGR`, `32bppBGRA`, …) normalized to the planar
/// R,G,B(,A) layout [`ImageInput`] takes. Premultiplied variants
/// (PRGBA/PBGRA) are an [`Rgba`](ChannelOrder::Rgba)/[`Bgra`](ChannelOrder::Bgra)
/// byte order plus `ImageInput::premultiplied_alpha = true` — premultiplication
/// is a property flag, not a different layout. The emitted file always uses
/// the canonical GUID for its channel count (`24bppRGB`, `32bppBGRA`/
/// `32bppPBGRA`): byte-order-only GUID variants would change nothing but the
/// order a conformant decoder interleaves its output in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOrder {
    /// One byte per pixel (`8bppGray`).
    Gray,
    /// R,G,B triplets (`24bppRGB` memory order).
    Rgb,
    /// B,G,R triplets (`24bppBGR` memory order).
    Bgr,
    /// R,G,B,A quads (`32bppRGBA` memory order).
    Rgba,
    /// B,G,R,A quads (`32bppBGRA` memory order).
    Bgra,
}

impl ChannelOrder {
    fn channels(self) -> usize {
        match self {
            ChannelOrder::Gray => 1,
            ChannelOrder::Rgb | ChannelOrder::Bgr => 3,
            ChannelOrder::Rgba | ChannelOrder::Bgra => 4,
        }
    }
    /// Normalized plane index (R,G,B,A order) of interleaved position `i`.
    fn plane_of(self, i: usize) -> usize {
        match self {
            ChannelOrder::Gray | ChannelOrder::Rgb | ChannelOrder::Rgba => i,
            ChannelOrder::Bgr | ChannelOrder::Bgra => match i {
                0 => 2,
                2 => 0,
                other => other,
            },
        }
    }
}

/// De-interleave an 8-bit pixel buffer in the given channel `order` into the
/// normalized planar layout [`ImageInput`] takes (R,G,B\[,A\] planes, or a
/// single gray plane). `bytes.len()` must equal `width × height × channels`.
pub fn deinterleave(
    order: ChannelOrder,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<Vec<u8>>, EncodeError> {
    let n = width as usize * height as usize;
    let ch = order.channels();
    if bytes.len() != n * ch {
        return Err(EncodeError::Invalid(format!(
            "interleaved buffer len {} != {width}x{height}x{ch}",
            bytes.len()
        )));
    }
    let mut planes = vec![vec![0u8; n]; ch];
    for (px, chunk) in bytes.chunks_exact(ch).enumerate() {
        for (i, &v) in chunk.iter().enumerate() {
            planes[order.plane_of(i)][px] = v;
        }
    }
    Ok(planes)
}

/// 8-bit pixel input. One plane per component, each row-major with
/// `len == width * height`: **1** plane = grayscale, **3** = RGB, **4** =
/// RGB + alpha (encoded as a T.832 alpha image plane). **2** planes
/// (gray+alpha) is rejected: JPEG XR has no grayscale-with-alpha container
/// pixel format (the channels+alpha GUID family starts at 3 channels), so
/// such a file would be unrepresentable — expand to RGBA. Interleaved
/// buffers in common memory orders normalize via [`deinterleave`].
pub struct ImageInput<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// 1 (gray), 3 (R,G,B) or 4 (R,G,B,A) planes, each `width × height`
    /// row-major.
    pub planes: &'a [Vec<u8>],
    /// The alpha plane (4-plane input) holds premultiplied values. Sets the
    /// codestream `premultiplied_alpha_flag` (a property bit, not a different
    /// coding — T.832 8.3.17) and selects the `32bppPBGRA` container GUID.
    pub premultiplied_alpha: bool,
}

/// Encode 8-bit grayscale/RGB(A) pixels into a JPEG-XR file (TIFF container +
/// WMPHOTO codestream) at the given per-band quantizers. `QpSet::LOSSLESS` is
/// bit-exact; higher QP trades fidelity for size (ship mode). An alpha plane
/// (4-plane input) is quantized with the same `qp` — use
/// [`encode_with_alpha_qp`] to quantize it independently. Output is decodable
/// by `decode` and structurally clones a real Amazon JXR.
pub fn encode(input: &ImageInput<'_>, mode: ColorMode, qp: QpSet) -> Result<Vec<u8>, EncodeError> {
    encode_with_alpha_qp(input, mode, qp, qp)
}

/// [`encode`] with the alpha image plane quantized by its **own** per-band
/// `alpha_qp` (the JxrEncApp `-Q` analog; the plane carries its own QPs in its
/// plane header). Ignored unless the input has a 4th (alpha) plane.
pub fn encode_with_alpha_qp(
    input: &ImageInput<'_>,
    mode: ColorMode,
    qp: QpSet,
    alpha_qp: QpSet,
) -> Result<Vec<u8>, EncodeError> {
    encode_with_options(
        input,
        mode,
        EncodeOptions {
            qp,
            alpha_qp: Some(alpha_qp),
            ..Default::default()
        },
    )
}

/// [`encode`] over the full option surface ([`EncodeOptions`]): chroma
/// sampling (4:4:4 / 4:2:2 / 4:2:0 / luma-only), independent alpha QPs, and
/// scaled arithmetic.
/// The geometry every encode path validates the same way: window margins,
/// padded MB grid, uniform tile split, size caps.
struct Geometry {
    window: (u32, u32),
    tile_cols_mb: Vec<usize>,
    tile_rows_mb: Vec<usize>,
    /// Window-padded MB grid (the shape DQUANT index maps address).
    mb_cols: usize,
    mb_rows: usize,
}

fn validate_geometry(w: u32, h: u32, opts: &EncodeOptions) -> Result<Geometry, EncodeError> {
    if opts.trim_flexbits > 15 {
        return Err(EncodeError::Invalid("trim_flexbits must be 0–15".into()));
    }
    if opts.window_top > 63 || opts.window_left > 63 {
        return Err(EncodeError::Invalid(
            "window margins are 6-bit fields (0–63)".into(),
        ));
    }
    if w == 0 || h == 0 {
        return Err(EncodeError::Invalid("zero-size image".into()));
    }
    if w > 1 << 28 || h > 1 << 28 {
        // The long header carries 32-bit dims, but cap at the sane end of the
        // spec range (the decoder's own decompression budget would reject the
        // grid anyway).
        return Err(EncodeError::Invalid("dims exceed 2^28".into()));
    }
    let window = (opts.window_top as u32, opts.window_left as u32);
    // Tile grid: uniform split of the padded MB grid (which includes the
    // window margins). Each tile must be ≥ 1 MB; counts are 12-bit fields.
    let mb_cols = ((w + window.1).div_ceil(16)) as usize;
    let mb_rows = ((h + window.0).div_ceil(16)) as usize;
    let (tc, tr) = (
        opts.tile_cols.max(1) as usize,
        opts.tile_rows.max(1) as usize,
    );
    if tc > 4096 || tr > 4096 {
        return Err(EncodeError::Invalid(
            "tile counts are 12-bit fields (≤ 4096)".into(),
        ));
    }
    if tc > mb_cols || tr > mb_rows {
        return Err(EncodeError::Invalid(format!(
            "{tc}x{tr} tiles over a {mb_cols}x{mb_rows} MB grid (every tile needs ≥ 1 MB)"
        )));
    }
    let (tile_cols_mb, tile_rows_mb) = if tc > 1 || tr > 1 {
        (uniform_tiles(mb_cols, tc), uniform_tiles(mb_rows, tr))
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(Geometry {
        window,
        tile_cols_mb,
        tile_rows_mb,
        mb_cols,
        mb_rows,
    })
}

/// Shape-check a caller-supplied [`QpPlan`] against the tile grid — the
/// public-surface counterpart of the emission-side debug assertions.
fn validate_qp_plan(p: &QpPlan, geom: &Geometry) -> Result<(), EncodeError> {
    let ntiles = geom.tile_cols_mb.len().max(1) * geom.tile_rows_mb.len().max(1);
    if p.tiles.len() != 1 && p.tiles.len() != ntiles {
        return Err(EncodeError::Invalid(format!(
            "QpPlan carries {} tile entries; expected 1 (image-uniform) or {ntiles} (one per tile)",
            p.tiles.len()
        )));
    }
    let (nlp, nhp) = (p.num_lp_qps(), p.num_hp_qps());
    for t in &p.tiles {
        if !(1..=16).contains(&t.lp.len()) || !(1..=16).contains(&t.hp.len()) {
            return Err(EncodeError::Invalid(
                "each tile needs 1–16 LP and 1–16 HP QP sets (num_qps is a 4-bit field)".into(),
            ));
        }
        if t.lp.len() != nlp || t.hp.len() != nhp {
            return Err(EncodeError::Invalid(
                "every tile must declare the same LP/HP set counts (the per-band num_qps \
                 shape is image-level)"
                    .into(),
            ));
        }
    }
    let nmb = geom.mb_cols * geom.mb_rows;
    let check_index = |idx: &[u8], sets: usize, band: &str| -> Result<(), EncodeError> {
        if idx.is_empty() {
            return Ok(());
        }
        if sets == 1 {
            return Err(EncodeError::Invalid(format!(
                "{band}_index map with a single {band} set would never be read — \
                 drop the map or declare more sets"
            )));
        }
        if idx.len() != nmb {
            return Err(EncodeError::Invalid(format!(
                "{band}_index map has {} entries; the (window-padded) MB grid is {}x{} = {nmb}",
                idx.len(),
                geom.mb_cols,
                geom.mb_rows
            )));
        }
        if let Some(&bad) = idx.iter().find(|&&i| i as usize >= sets) {
            return Err(EncodeError::Invalid(format!(
                "{band}_index entry {bad} out of range (this plan declares {sets} {band} sets)"
            )));
        }
        Ok(())
    };
    check_index(&p.lp_index, nlp, "lp")?;
    check_index(&p.hp_index, nhp, "hp")
}

/// The documented `qp_plan` contract: paths that don't route through the
/// color plane machinery reject an explicit plan instead of silently
/// dropping its tile structure and index maps.
fn reject_qp_plan(opts: &EncodeOptions, path: &str) -> Result<(), EncodeError> {
    if opts.qp_plan.is_some() {
        return Err(EncodeError::Unsupported(format!(
            "qp_plan (per-tile QP sets / DQUANT index maps) is implemented for the \
             color-coded paths only; the {path} path takes the uniform qp"
        )));
    }
    Ok(())
}

/// Resolve the primary-plane QP source for the color-coded paths: an explicit
/// `qp_plan` (validated against the geometry), the `chroma_qp` fold, or `None`
/// (the classic single-set path, byte-stable).
fn resolve_qp_plan(opts: &EncodeOptions, geom: &Geometry) -> Result<Option<QpPlan>, EncodeError> {
    match (&opts.qp_plan, opts.chroma_qp) {
        (Some(_), Some(_)) => Err(EncodeError::Invalid(
            "qp_plan and chroma_qp are mutually exclusive QP sources".into(),
        )),
        (Some(p), None) => {
            validate_qp_plan(p, geom)?;
            Ok(Some(p.clone()))
        }
        (None, Some(c)) => Ok(Some(quant::QpPlan::uniform(opts.qp, Some(c)))),
        (None, None) => Ok(None),
    }
}

/// The full-surface 8-bit encode: [`encode`] plus everything in
/// [`EncodeOptions`] (chroma sampling, band truncation, windowing, tiling,
/// overlap pre-filtering, frequency order, scaled arithmetic, the complete
/// QP syntax). `Default::default()` options reproduce `encode(…)`
/// byte-for-byte. CMYK/N-component [`ColorMode`]s route the planes through
/// the per-component path ([`encode_typed`] accepts them at 16-bit too).
pub fn encode_with_options(
    input: &ImageInput<'_>,
    mode: ColorMode,
    opts: EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    use crate::decode::consts::{INT_YUV420, INT_YUV422, INT_YUV444};
    if matches!(
        mode,
        ColorMode::Cmyk | ColorMode::CmykDirect | ColorMode::NComponent
    ) {
        return encode_multi_typed(
            &SamplePlanes::U8(input.planes),
            input.width,
            input.height,
            input.premultiplied_alpha,
            mode,
            opts,
        );
    }
    let (qp, alpha_qp) = (opts.qp, opts.alpha_qp.unwrap_or(opts.qp));
    let bands = opts.bands.code();
    let (w, h) = (input.width, input.height);
    let geom = validate_geometry(w, h, &opts)?;
    let window = geom.window;
    let tiles: (&[usize], &[usize]) = (&geom.tile_cols_mb, &geom.tile_rows_mb);
    let overlap = opts.overlap.code();
    let frequency = opts.frequency;
    // Explicit qp_plan, or chroma_qp folded into a single-set QpPlan with
    // separate chroma bytes (COMP_SEPARATE emission); None keeps the classic
    // plan-free path.
    let owned_plan = resolve_qp_plan(&opts, &geom)?;
    let plan = owned_plan.as_ref();
    let n = w as usize * h as usize;
    let check = |p: &[u8]| {
        if p.len() == n {
            Ok(())
        } else {
            Err(EncodeError::Invalid("plane len != width*height".into()))
        }
    };
    if input.planes.len() == 2 {
        return Err(EncodeError::Invalid(
            "gray+alpha: JPEG XR has no grayscale-with-alpha container pixel format; \
             supply RGBA (4 planes)"
                .into(),
        ));
    }
    if input.premultiplied_alpha && input.planes.len() != 4 {
        return Err(EncodeError::Invalid(
            "premultiplied_alpha set but no alpha plane (4 planes)".into(),
        ));
    }
    if input.planes.len() == 4 {
        if mode != ColorMode::Color {
            return Err(EncodeError::Invalid(
                "alpha requires ColorMode::Color (no gray+alpha pixel format exists)".into(),
            ));
        }
        for p in input.planes {
            check(p)?;
        }
        if opts.chroma == ChromaSampling::YOnly {
            return Err(EncodeError::Unsupported(
                "YOnly chroma with an alpha plane is not implemented".into(),
            ));
        }
        if opts.bands != BandsPresent::All || opts.trim_flexbits != 0 {
            return Err(EncodeError::Unsupported(
                "band truncation / trim_flexbits with an alpha plane is not implemented".into(),
            ));
        }
        reject_qp_plan(&opts, "alpha-plane")?;
        let fmt = match opts.chroma {
            ChromaSampling::Yuv422 => INT_YUV422,
            ChromaSampling::Yuv420 => INT_YUV420,
            _ => INT_YUV444,
        };
        // No auto-gray collapse here even when R==G==B: gray+alpha has no
        // container pixel format, so collapsing would change the declared
        // format — and all-zero chroma planes cost next to nothing.
        return Ok(color::encode_color_alpha(
            &input.planes[0],
            &input.planes[1],
            &input.planes[2],
            &input.planes[3],
            w,
            h,
            qp,
            alpha_qp,
            opts.chroma_qp,
            input.premultiplied_alpha,
            fmt,
            opts.scaled,
            window,
            tiles,
            overlap,
            frequency,
        ));
    }
    // A 1-plane input is grayscale regardless of mode — there's no chroma to
    // synthesize. Color auto-gray-detect (R==G==B) lives in the pipeline.
    let want_color = mode == ColorMode::Color && input.planes.len() != 1;
    if !want_color {
        if input.planes.len() != 1 {
            return Err(EncodeError::Invalid(format!(
                "grayscale expects 1 plane, got {}",
                input.planes.len()
            )));
        }
        check(&input.planes[0])?;
        reject_qp_plan(&opts, "grayscale")?;
        // QpSet::LOSSLESS at All bands ⇒ bit-exact; truncation/trim ⇒ lossy.
        return Ok(gray::encode_grayscale_options(
            &input.planes[0],
            w,
            h,
            qp,
            opts.scaled,
            bands,
            opts.trim_flexbits,
            window,
            tiles,
            overlap,
            frequency,
        ));
    }
    if input.planes.len() != 3 {
        return Err(EncodeError::Invalid(format!(
            "color expects 3 planes (RGB), got {}",
            input.planes.len()
        )));
    }
    check(&input.planes[0])?;
    check(&input.planes[1])?;
    check(&input.planes[2])?;
    // Auto-gray: a "color" image whose channels are identical everywhere carries
    // no chroma — emit `8bppGray` (smaller; the chroma planes would be all-zero;
    // the chroma-sampling choice is moot on a gray image). An explicit qp_plan
    // suppresses the collapse: the gray path can't honor a plan, and silently
    // dropping it would be worse than the few bytes of zero chroma.
    if opts.qp_plan.is_none()
        && input.planes[0] == input.planes[1]
        && input.planes[1] == input.planes[2]
    {
        return Ok(gray::encode_grayscale_options(
            &input.planes[0],
            w,
            h,
            qp,
            opts.scaled,
            bands,
            opts.trim_flexbits,
            window,
            tiles,
            overlap,
            frequency,
        ));
    }
    let (r, g, b) = (&input.planes[0], &input.planes[1], &input.planes[2]);
    let trim = opts.trim_flexbits;
    match opts.chroma {
        ChromaSampling::YOnly if opts.bands == BandsPresent::All && trim == 0 => {
            reject_qp_plan(&opts, "YOnly")?;
            Ok(color::encode_yonly_from_color(
                r,
                g,
                b,
                w,
                h,
                qp,
                opts.scaled,
                window,
                tiles,
                overlap,
                frequency,
            ))
        }
        ChromaSampling::YOnly => Err(EncodeError::Unsupported(
            "band truncation / trim with YOnly chroma is not implemented".into(),
        )),
        ChromaSampling::Yuv444
            if !opts.scaled
                && opts.bands == BandsPresent::All
                && trim == 0
                && window == (0, 0)
                && tiles.0.is_empty()
                && opts.overlap == Overlap::None
                && !frequency
                && plan.is_none() =>
        {
            // The classic byte-stable path (clone of the original encoder).
            Ok(color::encode_color(r, g, b, w, h, qp))
        }
        ChromaSampling::Yuv444 => Ok(color::encode_color_options(
            r,
            g,
            b,
            w,
            h,
            qp,
            INT_YUV444,
            bands,
            opts.scaled,
            trim,
            window,
            tiles,
            overlap,
            frequency,
            plan,
        )),
        ChromaSampling::Yuv422 => Ok(color::encode_color_options(
            r,
            g,
            b,
            w,
            h,
            qp,
            INT_YUV422,
            bands,
            opts.scaled,
            trim,
            window,
            tiles,
            overlap,
            frequency,
            plan,
        )),
        ChromaSampling::Yuv420 => Ok(color::encode_color_options(
            r,
            g,
            b,
            w,
            h,
            qp,
            INT_YUV420,
            bands,
            opts.scaled,
            trim,
            window,
            tiles,
            overlap,
            frequency,
            plan,
        )),
    }
}

/// Packed-RGB dispatch (`Packed555`/`Packed565`/`Packed101010`): one plane
/// of packed words → three pre-bias channels → the standard color path with
/// the packed depth + GUID. Chroma subsampling and YOnly apply as for any
/// RGB input; lossless QP round-trips the packed words exactly.
fn encode_packed(
    samples: &SamplePlanes<'_>,
    w: u32,
    h: u32,
    premultiplied_alpha: bool,
    mode: ColorMode,
    opts: EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    use crate::decode::consts::{INT_YUV420, INT_YUV422, INT_YUV444, OUT_RGB};
    if mode != ColorMode::Color {
        return Err(EncodeError::Invalid(
            "packed RGB input is inherently color".into(),
        ));
    }
    if premultiplied_alpha {
        return Err(EncodeError::Invalid("packed RGB has no alpha plane".into()));
    }
    if samples.num_planes() != 1 {
        return Err(EncodeError::Invalid(format!(
            "packed input is ONE plane of packed words, got {} planes",
            samples.num_planes()
        )));
    }
    let geom = validate_geometry(w, h, &opts)?;
    let window = geom.window;
    let tiles: (&[usize], &[usize]) = (&geom.tile_cols_mb, &geom.tile_rows_mb);
    let (overlap, frequency) = (opts.overlap.code(), opts.frequency);
    let (qp, bands, trim) = (opts.qp, opts.bands.code(), opts.trim_flexbits);
    if samples.plane_len(0) != w as usize * h as usize {
        return Err(EncodeError::Invalid("plane len != width*height".into()));
    }
    let depth = samples.depth();
    let guid = samples.rgb_guid();
    let (rp, gp, bp) = convert::packed_prebias(samples, opts.scaled);
    let owned_plan = resolve_qp_plan(&opts, &geom)?;
    match opts.chroma {
        ChromaSampling::YOnly if opts.bands == BandsPresent::All && trim == 0 => {
            reject_qp_plan(&opts, "YOnly")?;
            Ok(color::encode_yonly_prebias(
                &rp,
                &gp,
                &bp,
                w,
                h,
                qp,
                opts.scaled,
                window,
                tiles,
                overlap,
                frequency,
                &depth,
                guid,
            ))
        }
        ChromaSampling::YOnly => Err(EncodeError::Unsupported(
            "band truncation / trim with YOnly chroma is not implemented".into(),
        )),
        chroma => {
            let fmt = match chroma {
                ChromaSampling::Yuv422 => INT_YUV422,
                ChromaSampling::Yuv420 => INT_YUV420,
                _ => INT_YUV444,
            };
            Ok(color::encode_color_prebias(
                &rp,
                &gp,
                &bp,
                w,
                h,
                qp,
                fmt,
                bands,
                opts.scaled,
                trim,
                window,
                tiles,
                overlap,
                frequency,
                owned_plan.as_ref(),
                &depth,
                guid,
                OUT_RGB,
            ))
        }
    }
}

/// Bi-level dispatch (`Bw`/`BwBlackIsOne`): one 0/1 plane through the gray
/// path with BD1WHITE1/BD1BLACK1 in the image header. Values above 1 are
/// rejected (T.832 bi-level range; the decoder clips to it).
fn encode_bw(
    samples: &SamplePlanes<'_>,
    w: u32,
    h: u32,
    premultiplied_alpha: bool,
    mode: ColorMode,
    opts: EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    if mode != ColorMode::Grayscale {
        return Err(EncodeError::Invalid(
            "bi-level input is inherently grayscale".into(),
        ));
    }
    if premultiplied_alpha {
        return Err(EncodeError::Invalid("bi-level has no alpha plane".into()));
    }
    if samples.num_planes() != 1 {
        return Err(EncodeError::Invalid(format!(
            "bi-level input is ONE plane, got {}",
            samples.num_planes()
        )));
    }
    if samples.plane_len(0) != w as usize * h as usize {
        return Err(EncodeError::Invalid("plane len != width*height".into()));
    }
    let bad = match samples {
        SamplePlanes::Bw(p) | SamplePlanes::BwBlackIsOne(p) => p[0].iter().any(|&v| v > 1),
        _ => unreachable!(),
    };
    if bad {
        return Err(EncodeError::Invalid(
            "bi-level values must be 0 or 1".into(),
        ));
    }
    let geom = validate_geometry(w, h, &opts)?;
    let window = geom.window;
    let tiles: (&[usize], &[usize]) = (&geom.tile_cols_mb, &geom.tile_rows_mb);
    let (overlap, frequency) = (opts.overlap.code(), opts.frequency);
    let (qp, bands, trim) = (opts.qp, opts.bands.code(), opts.trim_flexbits);
    reject_qp_plan(&opts, "bi-level (grayscale)")?;
    let pre = samples.prebias_plane(0, opts.scaled);
    Ok(gray::encode_gray_prebias(
        &pre,
        w,
        h,
        qp,
        opts.scaled,
        bands,
        trim,
        window,
        tiles,
        overlap,
        frequency,
        &samples.depth(),
        samples.gray_guid(),
    ))
}

/// The CMYK / CMYKDIRECT / NCOMPONENT dispatch shared by
/// [`encode_with_options`] (8-bit) and [`encode_typed`] (any depth): the
/// per-component multi-channel path (`encode::multi`).
fn encode_multi_typed(
    samples: &SamplePlanes<'_>,
    w: u32,
    h: u32,
    premultiplied_alpha: bool,
    mode: ColorMode,
    opts: EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    use crate::decode::consts::{
        INT_NCOMPONENT, INT_YUVK, OUT_CMYK, OUT_CMYKDIRECT, OUT_NCOMPONENT,
    };
    let deep = match samples {
        SamplePlanes::U8(_) => false,
        SamplePlanes::U16(_) => true,
        _ => {
            return Err(EncodeError::Invalid(
                "CMYK/N-component container formats exist for the 8- and 16-bit \
                 unsigned families only"
                    .into(),
            ));
        }
    };
    if !matches!(opts.chroma, ChromaSampling::Yuv444) {
        return Err(EncodeError::Invalid(
            "CMYK/N-component input keeps YUV 4:4:4 (no subsampled YUVK/NCOMPONENT \
             exists in T.832)"
                .into(),
        ));
    }
    if premultiplied_alpha {
        return Err(EncodeError::Invalid(
            "no premultiplied container pixel format exists for CMYK/N-component".into(),
        ));
    }
    let geom = validate_geometry(w, h, &opts)?;
    let window = geom.window;
    let tiles: (&[usize], &[usize]) = (&geom.tile_cols_mb, &geom.tile_rows_mb);
    let (overlap, frequency) = (opts.overlap.code(), opts.frequency);
    let (qp, bands, trim) = (opts.qp, opts.bands.code(), opts.trim_flexbits);
    reject_qp_plan(&opts, "CMYK/N-component (per-component)")?;
    let chroma_qp = opts.chroma_qp.unwrap_or(qp);
    let n = w as usize * h as usize;
    let np = samples.num_planes();
    for i in 0..np {
        if samples.plane_len(i) != n {
            return Err(EncodeError::Invalid("plane len != width*height".into()));
        }
    }
    let cmyk = matches!(mode, ColorMode::Cmyk | ColorMode::CmykDirect);
    let (nch, has_alpha) = if cmyk {
        match np {
            4 => (4, false),
            5 => (4, true),
            other => {
                return Err(EncodeError::Invalid(format!(
                    "CMYK expects 4 planes (C,M,Y,K) or 5 (+alpha), got {other}"
                )));
            }
        }
    } else {
        // N-component: the plane count IS the channel count, so an alpha
        // plane would be ambiguous — not offered (the decoder reads
        // n-channel-with-alpha files fine; encoding one has no use).
        match np {
            3..=8 => (np, false),
            other => {
                return Err(EncodeError::Invalid(format!(
                    "N-component expects 3–8 channel planes (the container GUID \
                     family stops at 8 channels; alpha is not offered), got {other}"
                )));
            }
        }
    };
    if has_alpha && (opts.bands != BandsPresent::All || trim != 0) {
        return Err(EncodeError::Unsupported(
            "band truncation / trim_flexbits with an alpha plane is not implemented".into(),
        ));
    }
    let depth = samples.depth();
    // Forward conversion per mode; the alpha plane (if any) takes the plain
    // family bias.
    let comps: Vec<Vec<i32>> = match mode {
        ColorMode::Cmyk => convert::cmyk_prebias(samples, opts.scaled),
        ColorMode::CmykDirect => convert::cmykdirect_prebias(samples, opts.scaled),
        _ => (0..nch)
            .map(|i| samples.prebias_plane(i, opts.scaled))
            .collect(),
    };
    let alpha_plane = has_alpha.then(|| samples.prebias_plane(nch, opts.scaled));
    let alpha = alpha_plane
        .as_ref()
        .map(|a| (&a[..], opts.alpha_qp.unwrap_or(qp)));
    let (int_fmt, out_fmt) = match mode {
        ColorMode::Cmyk => (INT_YUVK, OUT_CMYK),
        ColorMode::CmykDirect => (INT_YUVK, OUT_CMYKDIRECT),
        _ => (INT_NCOMPONENT, OUT_NCOMPONENT),
    };
    let guid = match mode {
        ColorMode::Cmyk | ColorMode::CmykDirect => match (deep, has_alpha) {
            (false, false) => container::pixel_format::CMYK32,
            (true, false) => container::pixel_format::CMYK64,
            (false, true) => container::pixel_format::CMYKA40,
            (true, true) => container::pixel_format::CMYKA80,
        },
        _ => container::pixel_format::nchannel(nch, deep, has_alpha),
    };
    Ok(multi::encode_multi_prebias(
        &comps,
        w,
        h,
        qp,
        chroma_qp,
        alpha,
        int_fmt,
        out_fmt,
        bands,
        opts.scaled,
        trim,
        window,
        tiles,
        overlap,
        frequency,
        &depth,
        &guid,
    ))
}

/// Typed (any-depth) pixel input for [`encode_typed`]: the deep-format
/// counterpart of [`ImageInput`]. Plane count carries the color shape exactly
/// as in the 8-bit API (**1** = grayscale, **3** = RGB; **4** = RGB + alpha,
/// 8-bit only); the [`SamplePlanes`] variant carries the depth.
pub struct TypedInput<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The typed planes; the variant carries the sample depth.
    pub samples: SamplePlanes<'a>,
    /// See [`ImageInput::premultiplied_alpha`] (4-plane inputs only).
    pub premultiplied_alpha: bool,
}

/// [`encode_with_options`] over typed planes ([`TypedInput`]): encode 8-bit
/// **or deep** pixels. `U8` routes through the classic 8-bit path
/// byte-for-byte; `U16`/`I16` (BD16/BD16S) add `16bppGray` / `48bppRGB` /
/// `…Fixed` and are bit-exact at `QpSet::LOSSLESS`; `I32` (BD32S,
/// `32bppGrayFixed` / `96bppRGBFixed`) sheds its 10 low bits on input
/// (`shift_bits` — reference-encoder behavior; q1 round-trips
/// `(x >> 10) << 10`) and rejects scaled arithmetic (`i32` transform
/// headroom; libjxr forces unscaled there too). Everything structural in
/// [`EncodeOptions`] — chroma sampling, bands/trim, windowing, tiles,
/// overlap, frequency order, `chroma_qp` — applies at any depth.
pub fn encode_typed(
    input: &TypedInput<'_>,
    mode: ColorMode,
    opts: EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    use crate::decode::consts::{INT_YUV420, INT_YUV422, INT_YUV444};
    let samples = &input.samples;
    let (w, h) = (input.width, input.height);
    if matches!(
        mode,
        ColorMode::Cmyk | ColorMode::CmykDirect | ColorMode::NComponent
    ) {
        return encode_multi_typed(samples, w, h, input.premultiplied_alpha, mode, opts);
    }
    if matches!(
        samples,
        SamplePlanes::Packed555(_) | SamplePlanes::Packed565(_) | SamplePlanes::Packed101010(_)
    ) {
        return encode_packed(samples, w, h, input.premultiplied_alpha, mode, opts);
    }
    if matches!(samples, SamplePlanes::Bw(_) | SamplePlanes::BwBlackIsOne(_)) {
        return encode_bw(samples, w, h, input.premultiplied_alpha, mode, opts);
    }
    if let SamplePlanes::U8(planes) = samples {
        return encode_with_options(
            &ImageInput {
                width: w,
                height: h,
                planes,
                premultiplied_alpha: input.premultiplied_alpha,
            },
            mode,
            opts,
        );
    }
    if matches!(samples, SamplePlanes::I32(_) | SamplePlanes::F32(_)) && opts.scaled {
        return Err(EncodeError::Unsupported(
            "scaled arithmetic with 32-bit input would overflow the i32 transform \
             (the reference encoder forces unscaled here too); set scaled = false"
                .into(),
        ));
    }
    // Float and RGBE inputs code through the pseudo-integer folds, where
    // chroma decimation has no sensible semantics (and RGBE's shared
    // exponent couples the channels per pixel) — the reference encoder
    // refuses the combination too (strenc.c: "Float or RGBE images must be
    // encoded with YUV 444!").
    if matches!(
        samples,
        SamplePlanes::F16(_) | SamplePlanes::F32(_) | SamplePlanes::Rgbe(_)
    ) && matches!(
        opts.chroma,
        ChromaSampling::Yuv420 | ChromaSampling::Yuv422 | ChromaSampling::YOnly
    ) {
        return Err(EncodeError::Unsupported(
            "float/RGBE input must keep YUV 4:4:4 chroma (folded samples survive \
             neither decimation nor a YUV-style luma projection; the reference \
             encoder enforces the same)"
                .into(),
        ));
    }
    let geom = validate_geometry(w, h, &opts)?;
    let window = geom.window;
    let tiles: (&[usize], &[usize]) = (&geom.tile_cols_mb, &geom.tile_rows_mb);
    let (overlap, frequency) = (opts.overlap.code(), opts.frequency);
    let (qp, bands, trim) = (opts.qp, opts.bands.code(), opts.trim_flexbits);
    let depth = samples.depth();
    let n = w as usize * h as usize;
    let np = samples.num_planes();
    for i in 0..np {
        if samples.plane_len(i) != n {
            return Err(EncodeError::Invalid("plane len != width*height".into()));
        }
    }
    if let SamplePlanes::Rgbe(planes) = samples {
        use crate::decode::consts::{INT_YUV444, OUT_RGBE};
        if np != 4 {
            return Err(EncodeError::Invalid(format!(
                "RGBE expects exactly 4 planes (R, G, B, shared exponent), got {np}"
            )));
        }
        if mode != ColorMode::Color {
            return Err(EncodeError::Invalid("RGBE is inherently color".into()));
        }
        if input.premultiplied_alpha {
            return Err(EncodeError::Invalid("RGBE has no alpha plane".into()));
        }
        if opts.bands != BandsPresent::All || trim != 0 {
            return Err(EncodeError::Unsupported(
                "band truncation / trim_flexbits with RGBE is not implemented".into(),
            ));
        }
        let (rp, gp, bp) = convert::rgbe_prebias(planes, opts.scaled);
        let owned_plan = resolve_qp_plan(&opts, &geom)?;
        return Ok(color::encode_color_prebias(
            &rp,
            &gp,
            &bp,
            w,
            h,
            qp,
            INT_YUV444,
            bands,
            opts.scaled,
            0,
            window,
            tiles,
            overlap,
            frequency,
            owned_plan.as_ref(),
            &depth,
            samples.rgb_guid(),
            OUT_RGBE,
        ));
    }
    match np {
        2 => {
            return Err(EncodeError::Invalid(
                "gray+alpha: JPEG XR has no grayscale-with-alpha container pixel format; \
                 supply RGBA (4 planes)"
                    .into(),
            ));
        }
        4 => {
            use crate::decode::consts::{INT_YUV420, INT_YUV422, INT_YUV444};
            if mode != ColorMode::Color {
                return Err(EncodeError::Invalid(
                    "alpha requires ColorMode::Color (no gray+alpha pixel format exists)".into(),
                ));
            }
            if opts.chroma == ChromaSampling::YOnly {
                return Err(EncodeError::Unsupported(
                    "YOnly chroma with an alpha plane is not implemented".into(),
                ));
            }
            if opts.bands != BandsPresent::All || trim != 0 {
                return Err(EncodeError::Unsupported(
                    "band truncation / trim_flexbits with an alpha plane is not implemented".into(),
                ));
            }
            let Some(guid) = samples.rgba_guid(input.premultiplied_alpha) else {
                return Err(EncodeError::Invalid(
                    "no premultiplied container pixel format exists for this sample \
                     family (only the unsigned-16 and float-32 families have one)"
                        .into(),
                ));
            };
            reject_qp_plan(&opts, "alpha-plane")?;
            let fmt = match opts.chroma {
                ChromaSampling::Yuv422 => INT_YUV422,
                ChromaSampling::Yuv420 => INT_YUV420,
                _ => INT_YUV444,
            };
            let rp = samples.prebias_plane(0, opts.scaled);
            let gp = samples.prebias_plane(1, opts.scaled);
            let bp = samples.prebias_plane(2, opts.scaled);
            let ap = samples.prebias_plane(3, opts.scaled);
            return Ok(color::encode_color_alpha_prebias(
                &rp,
                &gp,
                &bp,
                &ap,
                w,
                h,
                qp,
                opts.alpha_qp.unwrap_or(qp),
                opts.chroma_qp,
                input.premultiplied_alpha,
                fmt,
                opts.scaled,
                window,
                tiles,
                overlap,
                frequency,
                &depth,
                guid,
            ));
        }
        1 | 3 => {}
        other => {
            return Err(EncodeError::Invalid(format!(
                "expected 1 (gray), 3 (RGB) or 4 (RGBA) planes, got {other}"
            )));
        }
    }
    if input.premultiplied_alpha {
        return Err(EncodeError::Invalid(
            "premultiplied_alpha set but no alpha plane (4 planes)".into(),
        ));
    }
    let want_color = mode == ColorMode::Color && np != 1;
    if !want_color {
        if np != 1 {
            return Err(EncodeError::Invalid(format!(
                "grayscale expects 1 plane, got {np}"
            )));
        }
        reject_qp_plan(&opts, "grayscale")?;
        let pre = samples.prebias_plane(0, opts.scaled);
        return Ok(gray::encode_gray_prebias(
            &pre,
            w,
            h,
            qp,
            opts.scaled,
            bands,
            trim,
            window,
            tiles,
            overlap,
            frequency,
            &depth,
            samples.gray_guid(),
        ));
    }
    let rp = samples.prebias_plane(0, opts.scaled);
    let gp = samples.prebias_plane(1, opts.scaled);
    let bp = samples.prebias_plane(2, opts.scaled);
    // Auto-gray, in the pre-bias domain: channels the emitted codestream
    // could not distinguish (identical after conversion — for BD32S that is
    // equality of the surviving high bits) carry no chroma → emit the
    // family's gray format, mirroring the 8-bit auto-gray (and like it,
    // suppressed by an explicit qp_plan).
    if opts.qp_plan.is_none() && rp == gp && gp == bp {
        return Ok(gray::encode_gray_prebias(
            &rp,
            w,
            h,
            qp,
            opts.scaled,
            bands,
            trim,
            window,
            tiles,
            overlap,
            frequency,
            &depth,
            samples.gray_guid(),
        ));
    }
    let owned_plan = resolve_qp_plan(&opts, &geom)?;
    let plan = owned_plan.as_ref();
    match opts.chroma {
        ChromaSampling::YOnly if opts.bands == BandsPresent::All && trim == 0 => {
            reject_qp_plan(&opts, "YOnly")?;
            Ok(color::encode_yonly_prebias(
                &rp,
                &gp,
                &bp,
                w,
                h,
                qp,
                opts.scaled,
                window,
                tiles,
                overlap,
                frequency,
                &depth,
                samples.rgb_guid(),
            ))
        }
        ChromaSampling::YOnly => Err(EncodeError::Unsupported(
            "band truncation / trim with YOnly chroma is not implemented".into(),
        )),
        chroma => {
            let fmt = match chroma {
                ChromaSampling::Yuv422 => INT_YUV422,
                ChromaSampling::Yuv420 => INT_YUV420,
                _ => INT_YUV444,
            };
            Ok(color::encode_color_prebias(
                &rp,
                &gp,
                &bp,
                w,
                h,
                qp,
                fmt,
                bands,
                opts.scaled,
                trim,
                window,
                tiles,
                overlap,
                frequency,
                plan,
                &depth,
                samples.rgb_guid(),
                crate::decode::consts::OUT_RGB,
            ))
        }
    }
}

/// Map a 0–100 quality knob to per-band quantizers. 100 ⇒ lossless; lower ⇒
/// coarser, with HP quantized hardest (the `1:2:4` dc:lp:hp ratio Amazon-style).
/// Tuned so the mid-80s land near Amazon's per-plate size on LN content.
pub fn quality_to_qp(quality: u8) -> QpSet {
    if quality >= 100 {
        return QpSet::LOSSLESS;
    }
    let base = (((100 - quality as i32) + 2) / 3).clamp(1, 40) as u8;
    QpSet {
        dc: base,
        lp: base.saturating_mul(2),
        hp: base.saturating_mul(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip oracle: decode JXR bytes straight to i32 planes via the
    /// decoder, bypassing its JPEG re-encode in `jxr_transcode::transcode`.
    fn decode_to_planes(jxr: &[u8]) -> crate::decode::decoder::DecodedImage {
        let container = crate::decode::container::parse(jxr).expect("container parse");
        crate::decode::decoder::Decoder::new(container.image_data)
            .decode()
            .expect("decode")
    }

    #[test]
    fn roundtrip_constant_grayscale_dconly() {
        // DCONLY reconstructs a flat block exactly (LP/HP zero). Across several
        // sizes this also exercises multi-MB DC prediction + model/abs-table
        // adaptation: MB(0,0) codes the full DC, every other MB predicts to 0.
        for &(w, h) in &[(16u32, 16u32), (32, 16), (16, 32), (48, 32)] {
            for val in [128u8, 0, 255, 64, 100, 200] {
                let plane = vec![val; (w * h) as usize];
                let input = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&plane),
                    premultiplied_alpha: false,
                };
                let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
                let decoded = decode_to_planes(&jxr);
                assert_eq!((decoded.width, decoded.height), (w, h));
                for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                    assert_eq!(got, val as i32, "w={w} h={h} val={val} pixel {i}");
                }
            }
        }
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    /// Exact inverse of `transform::forward_transform_mb` (no overlap).
    fn inverse_transform_mb(buf: &mut [i32; 256]) -> [i32; 256] {
        use crate::decode::consts::MB_PIXEL_MAP;
        use crate::decode::math::{str_idct4x4_stage1, str_idct4x4_stage2};
        let mut dclp = [0i32; 16];
        for j in 0..16 {
            dclp[j] = buf[j * 16];
        }
        str_idct4x4_stage2(&mut dclp);
        for j in 0..16 {
            buf[j * 16] = dclp[j];
        }
        for j in 0..16 {
            let mut blk = [0i32; 16];
            blk.copy_from_slice(&buf[j * 16..j * 16 + 16]);
            str_idct4x4_stage1(&mut blk);
            buf[j * 16..j * 16 + 16].copy_from_slice(&blk);
        }
        let mut s = [0i32; 256];
        for by in 0..4 {
            for bx in 0..4 {
                let bb = by * 16 + bx * 64;
                for py in 0..4 {
                    for px in 0..4 {
                        s[(by * 4 + py) * 16 + bx * 4 + px] = buf[bb + MB_PIXEL_MAP[px + py * 4]];
                    }
                }
            }
        }
        s
    }

    /// A zero-HP 16×16 block from random pixels: forward, drop HP, inverse.
    /// `None` if any reconstructed pixel would clip (rare; caller retries).
    fn zero_hp_block(r: &mut Lcg) -> Option<[u8; 256]> {
        let mut samples = [0i32; 256];
        for s in samples.iter_mut() {
            *s = (r.next() % 41) as i32 - 20;
        }
        let mut buf = transform::forward_transform_mb(&samples);
        for (p, v) in buf.iter_mut().enumerate() {
            if p % 16 != 0 {
                *v = 0; // drop HP, keep the per-block DC at [k*16]
            }
        }
        let s = inverse_transform_mb(&mut buf);
        let mut out = [0u8; 256];
        for (o, &v) in out.iter_mut().zip(s.iter()) {
            let p = v + 128;
            if !(0..=255).contains(&p) {
                return None;
            }
            *o = p as u8;
        }
        Some(out)
    }

    #[test]
    fn roundtrip_zero_hp_grayscale_nohighpass() {
        // NOHIGHPASS is lossless for zero-HP content: exercises the full LP
        // run-level + refine + prediction path across multi-MB sizes.
        let mut r = Lcg(0xabcd_ef01);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 2)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let mut pixels = vec![0u8; w * h];
            for mx in 0..mbw {
                for my in 0..mbh {
                    let blk = loop {
                        if let Some(b) = zero_hp_block(&mut r) {
                            break b;
                        }
                    };
                    for py in 0..16 {
                        for px in 0..16 {
                            pixels[(my * 16 + py) * w + (mx * 16 + px)] = blk[py * 16 + px];
                        }
                    }
                }
            }
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "mbw={mbw} mbh={mbh} pixel {i}");
            }
        }
    }

    #[test]
    fn roundtrip_arbitrary_grayscale_allbands_lossless() {
        // The real goal: ANY grayscale image round-trips exactly (ALL_BANDS).
        let mut r = Lcg(0x5151_2727);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 3)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let pixels: Vec<u8> = (0..w * h).map(|_| (r.next() % 256) as u8).collect();
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "mbw={mbw} mbh={mbh} pixel {i}");
            }
        }
    }

    #[test]
    fn roundtrip_non_aligned_grayscale_lossless() {
        // Arbitrary (non-16-aligned) dimensions: edge-pad + decoder crop.
        let mut r = Lcg(0x9090_3434);
        for &(w, h) in &[
            (17u32, 31u32),
            (100, 50),
            (33, 16),
            (16, 33),
            (45, 45),
            (1, 1),
        ] {
            let pixels: Vec<u8> = (0..(w * h) as usize)
                .map(|_| (r.next() % 256) as u8)
                .collect();
            let input = ImageInput {
                width: w,
                height: h,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            assert_eq!((decoded.width, decoded.height), (w, h));
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "w={w} h={h} pixel {i}");
            }
        }
    }

    #[test]
    fn lossy_roundtrip_is_a_fixpoint() {
        // Lossy correctness without an external oracle: a decoded image already
        // sits on the quantization grid, so re-encoding it must yield a
        // byte-identical JXR (encode∘decode∘encode is a fixpoint). This holds
        // iff our forward quantizer is the exact inverse of the decoder's
        // dequant for every band. Mid-range pixels keep the reconstruction in
        // [0,255] so no clamping perturbs the second generation. Aligned sizes
        // only: windowing regenerates edge-padding from the (now lossy) edge, so
        // boundary MBs of a padded image aren't a fixpoint — a property of the
        // padding, not the quantizer.
        let mut r = Lcg(0x1357_9bdf);
        let qps = [
            QpSet {
                dc: 4,
                lp: 8,
                hp: 16,
            },
            QpSet {
                dc: 8,
                lp: 16,
                hp: 32,
            },
            QpSet {
                dc: 1,
                lp: 4,
                hp: 6,
            },
        ];
        for &(w, h) in &[(32u32, 32u32), (48, 32), (64, 48)] {
            let pixels: Vec<u8> = (0..(w * h) as usize)
                .map(|_| 96 + (r.next() % 64) as u8)
                .collect();
            for &qp in &qps {
                let input = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&pixels),
                    premultiplied_alpha: false,
                };
                let jxr1 = encode(&input, ColorMode::Grayscale, qp).expect("encode");
                let dec1 = decode_to_planes(&jxr1);
                let p1: Vec<u8> = dec1.image_plane[0]
                    .iter()
                    .map(|&v| v.clamp(0, 255) as u8)
                    .collect();
                let input2 = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&p1),
                    premultiplied_alpha: false,
                };
                let jxr2 = encode(&input2, ColorMode::Grayscale, qp).expect("re-encode");
                assert_eq!(jxr1, jxr2, "not a fixpoint at qp={qp:?} {w}x{h}");
            }
        }
    }

    #[test]
    fn lossy_error_grows_with_qp() {
        // Clean synthetic original with energy in every band: coarser quant ⇒
        // strictly more error. The fixpoint test only proves self-consistency;
        // this rules out a deadzone/rounding bug that would still round-trip but
        // quantize badly. (Monotonic on a clean master — unlike PSNR vs Amazon's
        // own already-quantized pixels.)
        let (w, h) = (64usize, 64usize);
        let pixels: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                let v = 110 + (x % 17) - (y % 13) + ((x * y) % 11) * 3; // LP + HP energy
                v.clamp(0, 255) as u8
            })
            .collect();
        let mse = |qp: QpSet| -> f64 {
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, qp).expect("encode");
            let d = decode_to_planes(&jxr);
            let se: f64 = pixels
                .iter()
                .zip(d.image_plane[0].iter())
                .map(|(&r, &g)| {
                    let e = r as f64 - g.clamp(0, 255) as f64;
                    e * e
                })
                .sum();
            se / (w * h) as f64
        };
        let m0 = mse(QpSet::LOSSLESS);
        let m4 = mse(QpSet {
            dc: 16,
            lp: 16,
            hp: 16,
        }); // sf = 4
        let m8 = mse(QpSet {
            dc: 32,
            lp: 32,
            hp: 32,
        }); // sf = 8
        assert_eq!(m0, 0.0, "lossless must be exact");
        assert!(
            m4 > 0.0 && m8 > m4,
            "error must grow with QP: m0={m0} m4={m4} m8={m8}"
        );
    }

    #[test]
    fn encode_color_via_public_api() {
        // 3-plane Color round-trips exact; 1-plane Color falls back to grayscale.
        let mut r = Lcg(0x1111_2222_3333_4444);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // High bits: the LCG's low 8 bits alias at period 256, which would make
        // R==G==B (zero chroma) when n is a multiple of 256.
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [rp.clone(), gp.clone(), bp.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).expect("color encode");
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 3, "3-plane color must emit RGB");
        for i in 0..n {
            assert_eq!(
                (
                    d.image_plane[0][i],
                    d.image_plane[1][i],
                    d.image_plane[2][i]
                ),
                (rp[i] as i32, gp[i] as i32, bp[i] as i32),
                "pixel {i}"
            );
        }
        // 1-plane in Color mode → grayscale (1 component, no synthesized chroma).
        let gplanes = [rp.clone()];
        let ginput = ImageInput {
            width: w,
            height: h,
            planes: &gplanes,
            premultiplied_alpha: false,
        };
        let gjxr = encode(&ginput, ColorMode::Color, QpSet::LOSSLESS).expect("gray fallback");
        assert_eq!(
            decode_to_planes(&gjxr).num_components,
            1,
            "1-plane color-mode ⇒ grayscale"
        );
    }

    #[test]
    fn color_mode_auto_gray_detects_equal_channels() {
        // Three identical channels carry no chroma → must emit 8bppGray.
        let mut r = Lcg(0xaaaa_5555_cccc_3333);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let plane: Vec<u8> = (0..n).map(|_| (r.next() % 256) as u8).collect();
        let planes = [plane.clone(), plane.clone(), plane.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let d = decode_to_planes(&jxr);
        assert_eq!(
            d.num_components, 1,
            "equal RGB channels must auto-detect to grayscale"
        );
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], plane[i] as i32, "pixel {i}");
        }
    }

    fn noise_planes<const N: usize>(r: &mut Lcg, n: usize) -> [Vec<u8>; N] {
        std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect())
    }

    #[test]
    fn roundtrip_rgba_alpha_plane_lossless() {
        // 4-plane RGBA → YUV444 primary plane + YONLY alpha image plane,
        // per-MB interleaved; all four channels bit-exact at LOSSLESS,
        // including non-16-aligned dims (both planes pad + crop identically).
        let mut r = Lcg(0xfeed_f00d_1234_5678);
        for &(w, h) in &[(48u32, 32u32), (17, 31), (16, 16), (100, 50)] {
            let n = (w * h) as usize;
            let planes: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let input = ImageInput {
                width: w,
                height: h,
                planes: &planes,
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).expect("rgba encode");
            let d = decode_to_planes(&jxr);
            assert_eq!(d.num_components, 4, "{w}x{h}: 3 primary + alpha");
            assert!(
                d.has_alpha && !d.premultiplied_alpha,
                "{w}x{h}: alpha flags"
            );
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes[c][i] as i32,
                        "{w}x{h} ch{c} px{i}"
                    );
                }
            }
        }
    }

    #[test]
    fn rgba_alpha_own_qp_is_independent() {
        // The alpha plane carries its own QPs in its plane header: a lossy
        // alpha leaves the lossless RGB bit-exact, and conversely.
        let mut r = Lcg(0xa1fa_0000_dead_beef);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 4] = noise_planes(&mut r, n);
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let lossy = QpSet {
            dc: 32,
            lp: 64,
            hp: 128,
        };

        let jxr = encode_with_alpha_qp(&input, ColorMode::Color, QpSet::LOSSLESS, lossy).unwrap();
        let d = decode_to_planes(&jxr);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(
                    d.image_plane[c][i], planes[c][i] as i32,
                    "RGB must stay exact ch{c}"
                );
            }
        }
        assert!(
            (0..n).any(|i| d.image_plane[3][i] != planes[3][i] as i32),
            "noise alpha at dc32/lp64/hp128 must show quantization error"
        );

        let jxr2 = encode_with_alpha_qp(&input, ColorMode::Color, lossy, QpSet::LOSSLESS).unwrap();
        let d2 = decode_to_planes(&jxr2);
        for i in 0..n {
            assert_eq!(
                d2.image_plane[3][i], planes[3][i] as i32,
                "alpha must stay exact px{i}"
            );
        }
        assert!(
            (0..3).any(|c| (0..n).any(|i| d2.image_plane[c][i] != planes[c][i] as i32)),
            "noise RGB at dc32/lp64/hp128 must show quantization error"
        );
    }

    #[test]
    fn rgba_premultiplied_flag_passthrough() {
        // premultiplied_alpha → `32bppPBGRA` GUID + the codestream property
        // bit (T.832 8.3.17), exposed by the decoder and the PixelBuffer.
        use crate::decode::pixels::AlphaMode;
        let mut r = Lcg(0x9e37_79b9_7f4a_7c15);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 4] = noise_planes(&mut r, n);
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: true,
        };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let c = crate::decode::container::parse(&jxr).expect("container");
        assert_eq!(
            c.pixel_format_uuid, "24c3dd6f-034e-fe4b-b185-3d77768dc910",
            "32bppPBGRA"
        );
        let d = crate::decode::decode_image(&c).expect("decode");
        assert!(d.has_alpha && d.premultiplied_alpha);
        let pb = d.to_pixel_buffer().expect("pixel buffer");
        assert_eq!(pb.alpha, AlphaMode::Premultiplied);
        assert_eq!(pb.channels, 4);

        let input2 = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let jxr2 = encode(&input2, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let c2 = crate::decode::container::parse(&jxr2).expect("container");
        assert_eq!(
            c2.pixel_format_uuid, "24c3dd6f-034e-fe4b-b185-3d77768dc90f",
            "32bppBGRA"
        );
        let d2 = crate::decode::decode_image(&c2).expect("decode");
        assert!(d2.has_alpha && !d2.premultiplied_alpha);
        assert_eq!(d2.to_pixel_buffer().unwrap().alpha, AlphaMode::Straight);
    }

    #[test]
    fn rgba_equal_channels_stays_color() {
        // R==G==B with an alpha plane must NOT auto-gray (gray+alpha has no
        // container pixel format): stays a 4-component 32bppBGRA file.
        let mut r = Lcg(0x5555_aaaa_5555_aaaa);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let g: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let a: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [g.clone(), g.clone(), g.clone(), a.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let c = crate::decode::container::parse(&jxr).expect("container");
        assert_eq!(c.pixel_format_uuid, "24c3dd6f-034e-fe4b-b185-3d77768dc90f");
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 4, "no auto-gray with alpha");
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], g[i] as i32);
            assert_eq!(d.image_plane[3][i], a[i] as i32);
        }
    }

    #[test]
    fn rgba_lossy_fixpoint_with_alpha() {
        // encode∘decode∘encode is byte-identical with lossy QPs on both planes
        // (alpha QP ≠ primary QP) — the quantizer-inversion discipline extended
        // to the alpha plane. Mid-range pixels + aligned dims, as in the
        // gray/color fixpoint tests.
        let mut r = Lcg(0x0ddc_0ffe_e123_4567);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let mk = |r: &mut Lcg| -> Vec<u8> {
            (0..n).map(|_| 96 + ((r.next() >> 32) as u8 % 64)).collect()
        };
        let planes = [mk(&mut r), mk(&mut r), mk(&mut r), mk(&mut r)];
        let (qp, aqp) = (
            QpSet {
                dc: 4,
                lp: 8,
                hp: 16,
            },
            QpSet {
                dc: 8,
                lp: 16,
                hp: 32,
            },
        );
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let jxr1 = encode_with_alpha_qp(&input, ColorMode::Color, qp, aqp).unwrap();
        let d = decode_to_planes(&jxr1);
        let ch = |c: usize| -> Vec<u8> {
            d.image_plane[c]
                .iter()
                .map(|&v| v.clamp(0, 255) as u8)
                .collect()
        };
        let p2 = [ch(0), ch(1), ch(2), ch(3)];
        let input2 = ImageInput {
            width: w,
            height: h,
            planes: &p2,
            premultiplied_alpha: false,
        };
        let jxr2 = encode_with_alpha_qp(&input2, ColorMode::Color, qp, aqp).unwrap();
        assert_eq!(jxr1, jxr2, "rgba lossy must be a fixpoint");
    }

    #[test]
    fn deinterleave_orders_normalize_to_rgb_planes() {
        // Two pixels in each memory order; planes come out R,G,B(,A) always.
        let bgra = [1u8, 2, 3, 4, 5, 6, 7, 8]; // px0 = B1 G2 R3 A4
        let p = deinterleave(ChannelOrder::Bgra, &bgra, 2, 1).unwrap();
        assert_eq!(p, vec![vec![3, 7], vec![2, 6], vec![1, 5], vec![4, 8]]);
        let rgba = [3u8, 2, 1, 4, 7, 6, 5, 8]; // same pixels, R,G,B,A order
        assert_eq!(deinterleave(ChannelOrder::Rgba, &rgba, 2, 1).unwrap(), p);
        let bgr = [1u8, 2, 3, 5, 6, 7];
        let rgb = [3u8, 2, 1, 7, 6, 5];
        let p3 = deinterleave(ChannelOrder::Bgr, &bgr, 2, 1).unwrap();
        assert_eq!(p3, vec![vec![3, 7], vec![2, 6], vec![1, 5]]);
        assert_eq!(deinterleave(ChannelOrder::Rgb, &rgb, 2, 1).unwrap(), p3);
        // Gray = single-plane passthrough.
        assert_eq!(
            deinterleave(ChannelOrder::Gray, &[9u8, 10], 2, 1).unwrap(),
            vec![vec![9, 10]]
        );
        // Wrong buffer length is rejected.
        assert!(deinterleave(ChannelOrder::Rgb, &[0u8; 5], 2, 1).is_err());
    }

    #[test]
    fn bgra_and_rgba_inputs_encode_byte_identical() {
        // The same pixels handed over in BGRA vs RGBA memory order must yield
        // the SAME file (orderings are input sugar; the codestream and the
        // canonical 32bppBGRA container don't depend on the source order).
        let mut r = Lcg(0xc0de_ba5e_c0de_ba5e);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let rgba: Vec<u8> = (0..n * 4).map(|_| (r.next() >> 32) as u8).collect();
        let bgra: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|px| [px[2], px[1], px[0], px[3]])
            .collect();
        let pa = deinterleave(ChannelOrder::Rgba, &rgba, w, h).unwrap();
        let pb = deinterleave(ChannelOrder::Bgra, &bgra, w, h).unwrap();
        assert_eq!(pa, pb, "normalized planes must be identical");
        let ia = ImageInput {
            width: w,
            height: h,
            planes: &pa,
            premultiplied_alpha: false,
        };
        let ib = ImageInput {
            width: w,
            height: h,
            planes: &pb,
            premultiplied_alpha: false,
        };
        let fa = encode(&ia, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let fb = encode(&ib, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        assert_eq!(fa, fb, "same pixels, different input order ⇒ same file");
        // And the file really carries those pixels (spot the first pixel).
        let d = decode_to_planes(&fa);
        assert_eq!(
            (
                d.image_plane[0][0],
                d.image_plane[1][0],
                d.image_plane[2][0],
                d.image_plane[3][0]
            ),
            (
                rgba[0] as i32,
                rgba[1] as i32,
                rgba[2] as i32,
                rgba[3] as i32
            )
        );
    }

    #[test]
    fn encode_with_options_dispatch() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x0715_0042_0715_0042);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [rp.clone(), gp.clone(), bp.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        // Default == classic encode, byte-for-byte.
        let a = encode_with_options(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        let b = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        assert_eq!(a, b, "Default options must reproduce classic encode bytes");
        // 4:2:0 via options: decodes to shape.
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                chroma: ChromaSampling::Yuv420,
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
        // YOnly: gray replication.
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                chroma: ChromaSampling::YOnly,
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], d.image_plane[1][i]);
        }
        // Scaled 444 lossless: bounded error (chroma half-step only).
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                scaled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        for i in 0..n {
            assert!((d.image_plane[0][i] - rp[i] as i32).abs() <= 2);
        }
        // YOnly + alpha rejected.
        let four = [rp.clone(), gp.clone(), bp.clone(), rp.clone()];
        let input4 = ImageInput {
            width: w,
            height: h,
            planes: &four,
            premultiplied_alpha: false,
        };
        assert!(
            encode_with_options(
                &input4,
                ColorMode::Color,
                EncodeOptions {
                    chroma: ChromaSampling::YOnly,
                    ..Default::default()
                },
            )
            .is_err()
        );
        // 420 + alpha works.
        let f = encode_with_options(
            &input4,
            ColorMode::Color,
            EncodeOptions {
                chroma: ChromaSampling::Yuv420,
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!(d.num_components, 4);
        assert!(d.has_alpha);
    }

    /// 4c: band truncation + trim_flexbits + long header via EncodeOptions.
    #[test]
    fn bands_trim_and_long_header() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x0042_4c00_0042_4c00);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [gray.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        let mse = |jxr: &[u8]| -> f64 {
            let d = dec(jxr);
            gray.iter()
                .zip(d.image_plane[0].iter())
                .map(|(&a, &b)| {
                    let e = a as f64 - b.clamp(0, 255) as f64;
                    e * e
                })
                .sum::<f64>()
                / n as f64
        };
        // Bands truncate monotonically: every level decodes, error grows,
        // size shrinks.
        let opts = |bands: BandsPresent, trim: u8| EncodeOptions {
            bands,
            trim_flexbits: trim,
            ..Default::default()
        };
        let all =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::All, 0)).unwrap();
        let noflex = encode_with_options(
            &input,
            ColorMode::Grayscale,
            opts(BandsPresent::NoFlexbits, 0),
        )
        .unwrap();
        let nohp = encode_with_options(
            &input,
            ColorMode::Grayscale,
            opts(BandsPresent::NoHighpass, 0),
        )
        .unwrap();
        let dconly =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::DcOnly, 0))
                .unwrap();
        assert_eq!(mse(&all), 0.0, "All bands lossless must be exact");
        let (m_nf, m_nh, m_dc) = (mse(&noflex), mse(&nohp), mse(&dconly));
        assert!(
            m_nf > 0.0 && m_nh > m_nf && m_dc > m_nh,
            "{m_nf} {m_nh} {m_dc}"
        );
        assert!(noflex.len() < all.len() && nohp.len() < noflex.len() && dconly.len() < nohp.len());
        // Trim: error grows with trim at All bands; trim=15 ≈ NoFlexbits-ish.
        let t4 =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::All, 4)).unwrap();
        let t15 =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::All, 15)).unwrap();
        let (m_t4, m_t15) = (mse(&t4), mse(&t15));
        assert!(m_t4 > 0.0 && m_t15 >= m_t4, "{m_t4} {m_t15}");
        assert!(t4.len() < all.len() && t15.len() < t4.len());
        // Same for the color path (420 + trim decodes fine).
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let cplanes = [rp, gp, bp];
        let cinput = ImageInput {
            width: w,
            height: h,
            planes: &cplanes,
            premultiplied_alpha: false,
        };
        for (chroma, bands, trim) in [
            (ChromaSampling::Yuv444, BandsPresent::NoFlexbits, 0u8),
            (ChromaSampling::Yuv444, BandsPresent::All, 6),
            (ChromaSampling::Yuv420, BandsPresent::All, 6),
            (ChromaSampling::Yuv420, BandsPresent::NoHighpass, 0),
        ] {
            let f = encode_with_options(
                &cinput,
                ColorMode::Color,
                EncodeOptions {
                    chroma,
                    bands,
                    trim_flexbits: trim,
                    ..Default::default()
                },
            )
            .unwrap();
            let d = dec(&f);
            assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
        }
        // Long header: dims beyond 2^16 encode + decode exactly.
        let (lw, lh) = (70_000u32, 16u32);
        let big: Vec<u8> = (0..(lw as usize * 16)).map(|i| (i % 251) as u8).collect();
        let bplanes = [big.clone()];
        let binput = ImageInput {
            width: lw,
            height: lh,
            planes: &bplanes,
            premultiplied_alpha: false,
        };
        let f = encode(&binput, ColorMode::Grayscale, QpSet::LOSSLESS).unwrap();
        let d = dec(&f);
        assert_eq!((d.width, d.height), (lw, lh));
        for (i, &v) in d.image_plane[0].iter().enumerate() {
            assert_eq!(v, big[i] as i32, "long-header px{i}");
        }
    }

    /// 4c: explicit window margins (`windowing_flag = 1`). The image sits at
    /// `(top, left)` inside the coded grid; decoders crop the margins away, so
    /// every lossless-exact path must stay exact — including odd margins over
    /// subsampled chroma (gray content) and the alpha plane (which shares the
    /// placement).
    #[test]
    fn explicit_window_margins_roundtrip() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x717d_0042_717d_0042);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        let windows = [(1u8, 1u8), (5, 9), (63, 63), (0, 7), (16, 0)];
        for &(w, h) in &[(30u32, 20u32), (48, 32), (16, 16)] {
            let n = (w * h) as usize;
            let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
            let gplanes = [gray.clone()];
            let ginput = ImageInput {
                width: w,
                height: h,
                planes: &gplanes,
                premultiplied_alpha: false,
            };
            let cplanes: [Vec<u8>; 3] = noise_planes(&mut r, n);
            let cinput = ImageInput {
                width: w,
                height: h,
                planes: &cplanes,
                premultiplied_alpha: false,
            };
            let aplanes: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let ainput = ImageInput {
                width: w,
                height: h,
                planes: &aplanes,
                premultiplied_alpha: false,
            };
            for &(top, left) in &windows {
                let opts = EncodeOptions {
                    window_top: top,
                    window_left: left,
                    ..Default::default()
                };
                // Grayscale: exact.
                let f = encode_with_options(&ginput, ColorMode::Grayscale, opts.clone()).unwrap();
                let d = dec(&f);
                assert_eq!((d.width, d.height), (w, h), "({top},{left}) {w}x{h}");
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[0][i], gray[i] as i32,
                        "gray ({top},{left}) px{i}"
                    );
                }
                // Color 4:4:4: exact.
                let f = encode_with_options(&cinput, ColorMode::Color, opts.clone()).unwrap();
                let d = dec(&f);
                for c in 0..3 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], cplanes[c][i] as i32,
                            "rgb ({top},{left}) ch{c} px{i}"
                        );
                    }
                }
                // RGBA: exact on all four channels.
                let f = encode_with_options(&ainput, ColorMode::Color, opts.clone()).unwrap();
                let d = dec(&f);
                assert_eq!(d.num_components, 4);
                for c in 0..4 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], aplanes[c][i] as i32,
                            "rgba ({top},{left}) ch{c} px{i}"
                        );
                    }
                }
                // 4:2:0 with gray content (zero chroma): exact even at odd margins.
                let gcolor = [gray.clone(), gray.clone(), gray.clone()];
                // (auto-gray would collapse this; go through the color driver.)
                let f = color::encode_color_options(
                    &gcolor[0],
                    &gcolor[1],
                    &gcolor[2],
                    w,
                    h,
                    QpSet::LOSSLESS,
                    crate::decode::consts::INT_YUV420,
                    crate::decode::consts::ALL_BANDS,
                    false,
                    0,
                    (top as u32, left as u32),
                    (&[], &[]),
                    0,
                    false,
                    None,
                );
                let d = dec(&f);
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[0][i], gray[i] as i32,
                        "420 ({top},{left}) px{i}"
                    );
                }
            }
        }
        // Margins are 6-bit fields: 64 is rejected.
        let p = vec![0u8; 256];
        let planes = [p.clone()];
        let input = ImageInput {
            width: 16,
            height: 16,
            planes: &planes,
            premultiplied_alpha: false,
        };
        assert!(
            encode_with_options(
                &input,
                ColorMode::Grayscale,
                EncodeOptions {
                    window_top: 64,
                    ..Default::default()
                },
            )
            .is_err()
        );
    }

    /// 4c: tiling. Multi-tile files (index table, per-tile packets, per-tile
    /// entropy resets, tile-relative prediction edges) round-trip exactly at
    /// lossless across grids, content kinds, and tiles×window combos; and
    /// because tiling only re-segments the entropy stream — coefficients are
    /// untouched — a tiled lossy decode must equal the untiled one
    /// pixel-for-pixel.
    #[test]
    fn tiled_roundtrip_and_equivalence() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x7113_d042_7113_d042);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        // 600px = 38 MB columns: 2 columns ⇒ 19-MB tiles, so the within-tile
        // 16-MB adapt cadence fires mid-tile too.
        for &(w, h, tc, tr) in &[
            (600u32, 32u32, 2u16, 2u16),
            (96, 96, 3, 2),
            (64, 64, 4, 1),
            (64, 64, 1, 4),
            (48, 32, 3, 2), // uneven split: 3 MBs / 3 cols, 2 MBs / 2 rows
            (17, 31, 2, 2), // non-aligned dims
        ] {
            let n = (w * h) as usize;
            let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
            let gplanes = [gray.clone()];
            let ginput = ImageInput {
                width: w,
                height: h,
                planes: &gplanes,
                premultiplied_alpha: false,
            };
            let opts = EncodeOptions {
                tile_cols: tc,
                tile_rows: tr,
                ..Default::default()
            };
            let f = encode_with_options(&ginput, ColorMode::Grayscale, opts.clone()).unwrap();
            let d = dec(&f);
            assert_eq!((d.width, d.height), (w, h));
            for i in 0..n {
                assert_eq!(
                    d.image_plane[0][i], gray[i] as i32,
                    "gray {tc}x{tr} {w}x{h} px{i}"
                );
            }
            // Color + RGBA, exact at lossless.
            let cplanes: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let cinput = ImageInput {
                width: w,
                height: h,
                planes: &cplanes[..3],
                premultiplied_alpha: false,
            };
            let f = encode_with_options(&cinput, ColorMode::Color, opts.clone()).unwrap();
            let d = dec(&f);
            for c in 0..3 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], cplanes[c][i] as i32,
                        "rgb {tc}x{tr} {w}x{h} ch{c} px{i}"
                    );
                }
            }
            let ainput = ImageInput {
                width: w,
                height: h,
                planes: &cplanes,
                premultiplied_alpha: false,
            };
            let f = encode_with_options(&ainput, ColorMode::Color, opts.clone()).unwrap();
            let d = dec(&f);
            assert_eq!(d.num_components, 4);
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], cplanes[c][i] as i32,
                        "rgba {tc}x{tr} {w}x{h} ch{c} px{i}"
                    );
                }
            }
        }
        // Tiled lossy == untiled lossy, pixel-for-pixel (and likewise at 420).
        let (w, h) = (96u32, 64u32);
        let n = (w * h) as usize;
        let cplanes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let cinput = ImageInput {
            width: w,
            height: h,
            planes: &cplanes,
            premultiplied_alpha: false,
        };
        for chroma in [ChromaSampling::Yuv444, ChromaSampling::Yuv420] {
            let qp = QpSet {
                dc: 16,
                lp: 32,
                hp: 64,
            };
            let base = EncodeOptions {
                qp,
                chroma,
                scaled: true,
                ..Default::default()
            };
            let untiled = encode_with_options(&cinput, ColorMode::Color, base.clone()).unwrap();
            let tiled = encode_with_options(
                &cinput,
                ColorMode::Color,
                EncodeOptions {
                    tile_cols: 3,
                    tile_rows: 2,
                    ..base.clone()
                },
            )
            .unwrap();
            let (du, dt) = (dec(&untiled), dec(&tiled));
            for c in 0..3 {
                assert_eq!(
                    du.image_plane[c], dt.image_plane[c],
                    "tiling must not change reconstruction ({chroma:?} ch{c})"
                );
            }
        }
        // Tiles × explicit window margins.
        let gplanes = [cplanes[0].clone()];
        let ginput = ImageInput {
            width: w,
            height: h,
            planes: &gplanes,
            premultiplied_alpha: false,
        };
        let f = encode_with_options(
            &ginput,
            ColorMode::Grayscale,
            EncodeOptions {
                tile_cols: 2,
                tile_rows: 2,
                window_top: 5,
                window_left: 9,
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!((d.width, d.height), (w, h));
        for i in 0..n {
            assert_eq!(
                d.image_plane[0][i], gplanes[0][i] as i32,
                "tiles×window px{i}"
            );
        }
        // Large noise tiles: packet offsets beyond 0xfaff exercise the 0xfb
        // 32-bit vlw_esc escape in the index table.
        let (bw, bh) = (768u32, 768u32);
        let bn = (bw * bh) as usize;
        let big: Vec<u8> = (0..bn).map(|_| (r.next() >> 32) as u8).collect();
        let bplanes = [big.clone()];
        let binput = ImageInput {
            width: bw,
            height: bh,
            planes: &bplanes,
            premultiplied_alpha: false,
        };
        let f = encode_with_options(
            &binput,
            ColorMode::Grayscale,
            EncodeOptions {
                tile_cols: 2,
                tile_rows: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            f.len() > 4 * 0xfb00,
            "noise file must be big enough to need the escape"
        );
        let d = dec(&f);
        for i in 0..bn {
            assert_eq!(d.image_plane[0][i], big[i] as i32, "vlw-escape px{i}");
        }
        // Validation: every tile needs ≥ 1 MB.
        let small = vec![0u8; 256];
        let splanes = [small];
        let sinput = ImageInput {
            width: 16,
            height: 16,
            planes: &splanes,
            premultiplied_alpha: false,
        };
        assert!(
            encode_with_options(
                &sinput,
                ColorMode::Grayscale,
                EncodeOptions {
                    tile_cols: 2,
                    ..Default::default()
                },
            )
            .is_err()
        );
    }

    /// 4c: overlap modes 1/2. The pre-filters are exact inverses of the
    /// decoder's post-filters over the same (disjoint) windows, so LOSSLESS
    /// stays bit-exact through overlap — across content kinds, subsampling,
    /// tiles (soft-tile continuations), window margins, and the alpha plane.
    /// Lossy overlap must still decode to the right shape, and overlapped
    /// lossy bytes must differ from non-overlapped (the filter is real).
    #[test]
    fn overlap_roundtrip_lossless() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x0a1a_4242_0a1a_4242);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        for &(w, h) in &[(48u32, 32u32), (17, 31), (64, 64)] {
            let n = (w * h) as usize;
            let planes4: [Vec<u8>; 4] = noise_planes(&mut r, n);
            for overlap in [Overlap::One, Overlap::Two] {
                // Grayscale exact.
                let gplanes = [planes4[0].clone()];
                let gi = ImageInput {
                    width: w,
                    height: h,
                    planes: &gplanes,
                    premultiplied_alpha: false,
                };
                let f = encode_with_options(
                    &gi,
                    ColorMode::Grayscale,
                    EncodeOptions {
                        overlap,
                        ..Default::default()
                    },
                )
                .unwrap();
                let d = dec(&f);
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[0][i], gplanes[0][i] as i32,
                        "gray {overlap:?} {w}x{h} px{i}"
                    );
                }
                // RGB 4:4:4 exact.
                let ci = ImageInput {
                    width: w,
                    height: h,
                    planes: &planes4[..3],
                    premultiplied_alpha: false,
                };
                let f = encode_with_options(
                    &ci,
                    ColorMode::Color,
                    EncodeOptions {
                        overlap,
                        ..Default::default()
                    },
                )
                .unwrap();
                let d = dec(&f);
                for c in 0..3 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], planes4[c][i] as i32,
                            "rgb {overlap:?} {w}x{h} ch{c} px{i}"
                        );
                    }
                }
                // RGBA exact (alpha plane filtered identically).
                let ai = ImageInput {
                    width: w,
                    height: h,
                    planes: &planes4,
                    premultiplied_alpha: false,
                };
                let f = encode_with_options(
                    &ai,
                    ColorMode::Color,
                    EncodeOptions {
                        overlap,
                        ..Default::default()
                    },
                )
                .unwrap();
                let d = dec(&f);
                assert_eq!(d.num_components, 4);
                for c in 0..4 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], planes4[c][i] as i32,
                            "rgba {overlap:?} {w}x{h} ch{c} px{i}"
                        );
                    }
                }
                // Gray content through 420/422 (zero chroma) exact — incl.
                // the chroma DC-domain pre-filter at overlap Two.
                for fmt in [
                    crate::decode::consts::INT_YUV420,
                    crate::decode::consts::INT_YUV422,
                ] {
                    let g = &planes4[0];
                    let f = color::encode_color_options(
                        g,
                        g,
                        g,
                        w,
                        h,
                        QpSet::LOSSLESS,
                        fmt,
                        crate::decode::consts::ALL_BANDS,
                        false,
                        0,
                        (0, 0),
                        (&[], &[]),
                        overlap.code(),
                        false,
                        None,
                    );
                    let d = dec(&f);
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[0][i], g[i] as i32,
                            "{fmt} {overlap:?} {w}x{h} px{i}"
                        );
                    }
                }
            }
        }
        // Overlap × tiles (soft-tile continuations) × window margins, exact.
        let (w, h) = (96u32, 64u32);
        let n = (w * h) as usize;
        let planes4: [Vec<u8>; 4] = noise_planes(&mut r, n);
        for overlap in [Overlap::One, Overlap::Two] {
            let ai = ImageInput {
                width: w,
                height: h,
                planes: &planes4,
                premultiplied_alpha: false,
            };
            let f = encode_with_options(
                &ai,
                ColorMode::Color,
                EncodeOptions {
                    overlap,
                    tile_cols: 3,
                    tile_rows: 2,
                    window_top: 5,
                    window_left: 9,
                    ..Default::default()
                },
            )
            .unwrap();
            let d = dec(&f);
            assert_eq!((d.width, d.height), (w, h));
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes4[c][i] as i32,
                        "tiles×window {overlap:?} ch{c} px{i}"
                    );
                }
            }
        }
        // Lossy with overlap: decodes, and differs from no-overlap bytes.
        let ci = ImageInput {
            width: w,
            height: h,
            planes: &planes4[..3],
            premultiplied_alpha: false,
        };
        let qp = QpSet {
            dc: 16,
            lp: 32,
            hp: 64,
        };
        let base = EncodeOptions {
            qp,
            scaled: true,
            ..Default::default()
        };
        let f0 = encode_with_options(&ci, ColorMode::Color, base.clone()).unwrap();
        for overlap in [Overlap::One, Overlap::Two] {
            let f = encode_with_options(
                &ci,
                ColorMode::Color,
                EncodeOptions {
                    overlap,
                    ..base.clone()
                },
            )
            .unwrap();
            assert_ne!(f, f0, "{overlap:?} must change the coded bytes");
            let d = dec(&f);
            assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
        }
    }

    /// Scaled-mode LOSSY quantization: the QP→scaling-factor map is
    /// mode-dependent (decoder `quant_map` scaled branch; chroma DC/LP one
    /// order below luma), so a scaled lossy encode must land in the same
    /// quality regime as the unscaled one at the same QP — not garbage. The
    /// other scaled tests gate lossless exactness or decoder agreement, and
    /// neither of those sees lossy fidelity.
    #[test]
    fn scaled_lossy_quantizes_with_scaled_factors() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x5ca1_ed42_5ca1_ed42);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let psnr = |jxr: &[u8]| -> f64 {
            let c = parse(jxr).unwrap();
            let d = crate::decode::decode_image(&c).unwrap();
            let mut se = 0f64;
            for ch in 0..3 {
                for i in 0..n {
                    let e = (d.image_plane[ch][i].clamp(0, 255) - planes[ch][i] as i32) as f64;
                    se += e * e;
                }
            }
            10.0 * (255.0f64 * 255.0 * (3 * n) as f64 / se).log10()
        };
        let qp = QpSet {
            dc: 16,
            lp: 16,
            hp: 16,
        };
        let unscaled = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                qp,
                ..Default::default()
            },
        )
        .unwrap();
        let scaled = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                qp,
                scaled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (pu, ps) = (psnr(&unscaled), psnr(&scaled));
        assert!(
            ps > pu - 2.0,
            "scaled q16 must match unscaled quality: {ps:.2} vs {pu:.2} dB"
        );
        // And the same holds with overlap + 420 in the mix. (On independent
        // RGB noise, 4:2:0 PSNR vs source is dominated by the subsampling
        // loss itself — so the baseline is the UNSCALED 4:2:0 encode, not an
        // absolute figure.)
        let opts420 = EncodeOptions {
            qp,
            chroma: ChromaSampling::Yuv420,
            ..Default::default()
        };
        let u420 = encode_with_options(&input, ColorMode::Color, opts420.clone()).unwrap();
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                scaled: true,
                overlap: Overlap::Two,
                ..opts420.clone()
            },
        )
        .unwrap();
        let (pu420, ps420) = (psnr(&u420), psnr(&f));
        assert!(
            ps420 > pu420 - 2.0,
            "scaled+overlap 420 q16 must match unscaled 420: {ps420:.2} vs {pu420:.2} dB"
        );
    }

    /// 4d: frequency mode. Same per-MB band sections routed into per-band
    /// tile packets (index table addressing) — so LOSSLESS stays exact across
    /// the whole envelope, and a frequency-mode lossy decode must be
    /// PIXEL-IDENTICAL to the spatial one (same coefficients, re-segmented
    /// stream).
    #[test]
    fn frequency_mode_roundtrip_and_equivalence() {
        use crate::decode::container::parse;
        let mut r = Lcg(0xf4e0_0042_f4e0_0042);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        for &(w, h) in &[(48u32, 32u32), (17, 31), (96, 64)] {
            let n = (w * h) as usize;
            let planes4: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let freq = EncodeOptions {
                frequency: true,
                ..Default::default()
            };
            // Gray / RGB / RGBA exact at lossless.
            let gplanes = [planes4[0].clone()];
            let gi = ImageInput {
                width: w,
                height: h,
                planes: &gplanes,
                premultiplied_alpha: false,
            };
            let f = encode_with_options(&gi, ColorMode::Grayscale, freq.clone()).unwrap();
            let d = dec(&f);
            for i in 0..n {
                assert_eq!(
                    d.image_plane[0][i], gplanes[0][i] as i32,
                    "gray freq {w}x{h} px{i}"
                );
            }
            let ci = ImageInput {
                width: w,
                height: h,
                planes: &planes4[..3],
                premultiplied_alpha: false,
            };
            let f = encode_with_options(&ci, ColorMode::Color, freq.clone()).unwrap();
            let d = dec(&f);
            for c in 0..3 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes4[c][i] as i32,
                        "rgb freq {w}x{h} ch{c} px{i}"
                    );
                }
            }
            let ai = ImageInput {
                width: w,
                height: h,
                planes: &planes4,
                premultiplied_alpha: false,
            };
            let f = encode_with_options(&ai, ColorMode::Color, freq.clone()).unwrap();
            let d = dec(&f);
            assert_eq!(d.num_components, 4);
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes4[c][i] as i32,
                        "rgba freq {w}x{h} ch{c} px{i}"
                    );
                }
            }
            // Frequency × tiles × window × overlap, exact.
            if w >= 48 {
                let f = encode_with_options(
                    &ci,
                    ColorMode::Color,
                    EncodeOptions {
                        frequency: true,
                        tile_cols: 2,
                        tile_rows: 2,
                        window_top: 3,
                        window_left: 5,
                        overlap: Overlap::Two,
                        ..Default::default()
                    },
                )
                .unwrap();
                let d = dec(&f);
                assert_eq!((d.width, d.height), (w, h));
                for c in 0..3 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], planes4[c][i] as i32,
                            "freq×tiles×window×ovl {w}x{h} ch{c} px{i}"
                        );
                    }
                }
            }
            // Frequency × band truncation: fewer packets per tile, decodes
            // to the same pixels as the spatial encode at the same bands.
            for bands in [
                BandsPresent::NoFlexbits,
                BandsPresent::NoHighpass,
                BandsPresent::DcOnly,
            ] {
                let fs = encode_with_options(
                    &gi,
                    ColorMode::Grayscale,
                    EncodeOptions {
                        bands,
                        ..Default::default()
                    },
                )
                .unwrap();
                let ff = encode_with_options(
                    &gi,
                    ColorMode::Grayscale,
                    EncodeOptions {
                        bands,
                        frequency: true,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(
                    dec(&fs).image_plane[0],
                    dec(&ff).image_plane[0],
                    "{bands:?} spatial vs frequency must reconstruct identically"
                );
            }
        }
        // Lossy: frequency == spatial pixel-for-pixel (444 + 420, with trim).
        let (w, h) = (96u32, 64u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let ci = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        for chroma in [ChromaSampling::Yuv444, ChromaSampling::Yuv420] {
            let base = EncodeOptions {
                qp: QpSet {
                    dc: 16,
                    lp: 32,
                    hp: 64,
                },
                scaled: true,
                chroma,
                trim_flexbits: 4,
                ..Default::default()
            };
            let fs = encode_with_options(&ci, ColorMode::Color, base.clone()).unwrap();
            let ff = encode_with_options(
                &ci,
                ColorMode::Color,
                EncodeOptions {
                    frequency: true,
                    ..base.clone()
                },
            )
            .unwrap();
            let (ds, df) = (dec(&fs), dec(&ff));
            for c in 0..3 {
                assert_eq!(
                    ds.image_plane[c], df.image_plane[c],
                    "lossy frequency must equal spatial ({chroma:?} ch{c})"
                );
            }
        }
    }

    /// 4e: QP generality. (a) `chroma_qp` (COMP_SEPARATE at the uniform
    /// level): equal bytes stay byte-identical to the plain path; zero-chroma
    /// content is exact under any chroma QP; (b) COMP_INDEPENDENT (U ≠ V);
    /// (c) per-tile QP sets — an all-lossless-tiles plan is exact, and in a
    /// mixed plan the lossless tiles' pixels stay EXACT while lossy tiles
    /// deviate; (d) per-MB DQUANT (LP+HP set lists + index maps): lossless-
    /// set MBs stay exact, lossy-set MBs deviate, and the same plan in
    /// frequency mode reconstructs identically (index bits route per band).
    /// Any emission/prediction mismatch would desync the entropy decode
    /// entirely, so exactness is a sharp gate.
    #[test]
    fn qp_generality_separate_pertile_dquant() {
        use crate::decode::container::parse;
        use quant::{BandQp, QpPlan, TileQps};
        let mut r = Lcg(0x9e9e_0042_9e9e_0042);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        let (w, h) = (64u32, 64u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();

        // (a) chroma_qp == qp ⇒ byte-identical (derived COMP_UNIFORM).
        let qp = QpSet {
            dc: 8,
            lp: 16,
            hp: 32,
        };
        let a = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                qp,
                ..Default::default()
            },
        )
        .unwrap();
        let b = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                qp,
                chroma_qp: Some(qp),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(a, b, "equal chroma_qp must emit COMP_UNIFORM bytes");
        // Zero-chroma content + harsh chroma QP ⇒ still exact. Routed through
        // the PUBLIC qp_plan surface: the explicit plan also suppresses the
        // auto-gray collapse, so this really emits the color codestream.
        let gcolor = [gray.clone(), gray.clone(), gray.clone()];
        let ginput = ImageInput {
            width: w,
            height: h,
            planes: &gcolor,
            premultiplied_alpha: false,
        };
        let harsh_chroma = QpPlan::uniform(
            QpSet::LOSSLESS,
            Some(QpSet {
                dc: 64,
                lp: 64,
                hp: 64,
            }),
        );
        let f = encode_with_options(
            &ginput,
            ColorMode::Color,
            EncodeOptions {
                qp_plan: Some(harsh_chroma.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!(
            d.num_components, 3,
            "qp_plan must suppress the auto-gray collapse"
        );
        for i in 0..n {
            assert_eq!(
                d.image_plane[0][i], gray[i] as i32,
                "zero-chroma separate px{i}"
            );
        }
        // The same single-set plan and the chroma_qp fold are one emission.
        let via_chroma_qp = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                chroma_qp: Some(QpSet {
                    dc: 64,
                    lp: 64,
                    hp: 64,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        let via_plan = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                qp_plan: Some(harsh_chroma),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            via_chroma_qp, via_plan,
            "qp_plan(uniform) must equal the chroma_qp bytes"
        );
        // Colored content + harsh chroma: decodes, smaller than all-lossless.
        let lossless = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                chroma_qp: Some(QpSet {
                    dc: 64,
                    lp: 64,
                    hp: 64,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            f.len() < lossless.len(),
            "harsh chroma must shrink the file"
        );
        let d = dec(&f);
        assert_eq!((d.width, d.height, d.num_components), (w, h, 3));

        // (b) COMP_INDEPENDENT (Y, U, V all different) — lossless Y, lossy U/V.
        let plan_indep = QpPlan {
            tiles: vec![TileQps {
                dc: BandQp([0, 16, 32]),
                lp: vec![BandQp([0, 16, 32])],
                hp: vec![BandQp([0, 16, 32])],
            }],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        };
        let f = encode_with_options(
            &ginput,
            ColorMode::Color,
            EncodeOptions {
                qp_plan: Some(plan_indep),
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        for i in 0..n {
            assert_eq!(
                d.image_plane[0][i], gray[i] as i32,
                "independent zero-chroma px{i}"
            );
        }

        // (c) per-tile QP sets over a 2x2 grid: all-lossless = exact; mixed =
        // the lossless tiles' pixel regions stay exact.
        let mbw = (w as usize) / 16;
        let tile_l = TileQps {
            dc: BandQp::uniform(0),
            lp: vec![BandQp::uniform(0)],
            hp: vec![BandQp::uniform(0)],
        };
        let tile_q = TileQps {
            dc: BandQp::uniform(32),
            lp: vec![BandQp::uniform(64)],
            hp: vec![BandQp::uniform(96)],
        };
        let all_lossless = QpPlan {
            tiles: vec![
                tile_l.clone(),
                tile_l.clone(),
                tile_l.clone(),
                tile_l.clone(),
            ],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        };
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                tile_cols: 2,
                tile_rows: 2,
                qp_plan: Some(all_lossless),
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(
                    d.image_plane[c][i], planes[c][i] as i32,
                    "per-tile lossless ch{c} px{i}"
                );
            }
        }
        // Mixed: tile 0 (top-left 32x32) lossless, others lossy.
        let mixed = QpPlan {
            tiles: vec![
                tile_l.clone(),
                tile_q.clone(),
                tile_q.clone(),
                tile_q.clone(),
            ],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        };
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions {
                tile_cols: 2,
                tile_rows: 2,
                qp_plan: Some(mixed),
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        let mut lossy_diffs = 0usize;
        for c in 0..3 {
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = y * w as usize + x;
                    if x < 32 && y < 32 {
                        assert_eq!(
                            d.image_plane[c][i], planes[c][i] as i32,
                            "lossless tile must stay exact ch{c} ({x},{y})"
                        );
                    } else if d.image_plane[c][i] != planes[c][i] as i32 {
                        lossy_diffs += 1;
                    }
                }
            }
        }
        assert!(lossy_diffs > 0, "lossy tiles must actually quantize");

        // (d) per-MB DQUANT: LP sets [lossless, harsh] + HP sets [lossless,
        // harsh] on a checkerboard; set-0 MBs exact, set-1 MBs deviate;
        // frequency mode reconstructs identically.
        let mbh = (h as usize) / 16;
        let map: Vec<u8> = (0..mbw * mbh)
            .map(|i| (((i % mbw) + (i / mbw)) % 2) as u8)
            .collect();
        let dq = QpPlan {
            tiles: vec![TileQps {
                dc: BandQp::uniform(0),
                lp: vec![BandQp::uniform(0), BandQp::uniform(64)],
                hp: vec![BandQp::uniform(0), BandQp::uniform(96)],
            }],
            lp_index: map.clone(),
            hp_index: map.clone(),
        };
        let enc = |frequency: bool| {
            encode_with_options(
                &input,
                ColorMode::Color,
                EncodeOptions {
                    frequency,
                    qp_plan: Some(dq.clone()),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let fs = enc(false);
        let ds = dec(&fs);
        let mut lossy_mb_diffs = 0usize;
        for c in 0..3 {
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = y * w as usize + x;
                    let mb = (y / 16) * mbw + (x / 16);
                    if map[mb] == 0 {
                        assert_eq!(
                            ds.image_plane[c][i], planes[c][i] as i32,
                            "set-0 (lossless) MB must stay exact ch{c} ({x},{y})"
                        );
                    } else if ds.image_plane[c][i] != planes[c][i] as i32 {
                        lossy_mb_diffs += 1;
                    }
                }
            }
        }
        assert!(lossy_mb_diffs > 0, "set-1 MBs must actually quantize");
        let ff = enc(true);
        let df = dec(&ff);
        for c in 0..3 {
            assert_eq!(
                ds.image_plane[c], df.image_plane[c],
                "DQUANT frequency must equal spatial (ch{c})"
            );
        }
    }

    /// The public `qp_plan` contract: shape validation (every malformed plan
    /// is `Invalid` BEFORE any coding state is built), the path rejections
    /// (`Unsupported` on gray/YOnly/alpha/multi/bi-level), the
    /// `chroma_qp` mutual exclusion, auto-gray suppression at both entry
    /// points, and plan support on the typed (deep + packed) color paths.
    #[test]
    fn qp_plan_public_surface() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x7a7a_0011_7a7a_0011);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let input = ImageInput {
            width: w,
            height: h,
            planes: &planes,
            premultiplied_alpha: false,
        };
        let lossless_tile = || TileQps {
            dc: BandQp::uniform(0),
            lp: vec![BandQp::uniform(0)],
            hp: vec![BandQp::uniform(0)],
        };
        let uniform_plan = || QpPlan {
            tiles: vec![lossless_tile()],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        };
        let opts_with = |p: QpPlan| EncodeOptions {
            qp_plan: Some(p),
            ..Default::default()
        };
        let invalid = |r: Result<Vec<u8>, EncodeError>, what: &str| {
            assert!(
                matches!(r, Err(EncodeError::Invalid(_))),
                "{what} must be EncodeError::Invalid, got {r:?}"
            );
        };
        let unsupported = |r: Result<Vec<u8>, EncodeError>, what: &str| {
            assert!(
                matches!(r, Err(EncodeError::Unsupported(_))),
                "{what} must be EncodeError::Unsupported, got {r:?}"
            );
        };

        // -- Shape validation (all Invalid) --
        let mut p = uniform_plan();
        p.tiles = vec![lossless_tile(), lossless_tile(), lossless_tile()];
        invalid(
            encode_with_options(
                &input,
                ColorMode::Color,
                EncodeOptions {
                    tile_cols: 2,
                    tile_rows: 2,
                    ..opts_with(p)
                },
            ),
            "3 tile entries on a 2x2 grid",
        );
        let mut p = uniform_plan();
        p.tiles[0].hp = vec![BandQp::uniform(0); 17];
        invalid(
            encode_with_options(&input, ColorMode::Color, opts_with(p)),
            "17 HP sets",
        );
        let mut p = QpPlan {
            tiles: vec![
                lossless_tile(),
                lossless_tile(),
                lossless_tile(),
                lossless_tile(),
            ],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        };
        p.tiles[2].lp = vec![BandQp::uniform(0), BandQp::uniform(8)];
        invalid(
            encode_with_options(
                &input,
                ColorMode::Color,
                EncodeOptions {
                    tile_cols: 2,
                    tile_rows: 2,
                    ..opts_with(p)
                },
            ),
            "ragged per-tile LP set counts",
        );
        let (mbw, mbh) = (w.div_ceil(16) as usize, h.div_ceil(16) as usize);
        let two_sets = || QpPlan {
            tiles: vec![TileQps {
                dc: BandQp::uniform(0),
                lp: vec![BandQp::uniform(0), BandQp::uniform(64)],
                hp: vec![BandQp::uniform(0), BandQp::uniform(64)],
            }],
            lp_index: Vec::new(),
            hp_index: Vec::new(),
        };
        let mut p = two_sets();
        p.lp_index = vec![0; mbw * mbh - 1];
        invalid(
            encode_with_options(&input, ColorMode::Color, opts_with(p)),
            "short lp_index map",
        );
        let mut p = two_sets();
        p.hp_index = vec![2; mbw * mbh];
        invalid(
            encode_with_options(&input, ColorMode::Color, opts_with(p)),
            "hp_index entry out of range",
        );
        let mut p = uniform_plan();
        p.lp_index = vec![0; mbw * mbh];
        invalid(
            encode_with_options(&input, ColorMode::Color, opts_with(p)),
            "index map with a single set",
        );
        invalid(
            encode_with_options(
                &input,
                ColorMode::Color,
                EncodeOptions {
                    chroma_qp: Some(QpSet {
                        dc: 8,
                        lp: 8,
                        hp: 8,
                    }),
                    ..opts_with(uniform_plan())
                },
            ),
            "qp_plan + chroma_qp",
        );

        // -- Path rejections (all Unsupported) --
        let gray = vec![planes[0].clone()];
        let ginput = ImageInput {
            width: w,
            height: h,
            planes: &gray,
            premultiplied_alpha: false,
        };
        unsupported(
            encode_with_options(&ginput, ColorMode::Grayscale, opts_with(uniform_plan())),
            "qp_plan x grayscale",
        );
        unsupported(
            encode_with_options(
                &input,
                ColorMode::Color,
                EncodeOptions {
                    chroma: ChromaSampling::YOnly,
                    ..opts_with(uniform_plan())
                },
            ),
            "qp_plan x YOnly",
        );
        let four: Vec<Vec<u8>> = planes.iter().cloned().chain([planes[0].clone()]).collect();
        let ainput = ImageInput {
            width: w,
            height: h,
            planes: &four,
            premultiplied_alpha: false,
        };
        unsupported(
            encode_with_options(&ainput, ColorMode::Color, opts_with(uniform_plan())),
            "qp_plan x alpha plane",
        );
        let cmyk: Vec<Vec<u8>> = (0..4).map(|c| planes[c % 3].clone()).collect();
        let cinput = ImageInput {
            width: w,
            height: h,
            planes: &cmyk,
            premultiplied_alpha: false,
        };
        unsupported(
            encode_with_options(&cinput, ColorMode::Cmyk, opts_with(uniform_plan())),
            "qp_plan x CMYK",
        );
        let bw: Vec<Vec<u8>> = vec![planes[0].iter().map(|&v| v & 1).collect()];
        unsupported(
            encode_typed(
                &TypedInput {
                    width: w,
                    height: h,
                    samples: SamplePlanes::Bw(&bw),
                    premultiplied_alpha: false,
                },
                ColorMode::Grayscale,
                opts_with(uniform_plan()),
            ),
            "qp_plan x bi-level",
        );

        // -- Auto-gray suppression: R==G==B + qp_plan emits the color GUID --
        let gcolor = [planes[0].clone(), planes[0].clone(), planes[0].clone()];
        let gc_input = ImageInput {
            width: w,
            height: h,
            planes: &gcolor,
            premultiplied_alpha: false,
        };
        let without = encode_with_options(&gc_input, ColorMode::Color, Default::default()).unwrap();
        assert!(
            parse(&without).unwrap().pixel_format_uuid.ends_with("08"),
            "gray content without a plan still collapses to 8bppGray"
        );
        let with =
            encode_with_options(&gc_input, ColorMode::Color, opts_with(uniform_plan())).unwrap();
        assert!(
            parse(&with).unwrap().pixel_format_uuid.ends_with("0d"),
            "qp_plan must suppress auto-gray (24bppRGB GUID)"
        );

        // -- Typed paths: U16 RGB with per-MB DQUANT (lossless set-0 exact), --
        // -- packed 565 with an all-lossless per-tile plan (words exact).    --
        let deep: Vec<Vec<u16>> = (0..3)
            .map(|_| (0..n).map(|_| (r.next() >> 24) as u16).collect())
            .collect();
        let map: Vec<u8> = (0..mbw * mbh).map(|i| (i % 2) as u8).collect();
        let mut dq = two_sets();
        dq.lp_index = map.clone();
        dq.hp_index = map.clone();
        let f = encode_typed(
            &TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U16(&deep),
                premultiplied_alpha: false,
            },
            ColorMode::Color,
            opts_with(dq),
        )
        .unwrap();
        let c = parse(&f).unwrap();
        let d = crate::decode::decode_image(&c).unwrap();
        let mut deviations = 0usize;
        for ch in 0..3 {
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = y * w as usize + x;
                    if map[(y / 16) * mbw + (x / 16)] == 0 {
                        assert_eq!(
                            d.image_plane[ch][i], deep[ch][i] as i32,
                            "U16 DQUANT set-0 MB must stay exact ch{ch} ({x},{y})"
                        );
                    } else if d.image_plane[ch][i] != deep[ch][i] as i32 {
                        deviations += 1;
                    }
                }
            }
        }
        assert!(
            deviations > 0,
            "U16 DQUANT set-1 MBs must actually quantize"
        );
        let packed: Vec<Vec<u16>> = vec![(0..n).map(|_| (r.next() >> 40) as u16).collect()];
        let f = encode_typed(
            &TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::Packed565(&packed),
                premultiplied_alpha: false,
            },
            ColorMode::Color,
            EncodeOptions {
                tile_cols: 2,
                ..opts_with(QpPlan {
                    tiles: vec![lossless_tile(), lossless_tile()],
                    lp_index: Vec::new(),
                    hp_index: Vec::new(),
                })
            },
        )
        .unwrap();
        let c = parse(&f).unwrap();
        // Packed output formatting re-packs into ONE word plane (the in-crate
        // packed tests assert the same shape).
        let d = crate::decode::decode_image(&c).unwrap();
        assert_eq!(d.num_components, 1);
        for (i, px) in packed[0].iter().enumerate() {
            assert_eq!(
                d.image_plane[0][i], *px as i32,
                "packed 565 per-tile lossless plan word px{i}"
            );
        }
    }

    /// `chroma_qp` applies to the 4-plane (alpha) path's PRIMARY plane, and the
    /// alpha drivers must honour it.
    /// Zero-chroma content stays exact under a harsh chroma quantizer;
    /// colored content shrinks; equal bytes derive `COMP_UNIFORM`
    /// byte-stably.
    #[test]
    fn alpha_chroma_qp_applies() {
        use crate::decode::container::parse;
        let mut r = Lcg(0xa1fa_0077);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let alpha: Vec<u8> = (0..n).map(|_| (r.next() >> 24) as u8).collect();
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        let harsh = QpSet {
            dc: 64,
            lp: 64,
            hp: 64,
        };
        // Zero-chroma RGBA + harsh chroma_qp at q1: exact on all 4 channels.
        let gplanes = vec![gray.clone(), gray.clone(), gray.clone(), alpha.clone()];
        let ginput = ImageInput {
            width: w,
            height: h,
            planes: &gplanes,
            premultiplied_alpha: false,
        };
        let f = encode_with_options(
            &ginput,
            ColorMode::Color,
            EncodeOptions {
                chroma_qp: Some(harsh),
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!(d.num_components, 4);
        for (c, plane) in [&gray, &gray, &gray, &alpha].iter().enumerate() {
            for i in 0..n {
                assert_eq!(
                    d.image_plane[c][i], plane[i] as i32,
                    "alpha chroma_qp ch{c} px{i}"
                );
            }
        }
        // Colored RGBA: harsh chroma shrinks the file; still 4-channel.
        let cplanes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let four = vec![
            cplanes[0].clone(),
            cplanes[1].clone(),
            cplanes[2].clone(),
            alpha,
        ];
        let cinput = ImageInput {
            width: w,
            height: h,
            planes: &four,
            premultiplied_alpha: false,
        };
        let lossless = encode_with_options(&cinput, ColorMode::Color, Default::default()).unwrap();
        let f = encode_with_options(
            &cinput,
            ColorMode::Color,
            EncodeOptions {
                chroma_qp: Some(harsh),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            f.len() < lossless.len(),
            "harsh chroma must shrink the alpha file"
        );
        assert_eq!(dec(&f).num_components, 4);
        // chroma_qp == qp derives COMP_UNIFORM: byte-identical to None.
        let same = encode_with_options(
            &cinput,
            ColorMode::Color,
            EncodeOptions {
                chroma_qp: Some(QpSet::LOSSLESS),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            same, lossless,
            "equal chroma_qp must stay byte-stable on the alpha path"
        );
    }

    #[test]
    fn alpha_input_validation() {
        let (w, h) = (16u32, 16u32);
        let n = (w * h) as usize;
        let p = vec![128u8; n];
        // 2 planes: no gray+alpha container pixel format exists.
        let two = [p.clone(), p.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &two,
            premultiplied_alpha: false,
        };
        assert!(encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).is_err());
        assert!(encode(&input, ColorMode::Color, QpSet::LOSSLESS).is_err());
        // 4 planes in Grayscale mode: alpha cannot ride a grayscale image.
        let four = [p.clone(), p.clone(), p.clone(), p.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &four,
            premultiplied_alpha: false,
        };
        assert!(encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).is_err());
        // premultiplied flag without an alpha plane.
        let three = [p.clone(), p.clone(), p.clone()];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &three,
            premultiplied_alpha: true,
        };
        assert!(encode(&input, ColorMode::Color, QpSet::LOSSLESS).is_err());
        // wrong plane length among the 4.
        let bad = [p.clone(), p.clone(), p.clone(), vec![0u8; n - 1]];
        let input = ImageInput {
            width: w,
            height: h,
            planes: &bad,
            premultiplied_alpha: false,
        };
        assert!(encode(&input, ColorMode::Color, QpSet::LOSSLESS).is_err());
    }

    // ------------------------------------------------------------- 5a: deep

    /// Parse just the headers of an encoded file (image + plane), for
    /// asserting the emitted depth fields.
    fn headers_of(jxr: &[u8]) -> crate::decode::decoder::Decoder<'_> {
        let c = crate::decode::container::parse(jxr).expect("container");
        let mut d = crate::decode::decoder::Decoder::new(c.image_data);
        d.parse_headers().expect("headers");
        d
    }

    fn guid_of(jxr: &[u8]) -> String {
        crate::decode::container::parse(jxr)
            .unwrap()
            .pixel_format_uuid
    }

    #[test]
    fn deep_u16_q1_roundtrip_bit_exact() {
        use crate::decode::consts::BD16;
        let mut r = Lcg(0xd16_0001);
        for &(w, h) in &[(48u32, 32u32), (17, 31), (16, 16)] {
            let n = (w * h) as usize;
            // Full 16-bit range noise (extremes included via the gradient mix).
            let gray: Vec<u16> = (0..n)
                .map(|i| ((r.next() >> 32) as u16).wrapping_add(i as u16))
                .collect();
            let planes = [gray.clone()];
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U16(&planes),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
            assert_eq!(
                guid_of(&jxr),
                "24c3dd6f-034e-fe4b-b185-3d77768dc90b",
                "16bppGray GUID"
            );
            let hd = headers_of(&jxr);
            assert_eq!(hd.hdr.output_bitdepth, BD16);
            assert_eq!(hd.planes[0].shift_bits, 0);
            let d = decode_to_planes(&jxr);
            for i in 0..n {
                assert_eq!(d.image_plane[0][i], gray[i] as i32, "{w}x{h} gray px{i}");
            }
            // 3-plane RGB, same discipline.
            let rgb: [Vec<u16>; 3] =
                std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U16(&rgb),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
            assert_eq!(
                guid_of(&jxr),
                "24c3dd6f-034e-fe4b-b185-3d77768dc915",
                "48bppRGB GUID"
            );
            let d = decode_to_planes(&jxr);
            for c in 0..3 {
                for i in 0..n {
                    assert_eq!(d.image_plane[c][i], rgb[c][i] as i32, "{w}x{h} ch{c} px{i}");
                }
            }
        }
    }

    #[test]
    fn deep_i16_q1_roundtrip_bit_exact() {
        use crate::decode::consts::BD16S;
        let mut r = Lcg(0xd16_5);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // Signed noise across the whole i16 range.
        let gray: Vec<i16> = (0..n).map(|_| (r.next() >> 32) as u16 as i16).collect();
        let planes = [gray.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I16(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc913",
            "16bppGrayFixed GUID"
        );
        let hd = headers_of(&jxr);
        assert_eq!(hd.hdr.output_bitdepth, BD16S);
        assert_eq!(hd.planes[0].shift_bits, 0);
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], gray[i] as i32, "px{i}");
        }
        let rgb: [Vec<i16>; 3] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16 as i16).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I16(&rgb),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc912",
            "48bppRGBFixed GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], rgb[c][i] as i32, "ch{c} px{i}");
            }
        }
    }

    #[test]
    fn deep_i32_sheds_shift_bits_and_rejects_scaled() {
        use crate::decode::consts::BD32S;
        let mut r = Lcg(0xd32_5);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // Wide signed values: q1 round-trips the high 22 bits exactly —
        // `(x >> 10) << 10`, the same answer jxrencapp's shift_bits=10 gives.
        let gray: Vec<i32> = (0..n).map(|_| r.next() as i32 >> 4).collect();
        let planes = [gray.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I32(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc93f",
            "32bppGrayFixed GUID"
        );
        let hd = headers_of(&jxr);
        assert_eq!(hd.hdr.output_bitdepth, BD32S);
        assert_eq!(hd.planes[0].shift_bits, 10, "libjxr's default pre-shift");
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], (gray[i] >> 10) << 10, "px{i}");
        }
        // RGB variant.
        let rgb: [Vec<i32>; 3] =
            std::array::from_fn(|_| (0..n).map(|_| r.next() as i32 >> 4).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I32(&rgb),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc918",
            "96bppRGBFixed GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], (rgb[c][i] >> 10) << 10, "ch{c} px{i}");
            }
        }
        // Scaled arithmetic would overflow the i32 transform — explicit error.
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I32(&planes),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            scaled: true,
            ..Default::default()
        };
        assert!(encode_typed(&input, ColorMode::Grayscale, opts.clone()).is_err());
    }

    #[test]
    fn deep_f16_q1_bit_pattern_exact() {
        use crate::decode::consts::BD16F;
        let mut r = Lcg(0xf16_0001);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // Arbitrary bit patterns — NaN payloads, infinities, denormals all
        // included. The single non-survivor: -0.0 (0x8000) canonicalizes to
        // +0.0, exactly as the reference encoder does (probed).
        let canon = |v: u16| -> i32 { if v == 0x8000 { 0 } else { v as i32 } };
        let gray: Vec<u16> = (0..n).map(|_| (r.next() >> 32) as u16).collect();
        let planes = [gray.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F16(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc93e",
            "16bppGrayHalf GUID"
        );
        let hd = headers_of(&jxr);
        assert_eq!(hd.hdr.output_bitdepth, BD16F);
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(
                d.image_plane[0][i],
                canon(gray[i]),
                "gray px{i} bits {:#06x}",
                gray[i]
            );
        }
        // Specials, explicitly: ±0, ±Inf, quiet/neg NaN, ±denormal, ±1.0.
        let mut specials = vec![
            0x0000u16, 0x8000, 0x7c00, 0xfc00, 0x7e00, 0xfe00, 0x0001, 0x8001, 0x3c00, 0xbc00,
        ];
        specials.resize(256, 0x4248); // pad to 16x16 with 3.14-ish
        let planes = [specials.clone()];
        let input = TypedInput {
            width: 16,
            height: 16,
            samples: SamplePlanes::F16(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        let d = decode_to_planes(&jxr);
        for (i, &s) in specials.iter().enumerate() {
            assert_eq!(d.image_plane[0][i], canon(s), "special {s:#06x}");
        }
        // RGB halfs + scaled gray: both stay bit-pattern-exact at q1.
        let rgb: [Vec<u16>; 3] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F16(&rgb),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc93b",
            "48bppRGBHalf GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], canon(rgb[c][i]), "ch{c} px{i}");
            }
        }
        let planes = [gray.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F16(&planes),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            scaled: true,
            ..Default::default()
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, opts.clone()).unwrap();
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], canon(gray[i]), "scaled px{i}");
        }
    }

    #[test]
    fn deep_f32_q1_grid_exact_rounding_and_specials() {
        use crate::decode::consts::BD32F;
        let mut r = Lcg(0xf32_0001);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // Values ON the custom-float grid (13 mantissa bits, exponent within
        // the custom-normal range) round-trip bit-exactly at q1.
        let grid = |r: &mut Lcg| -> u32 {
            let sign = (((r.next() >> 13) & 1) as u32) << 31;
            let e = 124 + ((r.next() >> 17) % 77) as u32; // custom-normal range
            let m13 = ((r.next() >> 23) & 0x1fff) as u32;
            sign | (e << 23) | (m13 << 10)
        };
        let gray: Vec<u32> = (0..n).map(|_| grid(&mut r)).collect();
        let planes = [gray.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F32(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc911",
            "32bppGrayFloat GUID"
        );
        let hd = headers_of(&jxr);
        assert_eq!(hd.hdr.output_bitdepth, BD32F);
        assert_eq!(
            (hd.planes[0].len_mantissa, hd.planes[0].exp_bias),
            (13, 4),
            "reference defaults"
        );
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(
                d.image_plane[0][i] as u32, gray[i],
                "grid px{i} bits {:#010x}",
                gray[i]
            );
        }
        // RGB grid values, 128bppRGBFloat.
        let rgb: [Vec<u32>; 3] = std::array::from_fn(|_| (0..n).map(|_| grid(&mut r)).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F32(&rgb),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc91b",
            "128bppRGBFloat GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i] as u32, rgb[c][i], "ch{c} px{i}");
            }
        }
        // Rounding semantics (round-half-up on the dropped 10 bits, carry
        // into the exponent) + specials, via a 16x16 single-MB plane.
        let cases: &[(u32, u32)] = &[
            (0x0000_0000, 0x0000_0000), // +0
            (0x8000_0000, 0x0000_0000), // -0 → +0 (single zero)
            (0x7f80_0000, 0x7f80_0000), // +Inf
            (0xff80_0000, 0xff80_0000), // −Inf
            (0x7fc0_0000, 0x7fc0_0000), // quiet NaN (payload top bit on the grid)
            (0x3f80_03ff, 0x3f80_0400), // 1.0+ε rounds UP to the next grid step
            (0x3f80_01ff, 0x3f80_0000), // below half-step rounds DOWN
            (0x3fff_ffff, 0x4000_0000), // mantissa carry: ~1.9999999 → 2.0
            (0xbf80_03ff, 0xbf80_0400), // negative mirror of the round-up
            (0x4248_f5c3, 0x4248_f400), // π·16-ish truncates+rounds on grid
        ];
        let mut vals: Vec<u32> = cases.iter().map(|&(i, _)| i).collect();
        vals.resize(256, 0x3f80_0000);
        let planes = [vals];
        let input = TypedInput {
            width: 16,
            height: 16,
            samples: SamplePlanes::F32(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        let d = decode_to_planes(&jxr);
        for (i, &(input_bits, want)) in cases.iter().enumerate() {
            assert_eq!(
                d.image_plane[0][i] as u32, want,
                "case {input_bits:#010x} → got {:#010x}, want {want:#010x}",
                d.image_plane[0][i] as u32
            );
        }
        // Idempotence: arbitrary finite floats → q1 → decoded values are
        // canonical (re-encoding them is bit-exact, encode∘decode a fixpoint).
        let arb: Vec<u32> = (0..n)
            .map(|_| {
                let bits = (r.next() >> 32) as u32;
                let e = (bits >> 23) & 0xff;
                if e == 0 || e == 0xff {
                    bits & 0x7fff_ffff | 0x3f80_0000
                } else {
                    bits
                }
            })
            .collect();
        let planes = [arb];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F32(&planes),
            premultiplied_alpha: false,
        };
        let jxr1 = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        let d1 = decode_to_planes(&jxr1);
        let canon: Vec<u32> = d1.image_plane[0].iter().map(|&v| v as u32).collect();
        let planes2 = [canon.clone()];
        let input2 = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F32(&planes2),
            premultiplied_alpha: false,
        };
        let jxr2 = encode_typed(&input2, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
        let d2 = decode_to_planes(&jxr2);
        for i in 0..n {
            assert_eq!(d2.image_plane[0][i] as u32, canon[i], "fixpoint px{i}");
        }
        // Scaled arithmetic rejected for 32-bit floats too.
        let opts = EncodeOptions {
            scaled: true,
            ..Default::default()
        };
        assert!(encode_typed(&input2, ColorMode::Grayscale, opts.clone()).is_err());
    }

    #[test]
    fn rgbe_q1_normalized_roundtrip_exact() {
        use crate::decode::consts::{BD8, OUT_RGBE};
        let mut r = Lcg(0x46be_0001);
        for &(w, h) in &[(48u32, 32u32), (17, 31)] {
            let n = (w * h) as usize;
            // Normalized RGBE (the .hdr convention: max mantissa ≥ 128),
            // varied exponents; a few zero pixels (E = 0).
            let mut planes: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0u8; n]);
            for i in 0..n {
                if i % 37 == 0 {
                    continue; // zero pixel
                }
                let m: [u8; 3] = std::array::from_fn(|_| (r.next() >> 32) as u8);
                let hi = (0..3).max_by_key(|&k| m[k]).unwrap();
                for k in 0..3 {
                    planes[k][i] = if k == hi { m[k] | 0x80 } else { m[k] };
                }
                planes[3][i] = 120 + ((r.next() >> 32) % 31) as u8;
            }
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::Rgbe(&planes),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
            assert_eq!(
                guid_of(&jxr),
                "24c3dd6f-034e-fe4b-b185-3d77768dc93d",
                "32bppRGBE GUID"
            );
            let hd = headers_of(&jxr);
            assert_eq!(hd.hdr.output_clr_fmt, OUT_RGBE);
            assert_eq!(hd.hdr.output_bitdepth, BD8);
            let d = decode_to_planes(&jxr);
            assert_eq!(d.num_components, 4, "R, G, B + derived E");
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes[c][i] as i32,
                        "{w}x{h} ch{c} px{i} (E={})",
                        planes[3][i]
                    );
                }
            }
        }
    }

    #[test]
    fn rgbe_unnormalized_preserves_value_and_validation() {
        // Unnormalized pixels (all mantissas < 128) renormalize: bytes
        // change, the represented value (m · 2^E) is preserved exactly —
        // the half-bit imputation is below the byte's precision.
        let n = 256usize;
        let planes: [Vec<u8>; 4] = [vec![5u8; n], vec![64u8; n], vec![17u8; n], vec![136u8; n]];
        let input = TypedInput {
            width: 16,
            height: 16,
            samples: SamplePlanes::Rgbe(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        let d = decode_to_planes(&jxr);
        let value = |m: i32, e: i32| -> f64 {
            if e == 0 {
                0.0
            } else {
                m as f64 * ((e - 136) as f64).exp2()
            }
        };
        for i in 0..n {
            for c in 0..3 {
                let got = value(d.image_plane[c][i], d.image_plane[3][i]);
                let want = value(planes[c][i] as i32, planes[3][i] as i32);
                // forwardRGBE imputes a half bit on the first shift: the
                // value moves by exactly half an input ulp at that exponent.
                let ulp = ((planes[3][i] as i32 - 136) as f64).exp2();
                assert!(
                    (got - want).abs() <= ulp / 2.0 + 1e-12,
                    "px{i} ch{c}: {got} vs {want} (ulp {ulp})"
                );
            }
        }
        // Subsampled chroma is rejected for RGBE (and floats).
        for chroma in [ChromaSampling::Yuv420, ChromaSampling::Yuv422] {
            let opts = EncodeOptions {
                chroma,
                ..Default::default()
            };
            assert!(encode_typed(&input, ColorMode::Color, opts.clone()).is_err());
        }
        // 3-plane RGBE is malformed.
        let three = [planes[0].clone(), planes[1].clone(), planes[2].clone()];
        let input3 = TypedInput {
            width: 16,
            height: 16,
            samples: SamplePlanes::Rgbe(&three),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input3, ColorMode::Color, EncodeOptions::default()).is_err());
    }

    #[test]
    fn deep_scaled_u16_gray_q1_exact() {
        // Scaled arithmetic stays exactly invertible for 16-bit gray, as for
        // 8-bit: the conversion's `<< 3` is floored back by the decoder's
        // `(v + 4) >> 3`.
        let mut r = Lcg(0xd16_3ca1ed);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let gray: Vec<u16> = (0..n).map(|_| (r.next() >> 32) as u16).collect();
        let planes = [gray.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&planes),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            scaled: true,
            ..Default::default()
        };
        let jxr = encode_typed(&input, ColorMode::Grayscale, opts.clone()).unwrap();
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], gray[i] as i32, "px{i}");
        }
    }

    #[test]
    fn deep_structural_options_compose() {
        // The Phase-4 structural machinery is depth-agnostic: tiles + overlap
        // + frequency order + explicit window margins on BD16 RGB stays
        // bit-exact at q1.
        let mut r = Lcg(0xd16_57ac);
        let (w, h) = (70u32, 38u32);
        let n = (w * h) as usize;
        let rgb: [Vec<u16>; 3] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&rgb),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            tile_cols: 2,
            tile_rows: 2,
            overlap: Overlap::Two,
            frequency: true,
            window_top: 5,
            window_left: 9,
            ..Default::default()
        };
        let jxr = encode_typed(&input, ColorMode::Color, opts.clone()).unwrap();
        let d = decode_to_planes(&jxr);
        assert_eq!((d.width, d.height), (w, h));
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], rgb[c][i] as i32, "ch{c} px{i}");
            }
        }
        // Lossy + 4:2:0 + band truncation on deep input: decodes to shape,
        // error bounded by the subsample/quant loss (sanity, not parity —
        // parity is the harness's job).
        let opts = EncodeOptions {
            qp: QpSet {
                dc: 16,
                lp: 32,
                hp: 64,
            },
            chroma: ChromaSampling::Yuv420,
            bands: BandsPresent::NoFlexbits,
            scaled: true,
            ..Default::default()
        };
        let jxr = encode_typed(&input, ColorMode::Color, opts.clone()).unwrap();
        let d = decode_to_planes(&jxr);
        assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
        // chroma_qp (COMP_SEPARATE) smoke on deep color.
        let opts = EncodeOptions {
            qp: QpSet {
                dc: 4,
                lp: 8,
                hp: 16,
            },
            chroma_qp: Some(QpSet {
                dc: 16,
                lp: 32,
                hp: 64,
            }),
            ..Default::default()
        };
        let jxr = encode_typed(&input, ColorMode::Color, opts.clone()).unwrap();
        let d = decode_to_planes(&jxr);
        assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
    }

    #[test]
    fn deep_auto_gray_collapses_to_family_gray() {
        let mut r = Lcg(0xd16_96a7);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let g: Vec<u16> = (0..n).map(|_| (r.next() >> 32) as u16).collect();
        let rgb = [g.clone(), g.clone(), g.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&rgb),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc90b",
            "collapses to 16bppGray"
        );
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 1);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], g[i] as i32, "px{i}");
        }
    }

    #[test]
    fn deep_yonly_from_color_replicates_luma() {
        let mut r = Lcg(0xd16_404c);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let rgb: [Vec<u16>; 3] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&rgb),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            chroma: ChromaSampling::YOnly,
            ..Default::default()
        };
        let jxr = encode_typed(&input, ColorMode::Color, opts.clone()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc915",
            "stays 48bppRGB"
        );
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 3);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], d.image_plane[1][i], "R==G px{i}");
            assert_eq!(d.image_plane[1][i], d.image_plane[2][i], "G==B px{i}");
        }
    }

    #[test]
    fn encode_typed_u8_is_byte_stable() {
        // The U8 variant routes through the classic path byte-for-byte.
        let mut r = Lcg(0x08_b17e);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let opts = EncodeOptions {
            qp: QpSet {
                dc: 4,
                lp: 8,
                hp: 16,
            },
            chroma: ChromaSampling::Yuv420,
            scaled: true,
            ..Default::default()
        };
        let via_typed = encode_typed(
            &TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U8(&planes),
                premultiplied_alpha: false,
            },
            ColorMode::Color,
            opts.clone(),
        )
        .unwrap();
        let via_classic = encode_with_options(
            &ImageInput {
                width: w,
                height: h,
                planes: &planes,
                premultiplied_alpha: false,
            },
            ColorMode::Color,
            opts.clone(),
        )
        .unwrap();
        assert_eq!(via_typed, via_classic);
    }

    #[test]
    fn deep_rgba_q1_all_families_exact() {
        // 4-plane (RGB + alpha image plane) at every deep family: all four
        // channels bit-exact at q1 (BD32S masked by its shift; F16
        // canonicalizes -0).
        let mut r = Lcg(0xa1fa_d33b);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // U16 → 64bppRGBA.
        let p16: [Vec<u16>; 4] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&p16),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc916",
            "64bppRGBA GUID"
        );
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 4);
        assert!(d.has_alpha && !d.premultiplied_alpha);
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], p16[c][i] as i32, "U16 ch{c} px{i}");
            }
        }
        // Premultiplied U16 → 64bppPRGBA + flag.
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&p16),
            premultiplied_alpha: true,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc917",
            "64bppPRGBA GUID"
        );
        assert!(decode_to_planes(&jxr).premultiplied_alpha);
        // I16 → 64bppRGBAFixedPoint.
        let pi16: [Vec<i16>; 4] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16 as i16).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I16(&pi16),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc91d",
            "64bppRGBAFixed GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], pi16[c][i] as i32, "I16 ch{c} px{i}");
            }
        }
        // I32 → 128bppRGBAFixedPoint (shift-masked).
        let pi32: [Vec<i32>; 4] =
            std::array::from_fn(|_| (0..n).map(|_| r.next() as i32 >> 4).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I32(&pi32),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc91e",
            "128bppRGBAFixed GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(
                    d.image_plane[c][i],
                    (pi32[c][i] >> 10) << 10,
                    "I32 ch{c} px{i}"
                );
            }
        }
        // F16 → 64bppRGBAHalf (bit patterns; -0 canonicalizes).
        let pf16: [Vec<u16>; 4] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
        let canon = |v: u16| -> i32 { if v == 0x8000 { 0 } else { v as i32 } };
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F16(&pf16),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc93a",
            "64bppRGBAHalf GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], canon(pf16[c][i]), "F16 ch{c} px{i}");
            }
        }
        // F32 → 128bppRGBAFloat (grid values), + premultiplied variant.
        let grid = |r: &mut Lcg| -> u32 {
            let sign = (((r.next() >> 13) & 1) as u32) << 31;
            let e = 124 + ((r.next() >> 17) % 77) as u32;
            let m13 = ((r.next() >> 23) & 0x1fff) as u32;
            sign | (e << 23) | (m13 << 10)
        };
        let pf32: [Vec<u32>; 4] = std::array::from_fn(|_| (0..n).map(|_| grid(&mut r)).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F32(&pf32),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc919",
            "128bppRGBAFloat GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i] as u32, pf32[c][i], "F32 ch{c} px{i}");
            }
        }
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F32(&pf32),
            premultiplied_alpha: true,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc91a",
            "128bppPRGBAFloat GUID"
        );
        // Premultiplied has no GUID for the fixed/half families.
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::I16(&pi16),
            premultiplied_alpha: true,
        };
        assert!(encode_typed(&input, ColorMode::Color, EncodeOptions::default()).is_err());
        // Deep alpha QP independence: lossy alpha leaves lossless RGB exact.
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&p16),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            alpha_qp: Some(QpSet {
                dc: 32,
                lp: 64,
                hp: 128,
            }),
            ..Default::default()
        };
        let d = decode_to_planes(&encode_typed(&input, ColorMode::Color, opts.clone()).unwrap());
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], p16[c][i] as i32, "RGB must stay exact");
            }
        }
        assert!(
            (0..n).any(|i| d.image_plane[3][i] != p16[3][i] as i32),
            "lossy alpha must show quantization error"
        );
    }

    // ---------------------------------------------------------- 6a/6b: multi

    #[test]
    fn cmyk_q1_roundtrip_exact_u8_and_u16() {
        use crate::decode::consts::{BD8, BD16, OUT_CMYK};
        let mut r = Lcg(0xc111_0001);
        for &(w, h) in &[(48u32, 32u32), (17, 31)] {
            let n = (w * h) as usize;
            let p8: [Vec<u8>; 4] =
                std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect());
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U8(&p8),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Cmyk, EncodeOptions::default()).unwrap();
            assert_eq!(
                guid_of(&jxr),
                "24c3dd6f-034e-fe4b-b185-3d77768dc91c",
                "32bppCMYK GUID"
            );
            let hd = headers_of(&jxr);
            assert_eq!(
                (hd.hdr.output_clr_fmt, hd.hdr.output_bitdepth),
                (OUT_CMYK, BD8)
            );
            let d = decode_to_planes(&jxr);
            assert_eq!(d.num_components, 4);
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], p8[c][i] as i32,
                        "{w}x{h} u8 ch{c} px{i}"
                    );
                }
            }
            let via_8bit = encode_with_options(
                &ImageInput {
                    width: w,
                    height: h,
                    planes: &p8,
                    premultiplied_alpha: false,
                },
                ColorMode::Cmyk,
                EncodeOptions::default(),
            )
            .unwrap();
            assert_eq!(jxr, via_8bit, "both entry points produce identical bytes");
            let p16: [Vec<u16>; 4] =
                std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U16(&p16),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Cmyk, EncodeOptions::default()).unwrap();
            assert_eq!(
                guid_of(&jxr),
                "24c3dd6f-034e-fe4b-b185-3d77768dc91f",
                "64bppCMYK GUID"
            );
            assert_eq!(headers_of(&jxr).hdr.output_bitdepth, BD16);
            let d = decode_to_planes(&jxr);
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], p16[c][i] as i32,
                        "{w}x{h} u16 ch{c} px{i}"
                    );
                }
            }
        }
    }

    #[test]
    fn cmyk_scaled_and_structural_compose() {
        // Scaled q1 stays exact (sf = 1 everywhere; the lifting + asymmetric
        // bias round-trip in the scaled domain — numerically proven over the
        // full byte range), and the structural envelope composes.
        let mut r = Lcg(0xc111_57ac);
        let (w, h) = (70u32, 38u32);
        let n = (w * h) as usize;
        let p8: [Vec<u8>; 4] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&p8),
            premultiplied_alpha: false,
        };
        // Each structural option singly, then the full unscaled combination —
        // a failure names its option. (Scaled is checked separately below:
        // the component>0 half-step floor makes scaled q1 near-exact, not
        // bit-exact — the same caveat as scaled color.)
        let variants: [(&str, EncodeOptions); 5] = [
            (
                "tiles",
                EncodeOptions {
                    tile_cols: 2,
                    tile_rows: 2,
                    ..Default::default()
                },
            ),
            (
                "overlap1",
                EncodeOptions {
                    overlap: Overlap::One,
                    ..Default::default()
                },
            ),
            (
                "overlap2",
                EncodeOptions {
                    overlap: Overlap::Two,
                    ..Default::default()
                },
            ),
            (
                "frequency",
                EncodeOptions {
                    frequency: true,
                    ..Default::default()
                },
            ),
            (
                "combo",
                EncodeOptions {
                    tile_cols: 2,
                    tile_rows: 2,
                    overlap: Overlap::Two,
                    frequency: true,
                    window_top: 5,
                    window_left: 9,
                    ..Default::default()
                },
            ),
        ];
        for (name, opts) in variants {
            let d = decode_to_planes(&encode_typed(&input, ColorMode::Cmyk, opts.clone()).unwrap());
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(d.image_plane[c][i], p8[c][i] as i32, "[{name}] ch{c} px{i}");
                }
            }
        }
        // Scaled q1: bounded by the half-step floor propagated through the
        // ink lifting (a few code values), never unbounded.
        let opts = EncodeOptions {
            scaled: true,
            ..Default::default()
        };
        let d = decode_to_planes(&encode_typed(&input, ColorMode::Cmyk, opts.clone()).unwrap());
        for c in 0..4 {
            for i in 0..n {
                let e = (d.image_plane[c][i] - p8[c][i] as i32).abs();
                assert!(e <= 4, "scaled ch{c} px{i}: err {e}");
            }
        }
        let opts = EncodeOptions {
            qp: QpSet {
                dc: 8,
                lp: 16,
                hp: 32,
            },
            scaled: true,
            ..Default::default()
        };
        let d = decode_to_planes(&encode_typed(&input, ColorMode::Cmyk, opts.clone()).unwrap());
        assert_eq!((d.width, d.height, d.num_components), (w, h, 4));
        let opts = EncodeOptions {
            qp: QpSet {
                dc: 4,
                lp: 8,
                hp: 16,
            },
            chroma_qp: Some(QpSet {
                dc: 16,
                lp: 32,
                hp: 64,
            }),
            ..Default::default()
        };
        let d = decode_to_planes(&encode_typed(&input, ColorMode::Cmyk, opts.clone()).unwrap());
        assert_eq!(d.num_components, 4);
    }

    #[test]
    fn cmyka_q1_5ch_exact_and_alpha_qp() {
        let mut r = Lcg(0xc111_a1fa);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let p8: [Vec<u8>; 5] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&p8),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Cmyk, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc92c",
            "40bppCMYKAlpha GUID"
        );
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 5, "C,M,Y,K + alpha");
        assert!(d.has_alpha);
        for c in 0..5 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], p8[c][i] as i32, "ch{c} px{i}");
            }
        }
        let p16: [Vec<u16>; 5] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect());
        let input16 = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&p16),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input16, ColorMode::Cmyk, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc92d",
            "80bppCMYKAlpha GUID"
        );
        let d = decode_to_planes(&jxr);
        for c in 0..5 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], p16[c][i] as i32, "u16 ch{c} px{i}");
            }
        }
        let opts = EncodeOptions {
            alpha_qp: Some(QpSet {
                dc: 32,
                lp: 64,
                hp: 128,
            }),
            ..Default::default()
        };
        let d = decode_to_planes(&encode_typed(&input, ColorMode::Cmyk, opts.clone()).unwrap());
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], p8[c][i] as i32, "inks must stay exact");
            }
        }
        assert!((0..n).any(|i| d.image_plane[4][i] != p8[4][i] as i32));
    }

    #[test]
    fn cmykdirect_q1_exact() {
        use crate::decode::consts::OUT_CMYKDIRECT;
        let mut r = Lcg(0xc111_d12e);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let p8: [Vec<u8>; 4] =
            std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect());
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&p8),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::CmykDirect, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc91c",
            "CMYK GUID (direct)"
        );
        assert_eq!(headers_of(&jxr).hdr.output_clr_fmt, OUT_CMYKDIRECT);
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 4);
        for c in 0..4 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], p8[c][i] as i32, "ch{c} px{i}");
            }
        }
    }

    #[test]
    fn ncomponent_q1_exact_3_to_8() {
        use crate::decode::consts::OUT_NCOMPONENT;
        let mut r = Lcg(0x9c03_0001);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        for nch in [3usize, 5, 8] {
            let planes: Vec<Vec<u8>> = (0..nch)
                .map(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect())
                .collect();
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U8(&planes),
                premultiplied_alpha: false,
            };
            let jxr =
                encode_typed(&input, ColorMode::NComponent, EncodeOptions::default()).unwrap();
            let want_last = 0x20 + (nch - 3) as u8;
            assert_eq!(
                guid_of(&jxr),
                format!("24c3dd6f-034e-fe4b-b185-3d77768dc9{want_last:02x}"),
                "{nch}-channel GUID"
            );
            assert_eq!(headers_of(&jxr).hdr.output_clr_fmt, OUT_NCOMPONENT);
            let d = decode_to_planes(&jxr);
            assert_eq!(d.num_components, nch);
            for c in 0..nch {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes[c][i] as i32,
                        "{nch}ch ch{c} px{i}"
                    );
                }
            }
            let planes16: Vec<Vec<u16>> = (0..nch)
                .map(|_| (0..n).map(|_| (r.next() >> 32) as u16).collect())
                .collect();
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U16(&planes16),
                premultiplied_alpha: false,
            };
            let jxr =
                encode_typed(&input, ColorMode::NComponent, EncodeOptions::default()).unwrap();
            let d = decode_to_planes(&jxr);
            for c in 0..nch {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], planes16[c][i] as i32,
                        "u16 {nch}ch ch{c} px{i}"
                    );
                }
            }
        }
    }

    #[test]
    fn multi_mode_validation() {
        let (w, h) = (16u32, 16u32);
        let n = (w * h) as usize;
        let p = vec![128u8; n];
        let four = [p.clone(), p.clone(), p.clone(), p.clone()];
        // Subsampled chroma rejected.
        for chroma in [
            ChromaSampling::Yuv420,
            ChromaSampling::Yuv422,
            ChromaSampling::YOnly,
        ] {
            let opts = EncodeOptions {
                chroma,
                ..Default::default()
            };
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::U8(&four),
                premultiplied_alpha: false,
            };
            assert!(
                encode_typed(&input, ColorMode::Cmyk, opts.clone()).is_err(),
                "{chroma:?}"
            );
        }
        // Wrong plane counts.
        let three = [p.clone(), p.clone(), p.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&three),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Cmyk, EncodeOptions::default()).is_err());
        let nine: Vec<Vec<u8>> = (0..9).map(|_| p.clone()).collect();
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&nine),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::NComponent, EncodeOptions::default()).is_err());
        let two = [p.clone(), p.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&two),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::NComponent, EncodeOptions::default()).is_err());
        // Non-integer families rejected.
        let f16p: [Vec<u16>; 4] = std::array::from_fn(|_| vec![0x3c00u16; n]);
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::F16(&f16p),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Cmyk, EncodeOptions::default()).is_err());
        // Premultiplied rejected.
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U8(&four),
            premultiplied_alpha: true,
        };
        assert!(encode_typed(&input, ColorMode::Cmyk, EncodeOptions::default()).is_err());
    }

    // ---------------------------------------------------------- 6c/6d

    #[test]
    fn packed_q1_roundtrip_exact() {
        use crate::decode::consts::{BD5, BD10, BD565};
        let mut r = Lcg(0xbacc_0001);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // 555.
        let words: Vec<u16> = (0..n).map(|_| ((r.next() >> 32) as u16) & 0x7fff).collect();
        let planes = [words.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::Packed555(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc909",
            "16bppRGB555 GUID"
        );
        assert_eq!(headers_of(&jxr).hdr.output_bitdepth, BD5);
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 1, "packed output is one word plane");
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], words[i] as i32, "555 px{i}");
        }
        // 565.
        let words: Vec<u16> = (0..n).map(|_| (r.next() >> 32) as u16).collect();
        let planes = [words.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::Packed565(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc90a",
            "16bppRGB565 GUID"
        );
        assert_eq!(headers_of(&jxr).hdr.output_bitdepth, BD565);
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], words[i] as i32, "565 px{i}");
        }
        // 101010.
        let words: Vec<u32> = (0..n)
            .map(|_| ((r.next() >> 32) as u32) & 0x3fff_ffff)
            .collect();
        let planes = [words.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::Packed101010(&planes),
            premultiplied_alpha: false,
        };
        let jxr = encode_typed(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        assert_eq!(
            guid_of(&jxr),
            "24c3dd6f-034e-fe4b-b185-3d77768dc914",
            "32bppRGB101010 GUID"
        );
        assert_eq!(headers_of(&jxr).hdr.output_bitdepth, BD10);
        let d = decode_to_planes(&jxr);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], words[i] as i32, "101010 px{i}");
        }
        // Scaled q1: each unpacked channel within the chroma half-step.
        let words: Vec<u16> = (0..n).map(|_| (r.next() >> 32) as u16).collect();
        let planes = [words.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::Packed565(&planes),
            premultiplied_alpha: false,
        };
        let opts = EncodeOptions {
            scaled: true,
            ..Default::default()
        };
        let d = decode_to_planes(&encode_typed(&input, ColorMode::Color, opts.clone()).unwrap());
        for i in 0..n {
            let (gw, ww) = (d.image_plane[0][i] as u16, words[i]);
            let un = |v: u16| {
                [
                    (v & 31) as i32,
                    ((v >> 5) & 63) as i32,
                    ((v >> 11) & 31) as i32,
                ]
            };
            let (a, b) = (un(gw), un(ww));
            for c in 0..3 {
                assert!(
                    (a[c] - b[c]).abs() <= 1,
                    "scaled 565 px{i} ch{c}: {} vs {}",
                    a[c],
                    b[c]
                );
            }
        }
        // Packed input validation: one plane only, Color only.
        let two = [words.clone(), words.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::Packed565(&two),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Color, EncodeOptions::default()).is_err());
        let one = [words.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::Packed565(&one),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).is_err());
    }

    #[test]
    fn bw_q1_roundtrip_both_polarities() {
        use crate::decode::consts::{BD1BLACK1, BD1WHITE1};
        let mut r = Lcg(0xb1_0001);
        for &(w, h) in &[(48u32, 32u32), (17, 31)] {
            let n = (w * h) as usize;
            let bits: Vec<u8> = (0..n).map(|_| ((r.next() >> 32) & 1) as u8).collect();
            let planes = [bits.clone()];
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::Bw(&planes),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
            assert_eq!(
                guid_of(&jxr),
                "24c3dd6f-034e-fe4b-b185-3d77768dc905",
                "BlackWhite GUID"
            );
            assert_eq!(headers_of(&jxr).hdr.output_bitdepth, BD1WHITE1);
            let d = decode_to_planes(&jxr);
            for i in 0..n {
                assert_eq!(d.image_plane[0][i], bits[i] as i32, "{w}x{h} white1 px{i}");
            }
            let input = TypedInput {
                width: w,
                height: h,
                samples: SamplePlanes::BwBlackIsOne(&planes),
                premultiplied_alpha: false,
            };
            let jxr = encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).unwrap();
            assert_eq!(headers_of(&jxr).hdr.output_bitdepth, BD1BLACK1);
            let d = decode_to_planes(&jxr);
            for i in 0..n {
                assert_eq!(d.image_plane[0][i], bits[i] as i32, "{w}x{h} black1 px{i}");
            }
        }
        // Values above 1 rejected; color mode rejected.
        let bad = [vec![2u8; 256]];
        let input = TypedInput {
            width: 16,
            height: 16,
            samples: SamplePlanes::Bw(&bad),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).is_err());
        let ok = [vec![1u8; 256]];
        let input = TypedInput {
            width: 16,
            height: 16,
            samples: SamplePlanes::Bw(&ok),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Color, EncodeOptions::default()).is_err());
    }

    #[test]
    fn deep_input_validation() {
        let (w, h) = (16u32, 16u32);
        let n = (w * h) as usize;
        let p16 = vec![1234u16; n];
        // 2 planes: no gray+alpha pixel format at any depth.
        let two = [p16.clone(), p16.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&two),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Color, EncodeOptions::default()).is_err());
        // 4 deep planes in Grayscale mode: alpha cannot ride grayscale.
        let four = [p16.clone(), p16.clone(), p16.clone(), p16.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&four),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).is_err());
        // Wrong plane length.
        let bad = [vec![0u16; n - 1]];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&bad),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).is_err());
        // 3 planes in Grayscale mode.
        let three = [p16.clone(), p16.clone(), p16.clone()];
        let input = TypedInput {
            width: w,
            height: h,
            samples: SamplePlanes::U16(&three),
            premultiplied_alpha: false,
        };
        assert!(encode_typed(&input, ColorMode::Grayscale, EncodeOptions::default()).is_err());
    }
}
