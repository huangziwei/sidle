//! bokai - Fast ebook converter

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;

use bokai::{
    Book, Chapter, ChapterId, Format, NodeId, Role, ToCss, TocEntry, extract_section_tree,
};

#[derive(Parser)]
#[command(name = "bokai")]
// `BOKAI_VERSION` is the crate version, plus the release tag when build.rs was
// handed one. See bokai/build.rs.
#[command(version = env!("BOKAI_VERSION"), about = "Fast ebook converter", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show book metadata and structure
    Info {
        /// Input file (EPUB, AZW3, or MOBI)
        file: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Convert between ebook formats
    Convert {
        /// Input file (use - for stdin)
        input: String,

        /// Output file (default: stdout for text formats)
        output: Option<String>,

        /// Input format (epub, azw3, mobi, kfx). Required when reading from stdin.
        #[arg(short = 'f', long = "from")]
        from_format: Option<String>,

        /// Output format (md, txt, epub, azw3). Inferred from output extension if not specified.
        #[arg(short = 't', long = "to")]
        to_format: Option<String>,

        /// Suppress output messages
        #[arg(short, long)]
        quiet: bool,

        /// `.kfx-zip` → `.kfx` merge strategy. `fast` (default) passes entity
        /// bodies through verbatim; `mechanical` re-serializes every fragment.
        #[arg(long = "mode", default_value = "fast")]
        merge_mode: String,

        /// Page progression direction for PDF → KFX: `rtl` (Japanese/manga,
        /// turn pages right-to-left) or `ltr`. Omit for the device default
        /// (ltr). A scanned or text PDF carries no such metadata.
        #[cfg(feature = "pdf")]
        #[arg(long = "ppd")]
        ppd: Option<String>,

        /// Force the book's writing mode (EPUB → KFX): `vertical-rl`,
        /// `vertical-lr`, `horizontal-lr`, or `horizontal-rl`. A `-rl` mode
        /// turns pages right-to-left. Omit to keep the source's own mode.
        #[arg(long = "writing-mode")]
        writing_mode: Option<String>,

        /// Skip the native EPUB validator pass on EPUB output. By default a
        /// `→ epub` conversion validates what it wrote and prints the
        /// error-level findings; the conversion succeeds either way.
        #[cfg(feature = "validate")]
        #[arg(long = "no-validate")]
        no_validate: bool,

        /// Worker threads any one parallel stage may run at once (chapter
        /// build, image transcode, document synthesis). Each worker holds one
        /// job's working set. Default 0 = every core the platform reports.
        #[arg(short = 'j', long = "max-workers", default_value_t = 0)]
        max_workers: usize,
    },

    /// Extract hierarchical section tree (JSON)
    Sections {
        /// Input file (EPUB, AZW3, or MOBI)
        file: String,
    },

    /// Dump the IR (Intermediate Representation) for a book
    Dump {
        /// Input file (EPUB, AZW3, or MOBI)
        file: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Show structure only (hide text content)
        #[arg(short, long)]
        structure: bool,

        /// Hide style information
        #[arg(long)]
        no_styles: bool,

        /// Expand styles to show CSS properties (default: show style ID only)
        #[arg(long)]
        styles: bool,

        /// Only dump a specific chapter by ID
        #[arg(short, long)]
        chapter: Option<u32>,

        /// Only dump the style pool
        #[arg(long)]
        styles_only: bool,

        /// Limit tree traversal depth
        #[arg(short, long)]
        depth: Option<usize>,
    },

    /// Validate a conversion. Works in both directions: EPUB→KFX (default)
    /// or KFX→EPUB (via `--direction kfx-to-epub`). The ground truth is
    /// always the source format of the named direction.
    #[cfg(feature = "validate")]
    Validate {
        /// Which conversion direction to interpret. `epub-to-kfx` (default)
        /// treats the EPUB as ground truth; `kfx-to-epub` treats the KFX as
        /// ground truth.
        #[arg(long = "direction", default_value = "epub-to-kfx", global = true)]
        direction: String,

        #[command(subcommand)]
        check: ValidateCheck,
    },

    /// Rebuild a book's table of contents from its own structure (KFX or EPUB).
    /// Prints the chapters the proposer derives; with `output`, writes the
    /// repaired book.
    RepairToc {
        /// Input KFX or EPUB file.
        input: String,

        /// Output path. Omit to only print the proposed chapters (dry run).
        output: Option<String>,
    },

    /// Reorder an EPUB's spine to the order its own navigation reads, for a
    /// book whose spine contradicts its TOC. Prints the proposed reading
    /// order; with `output`, writes the reordered book.
    ReorderSpine {
        /// Input EPUB file.
        input: String,

        /// Output path. Omit to only print the proposed order (dry run).
        output: Option<String>,
    },

    /// Split a collection (合本版 / 全集 / boxed set) into the volumes it
    /// collects. Prints the proposed cuts; with `--out`, writes one EPUB per
    /// volume into that directory.
    Split {
        /// Input book. EPUB is split directly; an AZW3/MOBI/KFX is converted
        /// to EPUB first.
        input: String,

        /// Directory to write the volume EPUBs into. Omit for a dry run.
        #[arg(long = "out")]
        out: Option<String>,

        /// Series name to write into every volume's `belongs-to-collection`.
        #[arg(long = "series")]
        series: Option<String>,
    },
}

#[cfg(feature = "validate")]
fn parse_direction(s: &str) -> Result<bokai::validate::Direction, String> {
    match s {
        "epub-to-kfx" | "epub2kfx" | "e2k" => Ok(bokai::validate::Direction::EpubToKfx),
        "kfx-to-epub" | "kfx2epub" | "k2e" => Ok(bokai::validate::Direction::KfxToEpub),
        "azw3-to-epub" | "azw32epub" | "a2e" => Ok(bokai::validate::Direction::Azw3ToEpub),
        other => Err(format!(
            "--direction must be 'epub-to-kfx', 'kfx-to-epub', or 'azw3-to-epub', got '{other}'"
        )),
    }
}

/// `bokai repair-toc <input> [output]` — derive the chapter list from the
/// book's own structure and print it; with `output`, write the repaired book.
/// KFX reads its Contents page, EPUB its NCX/nav, Contents page or headings.
fn repair_toc_cmd(input: &str, output: Option<&str>) -> Result<(), String> {
    let bytes = std::fs::read(input).map_err(|e| format!("read {input}: {e}"))?;

    if bytes.starts_with(b"PK") {
        let proposed =
            bokai::formats::epub::toc_repair::propose_toc(&bytes).map_err(|e| e.to_string())?;
        println!(
            "proposed {} chapter(s) from the EPUB:",
            count_toc_entries(&proposed, |e| &e.children)
        );
        print_epub_toc(&proposed, 0, &mut 0);
        if let Some(out) = output {
            let repaired =
                bokai::formats::epub::toc_repair::repair_toc(&bytes).map_err(|e| e.to_string())?;
            std::fs::write(out, &repaired).map_err(|e| format!("write {out}: {e}"))?;
            println!("wrote repaired EPUB → {out} ({} bytes)", repaired.len());
            #[cfg(feature = "validate")]
            report_edit_regressions(&bytes, &repaired, "EPUB TOC repair");
        }
        return Ok(());
    }

    let proposed =
        bokai::formats::kfx::toc_repair::propose_toc(&bytes).map_err(|e| e.to_string())?;
    println!(
        "proposed {} chapter(s) from the KFX:",
        count_toc_entries(&proposed, |e| &e.children)
    );
    print_kfx_toc(&proposed, 0, &mut 0);
    if let Some(out) = output {
        let repaired =
            bokai::formats::kfx::toc_repair::repair_toc(&bytes).map_err(|e| e.to_string())?;
        std::fs::write(out, &repaired).map_err(|e| format!("write {out}: {e}"))?;
        println!("wrote repaired KFX → {out} ({} bytes)", repaired.len());
    }
    Ok(())
}

/// `bokai reorder-spine <input> [output]` — print the reading order the
/// navigation implies beside the one the spine declares; with `output`, write
/// the reordered book. The repair moves every reading position downstream.
fn reorder_spine_cmd(input: &str, output: Option<&str>) -> Result<(), String> {
    use bokai::formats::epub::spine_repair as spine;

    let bytes = std::fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
    if !bytes.starts_with(b"PK") {
        return Err("reorder-spine reads EPUB; a KFX reading order moves every \
                    position with it and is not a permutation"
            .to_string());
    }

    let m = spine::declared_spine_misordering(&bytes).map_err(|e| e.to_string())?;
    if !m.contradicts() {
        println!("in order: the spine reads this book's TOC entries in the order it lists them");
        return Ok(());
    }
    println!(
        "misordered: {} place(s) where the spine reads the TOC out of order; {} document(s) would move{}",
        m.descents,
        m.moved,
        if m.machine_sorted {
            "\n  the spine is its own manifest sorted lexicographically — a packaging artifact, not an authored order"
        } else {
            ""
        },
    );

    // The proposed order, one column, with each moved document's old position
    // beside it. Every fixed-width field sits on the left of the title.
    let current = spine::current_spine(&bytes).map_err(|e| e.to_string())?;
    let proposed = spine::propose_spine(&bytes).map_err(|e| e.to_string())?;
    let was: std::collections::HashMap<&str, usize> = current
        .iter()
        .enumerate()
        .map(|(i, d)| (d.idref.as_str(), i))
        .collect();
    println!("\nproposed reading order:");
    for (i, d) in proposed.iter().enumerate() {
        let from = was.get(d.idref.as_str()).copied().unwrap_or(i);
        let mark = if from == i {
            "        ".to_string()
        } else {
            format!("  ← {:<3} ", from + 1)
        };
        println!(
            "  {:>3}{mark}{}",
            i + 1,
            d.label.clone().unwrap_or_else(|| d.href.clone())
        );
    }

    if let Some(out) = output {
        let repaired = spine::repair_spine(&bytes).map_err(|e| e.to_string())?;
        std::fs::write(out, &repaired).map_err(|e| format!("write {out}: {e}"))?;
        println!("\nwrote reordered EPUB → {out} ({} bytes)", repaired.len());
        #[cfg(feature = "validate")]
        report_edit_regressions(&bytes, &repaired, "EPUB spine reorder");
    }
    Ok(())
}

/// `bokai split <input> [--out DIR] [--series NAME]` — print where a
/// collection divides into volumes, one line per proposed cut; with `--out`,
/// write the volumes.
fn split_cmd(input: &str, out: Option<&str>, series: Option<&str>) -> Result<(), String> {
    use bokai::formats::epub::split::{Numbering, propose_cuts, split};

    let bytes = as_epub(input)?;
    let cuts = propose_cuts(&bytes).map_err(|e| e.to_string())?;
    if cuts.is_empty() {
        println!("no volumes: this book evidences no collection to split");
        return Ok(());
    }
    println!("proposed {} volume(s):", cuts.len());
    let docs: usize = cuts.iter().map(|c| c.documents).sum();
    for cut in &cuts {
        let number = match cut.numbering {
            Numbering::Label => format!("{}", cut.number),
            Numbering::Sequence => format!("({})", cut.number),
        };
        println!(
            "  {:>6}  spine {:>5}  {:>4} docs  {:<44}  {}",
            number,
            cut.spine_index,
            cut.documents,
            cut.label,
            cut.cover.as_deref().unwrap_or("-")
        );
    }
    println!(
        "  covering spine {}..{} ({docs} of the book's documents); a number in \
         parentheses was counted, not read from the label",
        cuts[0].spine_index,
        cuts[0].spine_index + docs
    );

    let Some(dir) = out else { return Ok(()) };
    std::fs::create_dir_all(dir).map_err(|e| format!("create {dir}: {e}"))?;
    let volumes = split(&bytes, &cuts, series).map_err(|e| e.to_string())?;
    println!("\nwrote {} volume(s) to {dir}:", volumes.len());
    for (cut, epub) in cuts.iter().zip(&volumes) {
        let name = format!(
            "{:0>2} {}.epub",
            cut.number,
            cut.label.replace(['/', '\\', ':'], "-")
        );
        let path = format!("{dir}/{name}");
        std::fs::write(&path, epub).map_err(|e| format!("write {path}: {e}"))?;
        #[cfg(feature = "validate")]
        let verdict = {
            let report = bokai::validate::source::epub::validate(epub);
            let errors = report.count(bokai::validate::Severity::Error);
            if errors == 0 {
                "valid".to_string()
            } else {
                format!("{errors} ERROR(s):\n{}", report.errors_display())
            }
        };
        #[cfg(not(feature = "validate"))]
        let verdict = "written";
        println!("  {:>10}  {name}  ({verdict})", human_size(epub.len()));
    }
    Ok(())
}

/// Read a book as EPUB bytes, converting first from another importable
/// format.
fn as_epub(input: &str) -> Result<Vec<u8>, String> {
    use bokai::Exporter as _;

    let bytes = std::fs::read(input).map_err(|e| format!("read {input}: {e}"))?;
    let format =
        Format::from_path(input).ok_or_else(|| format!("{input}: not a format bokai can read"))?;
    if format == Format::Epub {
        return Ok(bytes);
    }
    if !format.can_import() {
        return Err(format!("{input}: {format:?} can't be read as a book"));
    }
    let mut book = Book::from_vec(bytes, format).map_err(|e| format!("read {input}: {e}"))?;
    let mut epub = std::io::Cursor::new(Vec::new());
    bokai::EpubExporter::new()
        .export(&mut book, &mut epub)
        .map_err(|e| format!("{input} -> epub: {e}"))?;
    Ok(epub.into_inner())
}

fn human_size(bytes: usize) -> String {
    match bytes {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / (1 << 10) as f64),
        n => format!("{n} B"),
    }
}

/// How many levels deep a TOC tree goes; 0 when there is none.
#[cfg(feature = "validate")]
fn toc_depth(entries: &[bokai::model::TocEntry]) -> usize {
    entries
        .iter()
        .map(|e| 1 + toc_depth(&e.children))
        .max()
        .unwrap_or(0)
}

/// Total entries in a TOC tree, nested children included.
fn count_toc_entries<T>(entries: &[T], children: fn(&T) -> &Vec<T>) -> usize {
    entries
        .iter()
        .map(|e| 1 + count_toc_entries(children(e), children))
        .sum()
}

/// Print a proposed EPUB TOC as the tree it is, indenting one level per depth.
fn print_epub_toc(entries: &[bokai::model::TocEntry], depth: usize, n: &mut usize) {
    for entry in entries {
        *n += 1;
        let pad = "  ".repeat(depth);
        println!("  {:>3}. {pad}{:<44} {}", *n, entry.title, entry.href);
        print_epub_toc(&entry.children, depth + 1, n);
    }
}

fn print_kfx_toc(
    entries: &[bokai::formats::kfx::toc_repair::TocEntry],
    depth: usize,
    n: &mut usize,
) {
    for entry in entries {
        *n += 1;
        println!(
            "  {:>3}. eid {:<8} {}{}",
            *n,
            entry.eid,
            "  ".repeat(depth),
            entry.label
        );
        print_kfx_toc(&entry.children, depth + 1, n);
    }
}

#[cfg(feature = "validate")]
#[derive(Subcommand)]
enum ValidateCheck {
    /// Verify every `<ruby>` pair in the EPUB is preserved in the KFX
    Ruby {
        /// Source EPUB
        epub: String,
        /// Converted KFX
        kfx: String,
        /// Show first N missing/extra pairs (default 20)
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify visible text from spine XHTML is preserved in KFX content
    Text {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Report CSS coverage + class-system richness (class= attr counts, CSS
    /// rule counts, <p> vs <div> leaf ratio) against the KFX baseline
    Style {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Report which HTML tag names get a semantic role vs fall through to generic Container
    Tags {
        epub: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify <a href> links from the source EPUB resolve to KFX anchors
    Links {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify <img src> resources from the source EPUB survive in KFX
    Images {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify headings + TOC from source survive in KFX book_navigation
    Nav {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify OPF metadata (title, language, author, cover, PPD) round-trips
    Metadata {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify book-level writing mode (horizontal-tb / vertical-rl / vertical-lr)
    /// is preserved across the conversion
    WritingMode {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify OPF `<spine page-progression-direction>` matches the source KFX
    /// (ltr / rtl, with the writing-mode override a vertical book takes)
    PageProgression {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Verify a fixed-layout (manga / comic) KFX produced a conformant
    /// pre-paginated EPUB: rendition:layout, per-page viewport, page-spread
    /// properties, and no orphan page thumbnails
    Fxl {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Validate one book's table of contents: properly formed, or
    /// chapterless over content with chapters? Sniffs EPUB vs KFX and reads
    /// only that source. Reports OK / SUSPECT / SPARSE.
    Toc {
        /// Input book (EPUB or KFX)
        file: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Validate one book file's structural conformance on its own. Sniffs
    /// EPUB vs KFX, runs that format's source checks plus the cross-format
    /// TOC audit, and reports one unified list of source defects.
    Source {
        /// Input book (EPUB or KFX)
        file: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Strict A/B tree diff of two EPUBs (byte-exact per zip entry) — the
    /// before/after gate for any change to the EPUB output
    EpubDiff {
        /// Reference EPUB (A — the before)
        a: String,
        /// Candidate EPUB (B — the after)
        b: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show first N per-file differences (default 20)
        #[arg(long, default_value_t = 20)]
        details: usize,
    },

    /// Run all available validations against the conversion
    All {
        epub: String,
        kfx: String,
        #[arg(long, default_value_t = 10)]
        details: usize,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Info { file, json } => show_info(&file, json),
        Command::Sections { file } => show_sections(&file),
        Command::Convert {
            input,
            output,
            from_format,
            to_format,
            quiet,
            merge_mode,
            #[cfg(feature = "pdf")]
            ppd,
            writing_mode,
            #[cfg(feature = "validate")]
            no_validate,
            max_workers,
        } => {
            #[cfg(feature = "pdf")]
            let ppd = ppd.as_deref();
            #[cfg(not(feature = "pdf"))]
            let ppd: Option<&str> = None;
            #[cfg(feature = "validate")]
            let validate_epub = !no_validate;
            #[cfg(not(feature = "validate"))]
            let validate_epub = false;
            convert(
                &input,
                output.as_deref(),
                from_format.as_deref(),
                to_format.as_deref(),
                quiet,
                &merge_mode,
                ppd,
                writing_mode.as_deref(),
                validate_epub,
                max_workers,
            )
        }
        Command::Dump {
            file,
            json,
            structure,
            no_styles,
            styles,
            chapter,
            styles_only,
            depth,
        } => dump_ir(
            &file,
            DumpOptions {
                json,
                structure,
                no_styles,
                styles,
                chapter,
                styles_only,
                depth,
            },
        ),
        #[cfg(feature = "validate")]
        Command::Validate { direction, check } => match parse_direction(&direction) {
            Err(e) => Err(e),
            Ok(dir) => match check {
                ValidateCheck::Ruby { epub, kfx, details } => {
                    validate_ruby(&epub, &kfx, details, dir)
                }
                ValidateCheck::Text { epub, kfx, details } => {
                    validate_text(&epub, &kfx, details, dir)
                }
                ValidateCheck::Style { epub, kfx, details } => validate_style(&epub, &kfx, details),
                ValidateCheck::Tags { epub, details } => validate_tags(&epub, details),
                ValidateCheck::Links { epub, kfx, details } => {
                    validate_links(&epub, &kfx, details, dir)
                }
                ValidateCheck::Images { epub, kfx, details } => {
                    validate_images(&epub, &kfx, details, dir)
                }
                ValidateCheck::Nav { epub, kfx, details } => {
                    validate_nav(&epub, &kfx, details, dir)
                }
                ValidateCheck::Metadata { epub, kfx, details } => {
                    validate_metadata(&epub, &kfx, details, dir)
                }
                ValidateCheck::WritingMode { epub, kfx, details } => {
                    validate_writing_mode(&epub, &kfx, details, dir)
                }
                ValidateCheck::PageProgression { epub, kfx, details } => {
                    validate_page_progression(&epub, &kfx, details, dir)
                }
                ValidateCheck::Fxl { epub, kfx, details } => {
                    validate_fxl(&epub, &kfx, details, dir)
                }
                ValidateCheck::Toc { file, json } => validate_toc(&file, json),
                ValidateCheck::Source { file, json } => validate_source(&file, json),
                ValidateCheck::EpubDiff {
                    a,
                    b,
                    json,
                    details,
                } => validate_epub_diff(&a, &b, json, details),
                ValidateCheck::All { epub, kfx, details } => {
                    validate_all(&epub, &kfx, details, dir)
                }
            },
        },
        Command::RepairToc { input, output } => repair_toc_cmd(&input, output.as_deref()),
        Command::ReorderSpine { input, output } => reorder_spine_cmd(&input, output.as_deref()),
        Command::Split { input, out, series } => {
            split_cmd(&input, out.as_deref(), series.as_deref())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Serialize)]
struct BookInfo {
    file: String,
    metadata: MetadataInfo,
    spine: Vec<SpineInfo>,
    toc: Vec<TocInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    landmarks: Vec<LandmarkInfo>,
    assets: Vec<AssetInfo>,
}

#[derive(Serialize)]
struct AssetInfo {
    path: String,
    size: usize,
}

#[derive(Serialize)]
struct MetadataInfo {
    title: String,
    authors: Vec<String>,
    language: String,
    identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subjects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rights: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_date: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    contributors: Vec<ContributorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title_sort: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    author_sorts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<CollectionInfoJson>,
    /// Amazon catalogue id, beside the generic `identifier`.
    #[serde(skip_serializing_if = "Option::is_none")]
    asin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_progression_direction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_writing_mode: Option<String>,
    /// `cde_content_type` for an issue of a periodical.
    #[serde(skip_serializing_if = "Option::is_none")]
    periodical: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    fixed_layout: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    book_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendition_spread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    orientation_lock: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_viewport: Option<(u32, u32)>,
}

#[derive(Serialize)]
struct ContributorInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_as: Option<String>,
}

#[derive(Serialize)]
struct CollectionInfoJson {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    collection_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<f64>,
}

#[derive(Serialize)]
struct SpineInfo {
    id: u32,
    path: String,
    size: usize,
}

#[derive(Serialize)]
struct TocInfo {
    title: String,
    href: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<TocInfo>,
}

#[derive(Serialize)]
struct LandmarkInfo {
    landmark_type: String,
    href: String,
    label: String,
}

#[cfg(feature = "validate")]
fn validate_ruby(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::ruby::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
        Ok(())
    } else {
        let (dropped, fabricated) = if dir.epub_is_source() {
            (&report.only_in_epub, &report.only_in_kfx)
        } else {
            (&report.only_in_kfx, &report.only_in_epub)
        };
        Err(format!(
            "{} dropped, {} fabricated ruby pair(s)",
            dropped.iter().map(|(_, n)| n).sum::<usize>(),
            fabricated.iter().map(|(_, n)| n).sum::<usize>()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_text(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::text::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean_for(dir) {
        Ok(())
    } else {
        let dropped = if dir.epub_is_source() {
            &report.only_in_epub
        } else {
            &report.only_in_kfx
        };
        let total: usize = dropped.iter().map(|(_, n)| n).sum();
        Err(format!(
            "{} characters missing from {}",
            total,
            dir.target_label()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_style(epub_path: &str, kfx_path: &str, details: usize) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;
    let report = bokai::validate::coverage::style::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary();
    if details > 0 {
        report.print_details(details);
    }
    if report.is_clean() {
        Ok(())
    } else {
        let mut bits: Vec<String> = Vec::new();
        if report.dropped > 0 {
            bits.push(format!("{} CSS declarations dropped", report.dropped));
        }
        if report.classes_collapsed_to_zero() {
            bits.push("no class system emitted".into());
        }
        if report.paragraphs_stuck_as_divs() {
            bits.push("paragraphs stuck as <div>".into());
        }
        Err(bits.join("; "))
    }
}

#[cfg(feature = "validate")]
fn validate_tags(epub_path: &str, details: usize) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let report = bokai::validate::coverage::tags::validate(&epub_bytes)?;
    report.print_summary();
    if details > 0 {
        report.print_details(details);
    }
    if report.is_clean() {
        Ok(())
    } else {
        let fallback = report
            .by_bucket
            .get(&bokai::validate::coverage::tags::Bucket::Fallback)
            .copied()
            .unwrap_or(0);
        Err(format!("{} elements with no role_map entry", fallback))
    }
}

#[cfg(feature = "validate")]
fn validate_links(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::links::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean_for(dir) {
        Ok(())
    } else {
        let dropped = if dir.epub_is_source() {
            &report.external_only_in_epub
        } else {
            &report.external_only_in_kfx
        };
        let dropped_n: usize = dropped.iter().map(|(_, n)| n).sum();
        Err(format!(
            "{} external dropped, {} dangling anchors, {} orphan link_to",
            dropped_n,
            report.dangling_anchors.len(),
            report.orphan_link_tos.len()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_images(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::images::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "{} dropped, {} dangling resources, {} orphan refs",
            report.dropped_count(dir),
            report.dangling_external_resources.len(),
            report.orphan_image_refs.len()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_nav(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::nav::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "{} dangling, {} heading diffs, TOC diff {:?}",
            report.dangling_nav.len(),
            report.heading_count_diffs.len(),
            report.toc_count_diff
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_metadata(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::metadata::validate(&epub_bytes, &kfx_bytes, dir)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "{} metadata field(s) mismatched",
            report.diffs.len()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_writing_mode(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::writing_mode::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "writing-mode mismatch: EPUB={}  KFX={}",
            report.epub_book_mode.as_css(),
            report.kfx_book_mode.as_css()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_page_progression(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::page_progression::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "page-progression-direction mismatch: EPUB={}  KFX={}",
            report.epub_ppd.as_str(),
            report.kfx_ppd.as_str()
        ))
    }
}

#[cfg(feature = "validate")]
fn validate_fxl(
    epub_path: &str,
    kfx_path: &str,
    _details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = bokai::validate::fidelity::fxl::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if report.is_clean() {
        Ok(())
    } else {
        Err("fixed-layout structure not conformant (see report above)".to_string())
    }
}

#[cfg(feature = "validate")]
fn validate_all(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: bokai::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    println!(
        "=== Direction: {} → {} ===",
        dir.source_label(),
        dir.target_label()
    );
    let mut all_clean = true;

    // A fixed-layout (manga / image) book is image pages: ruby, text, CSS,
    // tags and writing-mode print as information, while `fxl` shape plus
    // images / nav / metadata / links / PPD gate.
    let fxl = bokai::validate::fidelity::fxl::validate(&epub_bytes, &kfx_bytes)?;
    let reflow_gated = !fxl.kfx_fixed_layout;
    if fxl.kfx_fixed_layout {
        println!(
            "(fixed-layout book — ruby/text/style/tags/writing-mode are \
             informational; gating on FXL shape + images/nav/metadata/links/PPD)"
        );
    }

    println!("=== Ruby ===");
    let ruby = bokai::validate::fidelity::ruby::validate(&epub_bytes, &kfx_bytes)?;
    ruby.print_summary(dir);
    if details > 0 {
        ruby.print_details(details, dir);
    }
    if reflow_gated && !ruby.is_clean() {
        all_clean = false;
    }

    println!("\n=== Text ===");
    let text = bokai::validate::fidelity::text::validate(&epub_bytes, &kfx_bytes)?;
    text.print_summary(dir);
    if details > 0 {
        text.print_details(details, dir);
    }
    if reflow_gated && !text.is_clean_for(dir) {
        all_clean = false;
    }

    println!("\n=== Style ===");
    let style = bokai::validate::coverage::style::validate(&epub_bytes, &kfx_bytes)?;
    style.print_summary();
    if details > 0 {
        style.print_details(details);
    }
    if reflow_gated && !style.is_clean() {
        all_clean = false;
    }

    println!("\n=== Tags ===");
    let tags = bokai::validate::coverage::tags::validate(&epub_bytes)?;
    tags.print_summary();
    if details > 0 {
        tags.print_details(details);
    }
    if reflow_gated && !tags.is_clean() {
        all_clean = false;
    }

    println!("\n=== Links ===");
    let links = bokai::validate::fidelity::links::validate(&epub_bytes, &kfx_bytes)?;
    links.print_summary(dir);
    if details > 0 {
        links.print_details(details, dir);
    }
    if !links.is_clean_for(dir) {
        all_clean = false;
    }

    println!("\n=== Images ===");
    let images = bokai::validate::fidelity::images::validate(&epub_bytes, &kfx_bytes)?;
    images.print_summary(dir);
    if details > 0 {
        images.print_details(details);
    }
    if !images.is_clean() {
        all_clean = false;
    }

    println!("\n=== Nav ===");
    let nav = bokai::validate::fidelity::nav::validate(&epub_bytes, &kfx_bytes)?;
    nav.print_summary(dir);
    if details > 0 {
        nav.print_details(details, dir);
    }
    if !nav.is_clean() {
        all_clean = false;
    }

    println!("\n=== Metadata ===");
    let metadata = bokai::validate::fidelity::metadata::validate(&epub_bytes, &kfx_bytes, dir)?;
    metadata.print_summary(dir);
    if details > 0 {
        metadata.print_details(details, dir);
    }
    if !metadata.is_clean() {
        all_clean = false;
    }

    println!("\n=== Writing mode ===");
    let wm = bokai::validate::fidelity::writing_mode::validate(&epub_bytes, &kfx_bytes)?;
    wm.print_summary(dir);
    if reflow_gated && !wm.is_clean() {
        all_clean = false;
    }

    println!("\n=== Page progression direction ===");
    let ppd = bokai::validate::fidelity::page_progression::validate(&epub_bytes, &kfx_bytes)?;
    ppd.print_summary(dir);
    if !ppd.is_clean() {
        all_clean = false;
    }

    println!("\n=== Fixed layout ===");
    fxl.print_summary(dir);
    if !fxl.is_clean() {
        all_clean = false;
    }

    println!("\n=== Scorecard ===");
    // Ruby: denominator is the source side's pair count.
    let ruby_source_count = if dir.epub_is_source() {
        ruby.epub_pairs.len()
    } else {
        ruby.kfx_pairs.len()
    };
    let ruby_pct = if ruby_source_count == 0 {
        100.0
    } else {
        ruby.matched as f64 * 100.0 / ruby_source_count as f64
    };
    println!("  Ruby pairs:   {:.2}% preserved", ruby_pct);
    println!(
        "  Text chars:   {:.4}% preserved",
        text.preservation_ratio(dir) * 100.0
    );
    println!(
        "  CSS props:    {:.2}% covered",
        style.coverage_ratio() * 100.0
    );
    println!(
        "  HTML tags:    {:.2}% semantic",
        tags.semantic_ratio() * 100.0
    );
    let url_source_count = if dir.epub_is_source() {
        links.epub_external_count
    } else {
        links.kfx_external_anchor_count
    };
    let link_score = if url_source_count == 0 {
        100.0
    } else {
        let dropped = if dir.epub_is_source() {
            &links.external_only_in_epub
        } else {
            &links.external_only_in_kfx
        };
        let dropped_n: usize = dropped.iter().map(|(_, n)| n).sum();
        let preserved = url_source_count.saturating_sub(dropped_n);
        preserved as f64 * 100.0 / url_source_count as f64
    };
    println!("  External URLs:{:.2}% preserved", link_score);
    println!(
        "  Images:       {:.2}% preserved",
        images.preservation_ratio(dir) * 100.0
    );
    println!(
        "  Writing mode: {}",
        if wm.is_clean() { "preserved" } else { "LOST" }
    );
    println!(
        "  Page progression: {} ({} on KFX side)",
        if ppd.is_clean() { "preserved" } else { "LOST" },
        ppd.kfx_ppd.as_str()
    );

    if all_clean {
        Ok(())
    } else {
        Err("one or more checks failed".into())
    }
}

fn show_info(path: &str, json: bool) -> Result<(), String> {
    let mut book = Book::open(path).map_err(|e| e.to_string())?;

    if json {
        print_json(&mut book, path)
    } else {
        print_human(&mut book, path)
    }
}

fn show_sections(path: &str) -> Result<(), String> {
    let mut book = Book::open(path).map_err(|e| e.to_string())?;
    let tree = extract_section_tree(&mut book).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

#[cfg(feature = "validate")]
fn validate_toc(path: &str, json: bool) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    let audit = bokai::validate::source::toc::validate(&bytes)?;
    if json {
        let payload = serde_json::json!({
            "verdict": audit.verdict.as_str(),
            "nav_count": audit.nav_count,
            "nav_chapters": audit.nav_chapters,
            "fm_only": audit.fm_only,
            "contents_links": audit.contents_links,
            "headings": audit.headings,
            "section_heads": audit.section_heads,
            "has_toc_landmark": audit.has_toc_landmark,
            "flattened_volumes": audit.flattened.volumes,
            "flattened_entries": audit.flattened.misplaced,
            // How many levels the declared TOC itself has: 1 is a flat list, 0
            // no TOC at all. `flattened_*` counts the levels it lacks.
            "nav_depth": toc_depth(&audit.nav_tree),
            "nav_labels": audit.nav_labels,
            "contents_sample": audit.contents_sample,
        });
        println!(
            "{}",
            serde_json::to_string(&payload).map_err(|e| e.to_string())?
        );
    } else {
        audit.print_summary();
    }
    // A deficient TOC fails validation: chapterless, flattened, or
    // contradicted by the reading order. SPARSE is inconclusive. In --json
    // mode the verdict sits in the payload, with no process-level error.
    use bokai::validate::source::toc::Verdict;
    if json || audit.is_clean() {
        return Ok(());
    }
    let mut problems = Vec::new();
    if audit.misordered.contradicts() {
        problems.push(format!(
            "spine misordered: it reads the declared TOC out of order in {} place(s), {} document(s) out of position",
            audit.misordered.descents, audit.misordered.moved,
        ));
    }
    if audit.flattened.misplaced > 0 {
        problems.push(format!(
            "TOC flattened: {} volumes and their chapters are listed at one depth ({} entries belong under a volume)",
            audit.flattened.volumes, audit.flattened.misplaced,
        ));
    }
    if audit.verdict == Verdict::Suspect {
        problems.push(format!(
            "TOC deficient: declared {} chapter entries, but the book has {} in-book chapters",
            audit.nav_chapters,
            audit
                .contents_links
                .max(audit.headings)
                .max(audit.section_heads),
        ));
    }
    Err(problems.join("; "))
}

#[cfg(feature = "validate")]
fn validate_source(path: &str, json: bool) -> Result<(), String> {
    use bokai::validate::Severity;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    let report = bokai::validate::source::validate(&bytes);

    if json {
        let findings: Vec<_> = report
            .findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "check": f.check,
                    "rule": f.rule,
                    "severity": f.severity.as_str(),
                    "location": f.location,
                    "message": f.message,
                    "fix": f.fix.as_ref().map(|h| serde_json::json!({
                        "action": h.action,
                        "detail": h.detail,
                    })),
                })
            })
            .collect();
        let payload = serde_json::json!({
            "clean": report.is_clean(),
            "errors": report.count(Severity::Error),
            "warnings": report.count(Severity::Warning),
            "infos": report.count(Severity::Info),
            "findings": findings,
        });
        println!(
            "{}",
            serde_json::to_string(&payload).map_err(|e| e.to_string())?
        );
        return Ok(());
    }

    println!("{report}");
    // The process fails only on error-level defects; a warning prints without
    // failing the run. In --json mode the verdict sits in the payload.
    let errors = report.count(Severity::Error);
    if errors == 0 {
        Ok(())
    } else {
        Err(format!("{errors} source error(s)"))
    }
}

#[cfg(feature = "validate")]
fn validate_epub_diff(
    a_path: &str,
    b_path: &str,
    json: bool,
    details: usize,
) -> Result<(), String> {
    let a = std::fs::read(a_path).map_err(|e| format!("{}: {}", a_path, e))?;
    let b = std::fs::read(b_path).map_err(|e| format!("{}: {}", b_path, e))?;

    let report = bokai::validate::fidelity::epub_diff::validate(&a, &b)?;

    if json {
        let tally: serde_json::Map<String, serde_json::Value> = report
            .class_tally()
            .into_iter()
            .map(|(k, n)| (k.as_str().to_string(), serde_json::json!(n)))
            .collect();
        let payload = serde_json::json!({
            "identical": report.is_clean(),
            "identical_entries": report.identical,
            "only_in_a": report.only_in_a,
            "only_in_b": report.only_in_b,
            "class_tally": tally,
            "differing": report
                .differing
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "path": d.path,
                        "class": d.kind.as_str(),
                        "a_len": d.a_len,
                        "b_len": d.b_len,
                        "first_diff": d.first_diff,
                        "a_context": d.a_context,
                        "b_context": d.b_context,
                    })
                })
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string(&payload).map_err(|e| e.to_string())?
        );
        // The verdict prints to stdout; the exit status stays 0.
        return Ok(());
    }

    report.print_summary();
    if details > 0 {
        report.print_details(details);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!(
            "{} differing, {} only in A, {} only in B",
            report.differing.len(),
            report.only_in_a.len(),
            report.only_in_b.len()
        ))
    }
}

fn print_json(book: &mut Book, path: &str) -> Result<(), String> {
    let meta = book.metadata().clone();
    let asset_paths: Vec<_> = book.list_assets().to_vec();

    let assets: Vec<AssetInfo> = asset_paths
        .iter()
        .map(|p| {
            let size = book.load_asset(p).map(|d| d.len()).unwrap_or(0);
            AssetInfo {
                path: p.to_string_lossy().to_string(),
                size,
            }
        })
        .collect();

    let info = BookInfo {
        file: path.to_string(),
        metadata: MetadataInfo {
            title: meta.title.clone(),
            authors: meta.authors.clone(),
            language: meta.language.clone(),
            identifier: meta.identifier.clone(),
            publisher: meta.publisher.clone(),
            date: meta.date.clone(),
            subjects: meta.subjects.clone(),
            rights: meta.rights.clone(),
            cover_image: meta.cover_image.clone(),
            description: meta.description.clone(),
            modified_date: meta.modified_date.clone(),
            contributors: meta
                .contributors
                .iter()
                .map(|c| ContributorInfo {
                    name: c.name.clone(),
                    role: c.role.clone(),
                    file_as: c.file_as.clone(),
                })
                .collect(),
            title_sort: meta.title_sort.clone(),
            author_sorts: meta.author_sorts.clone(),
            collection: meta.collection.as_ref().map(|c| CollectionInfoJson {
                name: c.name.clone(),
                collection_type: c.collection_type.clone(),
                position: c.position,
            }),
            asin: meta.asin.clone(),
            page_progression_direction: meta.page_progression_direction.clone(),
            primary_writing_mode: meta.primary_writing_mode.clone(),
            periodical: meta.periodical.map(|k| k.cde_type()),
            fixed_layout: meta.fixed_layout,
            book_type: meta.book_type.clone(),
            rendition_spread: meta.rendition_spread.clone(),
            orientation_lock: meta.orientation_lock.map(|l| l.kindle_value()),
            default_viewport: meta.default_viewport,
        },
        spine: book
            .spine()
            .iter()
            .map(|e| SpineInfo {
                id: e.id.0,
                path: book.source_id(e.id).unwrap_or("").to_string(),
                size: e.size_estimate,
            })
            .collect(),
        toc: book.toc().iter().map(toc_to_info).collect(),
        landmarks: book
            .landmarks()
            .iter()
            .map(|l| LandmarkInfo {
                landmark_type: format!("{:?}", l.landmark_type),
                href: l.href.clone(),
                label: l.label.clone(),
            })
            .collect(),
        assets,
    };

    let json = serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

fn toc_to_info(entry: &TocEntry) -> TocInfo {
    TocInfo {
        title: entry.title.clone(),
        href: entry.href.clone(),
        children: entry.children.iter().map(toc_to_info).collect(),
    }
}

fn print_human(book: &mut Book, path: &str) -> Result<(), String> {
    let meta = book.metadata();
    println!("File: {path}");
    println!("Title: {}", meta.title);
    if !meta.authors.is_empty() {
        println!("Authors: {}", meta.authors.join(", "));
    }
    if !meta.language.is_empty() {
        println!("Language: {}", meta.language);
    }
    if !meta.identifier.is_empty() {
        println!("Identifier: {}", meta.identifier);
    }
    if let Some(ref asin) = meta.asin {
        println!("ASIN: {asin}");
    }
    if let Some(ref publisher) = meta.publisher {
        println!("Publisher: {publisher}");
    }
    if let Some(kind) = meta.periodical {
        println!("Periodical: {kind:?} ({})", kind.cde_type());
    }
    if let Some(ref date) = meta.date {
        println!("Date: {date}");
    }
    if !meta.subjects.is_empty() {
        println!("Subjects: {}", meta.subjects.join(", "));
    }
    if let Some(ref rights) = meta.rights {
        println!("Rights: {rights}");
    }
    if let Some(ref cover) = meta.cover_image {
        println!("Cover: {cover}");
    }
    if let Some(ref desc) = meta.description {
        let desc = desc.trim();
        match desc.char_indices().nth(200) {
            Some((cut, _)) => println!("Description: {}...", &desc[..cut]),
            None => println!("Description: {desc}"),
        }
    }
    if let Some(ref modified) = meta.modified_date {
        println!("Modified: {modified}");
    }
    if let Some(ref title_sort) = meta.title_sort {
        println!("Title Sort: {title_sort}");
    }
    if !meta.author_sorts.is_empty() {
        println!("Author Sort: {}", meta.author_sorts.join(" / "));
    }
    if !meta.contributors.is_empty() {
        println!("Contributors:");
        for contrib in &meta.contributors {
            let role = contrib.role.as_deref().unwrap_or("contributor");
            if let Some(ref file_as) = contrib.file_as {
                println!("  {} ({}) [{}]", contrib.name, role, file_as);
            } else {
                println!("  {} ({})", contrib.name, role);
            }
        }
    }
    if let Some(ref coll) = meta.collection {
        let coll_type = coll.collection_type.as_deref().unwrap_or("collection");
        if let Some(pos) = coll.position {
            if pos.fract() == 0.0 {
                println!("Collection: {} ({}, #{})", coll.name, coll_type, pos as i64);
            } else {
                println!("Collection: {} ({}, #{})", coll.name, coll_type, pos);
            }
        } else {
            println!("Collection: {} ({})", coll.name, coll_type);
        }
    }
    if let Some(ref ppd) = meta.page_progression_direction {
        println!("Page progression: {ppd}");
    }
    if let Some(ref wm) = meta.primary_writing_mode {
        println!("Writing mode: {wm}");
    }
    if meta.fixed_layout {
        let book_type = meta.book_type.as_deref().unwrap_or("-");
        println!("Fixed layout: yes (book-type {book_type})");
        if let Some(ref spread) = meta.rendition_spread {
            println!("Spread: {spread}");
        }
        if let Some((w, h)) = meta.default_viewport {
            println!("Viewport: {w}x{h}");
        }
    }
    if let Some(lock) = meta.orientation_lock {
        println!("Orientation lock: {}", lock.kindle_value());
    }

    println!("\nSpine ({} chapters):", book.spine().len());
    for entry in book.spine() {
        let source = book.source_id(entry.id).unwrap_or("?");
        println!(
            "  [{}] {} ({} bytes)",
            entry.id.0, source, entry.size_estimate
        );
    }

    println!("\nTable of Contents ({} entries):", book.toc().len());
    print_toc_human(book.toc(), 1);

    let landmarks = book.landmarks();
    if !landmarks.is_empty() {
        println!("\nLandmarks ({}):", landmarks.len());
        for landmark in landmarks {
            println!(
                "  {:?} -> {} ({})",
                landmark.landmark_type, landmark.href, landmark.label
            );
        }
    }

    let assets: Vec<_> = book.list_assets().to_vec();
    println!("\nAssets ({}):", assets.len());
    for asset in &assets {
        let size = book
            .load_asset(asset)
            .map(|data| format_bytes(data.len()))
            .unwrap_or_else(|_| "?".to_string());
        println!("  {} ({})", asset.display(), size);
    }

    Ok(())
}

/// Format byte size.
fn format_bytes(bytes: usize) -> String {
    format!("{} bytes", bytes)
}

fn print_toc_human(entries: &[TocEntry], depth: usize) {
    for entry in entries {
        let indent = "  ".repeat(depth);
        println!("{}{} -> {}", indent, entry.title, entry.href);
        if !entry.children.is_empty() {
            print_toc_human(&entry.children, depth + 1);
        }
    }
}

fn parse_format(fmt: &str) -> Result<Format, String> {
    match fmt.to_lowercase().as_str() {
        "md" | "markdown" | "txt" | "text" => Ok(Format::Markdown),
        "epub" => Ok(Format::Epub),
        "azw3" => Ok(Format::Azw3),
        "mobi" => Ok(Format::Mobi),
        "kfx" => Ok(Format::Kfx),
        _ => Err(format!(
            "Unknown format: {}. Supported: md, txt, epub, azw3, mobi, kfx",
            fmt
        )),
    }
}

/// Post-conversion EPUB validation diagnostic over bytes the caller has
/// written. `validate` runs the native validator and prints its findings; an
/// error finding prints under `quiet` too. The pass prints and never fails.
#[cfg(feature = "validate")]
fn report_epub_validation(bytes: &[u8], validate: bool, quiet: bool) {
    if !validate {
        return;
    }
    let report = bokai::validate::source::epub::validate(bytes);
    if report.has_errors() {
        eprintln!(
            "EPUB validation: {} error finding(s) — output written but NOT \
             epubcheck-valid (fix the converter and reconvert, or repair in the \
             book editor):\n{}",
            report.count(bokai::validate::Severity::Error),
            report.errors_display()
        );
    } else if !quiet {
        let warnings = report.count(bokai::validate::Severity::Warning);
        if warnings > 0 {
            eprintln!("EPUB validation: {warnings} warning(s) (non-blocking).");
        }
    }
}

/// Export `book` and, for an EPUB, run [`report_epub_validation`] over what
/// was written. A file sink streams the container; `book` drops before the
/// validator reads it back. Stdout is unseekable and buffers in memory.
#[cfg_attr(not(feature = "validate"), allow(unused_variables))]
fn write_export(
    mut book: Book,
    output_format: Format,
    output: Option<&str>,
    to_stdout: bool,
    validate_epub: bool,
    quiet: bool,
) -> Result<(), String> {
    if to_stdout {
        let mut cursor = std::io::Cursor::new(Vec::new());
        book.export(output_format, &mut cursor)
            .map_err(|e| format!("Conversion failed: {e}"))?;
        use std::io::Write;
        std::io::stdout()
            .write_all(cursor.get_ref())
            .map_err(|e| format!("Write failed: {e}"))?;
        #[cfg(feature = "validate")]
        report_epub_validation(cursor.get_ref(), validate_epub, quiet);
        return Ok(());
    }

    let output_path = output.ok_or("Output path required")?;
    let mut file =
        std::fs::File::create(output_path).map_err(|e| format!("Failed to create output: {e}"))?;
    book.export(output_format, &mut file)
        .map_err(|e| format!("Conversion failed: {e}"))?;
    drop(file);
    drop(book);

    #[cfg(feature = "validate")]
    if validate_epub {
        let bytes =
            std::fs::read(output_path).map_err(|e| format!("read back {output_path}: {e}"))?;
        report_epub_validation(&bytes, validate_epub, quiet);
    }
    Ok(())
}

/// Report the error-level findings a book-mutating edit introduced: a wild
/// book stays wild, and an edit adds no defect. Non-blocking, over a file the
/// caller has written.
#[cfg(feature = "validate")]
fn report_edit_regressions(before: &[u8], after: &[u8], what: &str) {
    let added = bokai::validate::source::added_errors(before, after);
    if added.is_empty() {
        return;
    }
    eprintln!("{what}: introduced {} new error finding(s):", added.len());
    for finding in &added {
        eprintln!(
            "  [{}] {}/{} @ {}: {}",
            finding.severity.as_str(),
            finding.check,
            finding.rule,
            finding.location,
            finding.message
        );
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "pdf"), allow(unused_variables))]
fn convert(
    input: &str,
    output: Option<&str>,
    from_format: Option<&str>,
    to_format: Option<&str>,
    quiet: bool,
    merge_mode: &str,
    ppd: Option<&str>,
    writing_mode: Option<&str>,
    validate: bool,
    max_workers: usize,
) -> Result<(), String> {
    let from_stdin = input == "-";

    // KFX → PDF (the return leg of the PDF↔KFX dual format): extract the
    // verbatim embedded PDF from a PDF-backed/container KFX. Dispatched here by
    // extension, before format parsing (PDF isn't in the `Format` enum).
    if !from_stdin
        && std::path::Path::new(input)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx"))
        && let Some(out) = output
        && out != "-"
        && std::path::Path::new(out)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
    {
        return convert_kfx_to_pdf(input, out, quiet);
    }

    let input_format = if let Some(fmt) = from_format {
        Some(parse_format(fmt)?)
    } else if from_stdin {
        return Err(
            "Input format required when reading from stdin. Use -f (epub|azw3|mobi|kfx)"
                .to_string(),
        );
    } else {
        Format::from_path(input)
    };

    if let Some(fmt) = input_format
        && !fmt.can_import()
    {
        return Err(format!("{:?} cannot be used as input format", fmt));
    }

    // Determine output format. A non-stdout target needs a real output path; a
    // recognized extension (.epub/.kfx/.txt/.md) selects the exporter. Use `-t`
    // to force the format (e.g. when writing Markdown to stdout via `-`).
    let output_format = if let Some(fmt) = to_format {
        parse_format(fmt)?
    } else if let Some(out) = output
        && out != "-"
    {
        Format::from_path(out).ok_or_else(|| {
            format!(
                "Unknown output format: {}. Supported: .epub, .kfx, .txt, .md",
                out
            )
        })?
    } else {
        return Err(
            "Output path required. Supported targets: .epub, .kfx, .txt, .md (use -t to override)"
                .to_string(),
        );
    };

    if !output_format.can_export() {
        return Err(format!(
            "{:?} cannot be used as output format. Supported: epub, kfx, md/txt",
            output_format
        ));
    }

    let to_stdout = output == Some("-");

    if !quiet && !to_stdout {
        let input_name = if from_stdin { "stdin" } else { input };
        eprintln!(
            "Converting {} -> {}",
            input_name,
            output.unwrap_or("stdout")
        );
    }

    // KFX -> EPUB, through the IR. Refuses a PDF-backed container.
    if !from_stdin
        && output_format == Format::Epub
        && std::path::Path::new(input)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx"))
    {
        let source = std::fs::File::open(input)
            .and_then(bokai::io::FileSource::new)
            .map_err(|e| format!("Failed to read input: {e}"))?;
        if bokai::formats::kfx::pdf_container::source_is_pdf_backed(&source) {
            return Err(
                "this KFX is a PDF-backed container; extract it with a .pdf output \
                 (e.g. `bokai convert in.kfx out.pdf`), not EPUB"
                    .to_string(),
            );
        }
        let mut book = Book::open_format(input, Format::Kfx)
            .map_err(|e| format!("Failed to open input: {e}"))?;
        book.set_max_workers(max_workers);
        write_export(book, Format::Epub, output, to_stdout, validate, quiet)?;
        if !quiet && !to_stdout {
            eprintln!("Done.");
        }
        return Ok(());
    }

    // Aozora Bunko `.zip` → `.epub`. A zip holding a `.txt` with `底本：` or
    // `［＃` markers takes the `aozora` pipeline: parse → cover → build_epub.
    #[cfg(feature = "aozora")]
    if !from_stdin
        && output_format == Format::Epub
        && std::path::Path::new(input)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
        && let Some(()) = aozora_dispatch(input, output, to_stdout, quiet, validate)?
    {
        return Ok(());
    }

    // Fast path: .kfx-zip -> .kfx merges fragments without the IR pipeline,
    // skipping storyline/section resolution. See `kfx::merge`.
    if !from_stdin
        && output_format == Format::Kfx
        && std::path::Path::new(input)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx-zip"))
    {
        let mode = match merge_mode {
            "fast" => bokai::formats::kfx::merge::MergeMode::Fast,
            "mechanical" | "" => bokai::formats::kfx::merge::MergeMode::Mechanical,
            other => {
                return Err(format!(
                    "--mode must be 'mechanical' or 'fast', got '{other}'"
                ));
            }
        };
        let bytes =
            bokai::formats::kfx::merge::merge_kfx_zip_with_mode(std::path::Path::new(input), mode)
                .map_err(|e| format!("Conversion failed: {e}"))?;
        if to_stdout {
            use std::io::Write;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|e| format!("Write failed: {e}"))?;
        } else {
            std::fs::write(output.unwrap(), &bytes)
                .map_err(|e| format!("Failed to write output: {e}"))?;
        }
        if !quiet && !to_stdout {
            eprintln!("Done.");
        }
        return Ok(());
    }

    // PDF → KFX: wrap the PDF verbatim into a fixed-layout PDOC KFX. The
    // device renders each embedded page.
    #[cfg(feature = "pdf")]
    if !from_stdin
        && output_format == Format::Kfx
        && std::path::Path::new(input)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
    {
        return convert_pdf_to_kfx(input, output, to_stdout, quiet, ppd);
    }

    let mut book = if from_stdin {
        use std::io::Read;
        let mut data = Vec::new();
        std::io::stdin()
            .read_to_end(&mut data)
            .map_err(|e| format!("Failed to read stdin: {e}"))?;
        Book::from_vec(data, input_format.unwrap())
            .map_err(|e| format!("Failed to parse input: {e}"))?
    } else {
        let fmt = input_format.or_else(|| Format::from_path(input));
        if let Some(fmt) = fmt {
            Book::open_format(input, fmt).map_err(|e| format!("Failed to open input: {e}"))?
        } else {
            Book::open(input).map_err(|e| format!("Failed to open input: {e}"))?
        }
    };
    book.set_max_workers(max_workers);

    // Force the writing mode via a metadata override (EPUB → KFX) — the same
    // `Book::set_metadata` hook an embedding caller uses to bake its own edited
    // metadata. A `-rl` mode also turns the page right-to-left.
    if let Some(wm) = writing_mode {
        let mut meta = book.metadata().clone();
        meta.primary_writing_mode = Some(wm.to_string());
        meta.page_progression_direction =
            Some(if wm.ends_with("-rl") { "rtl" } else { "ltr" }.to_string());
        book.set_metadata_override(meta);
    }

    let validate_epub = validate && output_format == Format::Epub;
    write_export(book, output_format, output, to_stdout, validate_epub, quiet)?;

    if !quiet && !to_stdout {
        eprintln!("Done.");
    }

    Ok(())
}

// ----------------------------------------------------------------------------
// Dump command
// ----------------------------------------------------------------------------

struct DumpOptions {
    json: bool,
    structure: bool,
    no_styles: bool,
    styles: bool,
    chapter: Option<u32>,
    styles_only: bool,
    depth: Option<usize>,
}

fn dump_ir(path: &str, opts: DumpOptions) -> Result<(), String> {
    let mut book = Book::open(path).map_err(|e| e.to_string())?;

    if opts.json {
        dump_ir_json(&mut book, path, &opts)
    } else {
        dump_ir_tree(&mut book, path, &opts)
    }
}

#[derive(Serialize)]
struct DumpInfo {
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    styles: Option<Vec<StyleInfo>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    chapters: Vec<ChapterDump>,
}

#[derive(Serialize)]
struct StyleInfo {
    id: u32,
    css: String,
}

#[derive(Serialize)]
struct ChapterDump {
    id: u32,
    path: String,
    node_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    tree: Option<NodeDump>,
}

#[derive(Serialize)]
struct NodeDump {
    id: u32,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    style_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    src: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<NodeDump>,
}

fn dump_ir_json(book: &mut Book, path: &str, opts: &DumpOptions) -> Result<(), String> {
    let mut info = DumpInfo {
        file: path.to_string(),
        styles: None,
        chapters: Vec::new(),
    };

    if opts.styles_only {
        let chapter_id = opts.chapter.unwrap_or(0);
        let chapter = book
            .load_chapter(ChapterId(chapter_id))
            .map_err(|e| e.to_string())?;
        info.styles = Some(collect_styles(&chapter));
        let json = serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?;
        println!("{json}");
        return Ok(());
    }

    let chapter_ids: Vec<(ChapterId, String)> = if let Some(id) = opts.chapter {
        let source = book.source_id(ChapterId(id)).unwrap_or("").to_string();
        vec![(ChapterId(id), source)]
    } else {
        book.spine()
            .iter()
            .map(|e| {
                let source = book.source_id(e.id).unwrap_or("").to_string();
                (e.id, source)
            })
            .collect()
    };

    for (id, source_path) in chapter_ids {
        let chapter = book.load_chapter(id).map_err(|e| e.to_string())?;

        let tree = if !opts.styles_only {
            Some(dump_node_json(&chapter, NodeId::ROOT, opts, 0))
        } else {
            None
        };

        info.chapters.push(ChapterDump {
            id: id.0,
            path: source_path,
            node_count: chapter.node_count(),
            tree,
        });
    }

    let json = serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

fn dump_node_json(chapter: &Chapter, id: NodeId, opts: &DumpOptions, depth: usize) -> NodeDump {
    let node = chapter.node(id).unwrap();

    let text = if !opts.structure && node.role == Role::Text && !node.text.is_empty() {
        // JSON output stays verbatim.
        let content = chapter.text(node.text);
        Some(clip_text(content, 100))
    } else {
        None
    };

    let style_id = if !opts.no_styles && node.style.0 != 0 {
        Some(node.style.0)
    } else {
        None
    };

    let children: Vec<NodeDump> = if opts.depth.is_none() || depth < opts.depth.unwrap() {
        chapter
            .children(id)
            .map(|child_id| dump_node_json(chapter, child_id, opts, depth + 1))
            .collect()
    } else {
        Vec::new()
    };

    NodeDump {
        id: id.0,
        role: role_to_string(node.role),
        text,
        style_id,
        href: chapter.semantics.href(id).map(String::from),
        src: chapter.semantics.src(id).map(String::from),
        alt: chapter.semantics.alt(id).map(String::from),
        anchor_id: chapter.semantics.id(id).map(String::from),
        children,
    }
}

fn dump_ir_tree(book: &mut Book, path: &str, opts: &DumpOptions) -> Result<(), String> {
    println!("File: {path}");
    println!();

    if opts.styles_only {
        let chapter_id = opts.chapter.unwrap_or(0);
        let chapter = book
            .load_chapter(ChapterId(chapter_id))
            .map_err(|e| e.to_string())?;
        println!("Style Pool ({} styles):", chapter.styles.len());
        for (id, style) in chapter.styles.iter() {
            let css = style.to_css_string();
            if css.is_empty() {
                println!("  [{}] (default)", id.0);
            } else {
                println!("  [{}] {}", id.0, css);
            }
        }
        return Ok(());
    }

    let chapter_ids: Vec<(ChapterId, String)> = if let Some(id) = opts.chapter {
        let source = book.source_id(ChapterId(id)).unwrap_or("").to_string();
        vec![(ChapterId(id), source)]
    } else {
        book.spine()
            .iter()
            .map(|e| {
                let source = book.source_id(e.id).unwrap_or("").to_string();
                (e.id, source)
            })
            .collect()
    };

    for (idx, (id, source_path)) in chapter_ids.iter().enumerate() {
        let chapter = book.load_chapter(*id).map_err(|e| e.to_string())?;

        if idx > 0 {
            println!();
        }
        println!(
            "Chapter {} [{}] ({} nodes)",
            id.0,
            source_path,
            chapter.node_count()
        );

        if !opts.no_styles {
            println!("  Styles: {} unique", chapter.styles.len());
        }

        println!();
        dump_node_tree(&chapter, NodeId::ROOT, opts, 0);
    }

    Ok(())
}

fn dump_node_tree(chapter: &Chapter, id: NodeId, opts: &DumpOptions, depth: usize) {
    if let Some(max_depth) = opts.depth
        && depth > max_depth
    {
        return;
    }

    let node = chapter.node(id).unwrap();
    let indent = "  ".repeat(depth);

    let mut line = format!("{}{}", indent, role_to_string(node.role));

    if !opts.no_styles && node.style.0 != 0 {
        line.push_str(&format!(" [s{}]", node.style.0));

        if opts.styles {
            if let Some(style) = chapter.styles.get(node.style) {
                let css = style.to_css_string();
                if !css.is_empty() {
                    line.push_str(&format!(" {{ {} }}", css.trim()));
                }
            }
        }
    }

    if let Some(href) = chapter.semantics.href(id) {
        line.push_str(&format!(" href=\"{}\"", truncate_text(href, 40)));
    }
    if let Some(src) = chapter.semantics.src(id) {
        line.push_str(&format!(" src=\"{}\"", truncate_text(src, 40)));
    }
    if let Some(alt) = chapter.semantics.alt(id) {
        line.push_str(&format!(" alt=\"{}\"", truncate_text(alt, 30)));
    }
    if let Some(anchor_id) = chapter.semantics.id(id) {
        line.push_str(&format!(" id=\"{}\"", anchor_id));
    }
    if let Some(class) = chapter.semantics.class(id) {
        line.push_str(&format!(" class=\"{}\"", class));
    }

    if !opts.structure && node.role == Role::Text && !node.text.is_empty() {
        let text = chapter.text(node.text);
        line.push_str(&format!(": \"{}\"", truncate_text(text, 60)));
    }

    println!("{line}");

    for child_id in chapter.children(id) {
        dump_node_tree(chapter, child_id, opts, depth + 1);
    }
}

fn collect_styles(chapter: &Chapter) -> Vec<StyleInfo> {
    chapter
        .styles
        .iter()
        .map(|(id, style)| StyleInfo {
            id: id.0,
            css: style.to_css_string(),
        })
        .collect()
}

fn role_to_string(role: Role) -> String {
    match role {
        Role::Text => "Text".to_string(),
        Role::Paragraph => "Paragraph".to_string(),
        Role::Heading(level) => format!("Heading({})", level),
        Role::Container => "Container".to_string(),
        Role::Image => "Image".to_string(),
        Role::Link => "Link".to_string(),
        Role::OrderedList => "OrderedList".to_string(),
        Role::UnorderedList => "UnorderedList".to_string(),
        Role::ListItem => "ListItem".to_string(),
        Role::Table => "Table".to_string(),
        Role::ColumnGroup => "ColumnGroup".to_string(),
        Role::Column => "Column".to_string(),
        Role::TableHead => "TableHead".to_string(),
        Role::TableBody => "TableBody".to_string(),
        Role::TableRow => "TableRow".to_string(),
        Role::TableCell => "TableCell".to_string(),
        Role::Sidebar => "Sidebar".to_string(),
        Role::Footnote => "Footnote".to_string(),
        Role::Figure => "Figure".to_string(),
        Role::Inline => "Inline".to_string(),
        Role::BlockQuote => "BlockQuote".to_string(),
        Role::Root => "Root".to_string(),
        Role::Break => "Break".to_string(),
        Role::Rule => "Rule".to_string(),
        Role::DefinitionList => "DefinitionList".to_string(),
        Role::DefinitionTerm => "DefinitionTerm".to_string(),
        Role::DefinitionDescription => "DefinitionDescription".to_string(),
        Role::CodeBlock => "CodeBlock".to_string(),
        Role::Caption => "Caption".to_string(),
        Role::Ruby => "Ruby".to_string(),
        Role::RubyText => "RubyText".to_string(),
    }
}

/// Clip a string to `max_chars` characters, appending `...` after a cut.
/// The count is characters, never bytes.
fn clip_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max_chars).collect();
    format!("{clipped}...")
}

/// Render a string for one line of tree output: control characters escaped,
/// everything else verbatim, clipped to `max_chars` source characters.
/// Whitespace is escaped, never collapsed.
fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for c in clip_text(text, max_chars).chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// =========================================================================
// PDF → KFX dispatch (called from `convert` when input is .pdf → .kfx)
// =========================================================================

#[cfg(feature = "pdf")]
fn convert_pdf_to_kfx(
    input: &str,
    output: Option<&str>,
    to_stdout: bool,
    quiet: bool,
    ppd: Option<&str>,
) -> Result<(), String> {
    // The page progression direction is normalized and validated up front:
    // `--ppd RTL` lowercases, `--ppd r2l` errors.
    let ppd = match ppd.map(|s| s.trim().to_ascii_lowercase()) {
        None => None,
        Some(s) if s == "rtl" || s == "ltr" => Some(s),
        Some(other) => {
            return Err(format!("--ppd must be 'rtl' or 'ltr', got '{other}'"));
        }
    };

    let bytes = std::fs::read(input).map_err(|e| format!("Failed to read input: {e}"))?;
    let doc = bokai::import::probe_pdf(bytes).map_err(|e| format!("PDF parse failed: {e}"))?;

    // Title: PDF /Info (title-cased if ALL CAPS, as Amazon does), else file
    // stem. Author: PDF /Info, if present.
    let title = doc
        .title
        .clone()
        .map(|t| title_case_if_shouting(&t))
        .unwrap_or_else(|| pdf_file_stem(input));
    let author = doc.author.clone();

    let meta = bokai::export::PdfKfxMeta {
        title: title.clone(),
        author: author.clone(),
        language: "en".to_string(),
        date: None,
        publisher: None,
        page_progression_direction: ppd.clone(),
    };

    if !quiet && !to_stdout {
        eprintln!(
            "PDF: {} pages → fixed-layout PDOC KFX\n  title:  {title}\n  author: {}\n  ppd:    {}",
            doc.pages.len(),
            author.as_deref().unwrap_or("(none)"),
            ppd.as_deref().unwrap_or("(default: ltr)"),
        );
    }

    // Render page 1 as the cover (PDOC library tile / sleep-screen art) via the
    // PDF engine (PDFKit). Optional — if it's unavailable, log and ship a
    // cover-less KFX.
    let cover = bokai::formats::pdf::render::render_pdf_page_jpeg(
        &doc.bytes,
        0,
        bokai::formats::pdf::render::COVER_TARGET_WIDTH_PX,
        bokai::formats::pdf::render::COVER_JPEG_QUALITY,
    );
    let cover_jpeg = match &cover {
        Ok(jpeg) => {
            if !quiet && !to_stdout {
                eprintln!("  cover:  page 1 rendered ({} bytes)", jpeg.len());
            }
            Some(jpeg.as_slice())
        }
        Err(e) => {
            if !quiet && !to_stdout {
                eprintln!("  cover:  skipped ({e})");
            }
            None
        }
    };

    // Extract the selectable text layer (PDFKit). Optional, like the cover: a
    // failure (or a non-macOS build) leaves a visual-only KFX.
    let text = bokai::formats::pdf::render::extract_pdf_text(&doc.bytes);
    let text_pages = match &text {
        Ok(pages) => {
            if !quiet && !to_stdout {
                let runs: usize = pages.iter().map(|p| p.runs.len()).sum();
                eprintln!("  text:   {runs} runs across {} pages", pages.len());
            }
            Some(pages.as_slice())
        }
        Err(e) => {
            if !quiet && !to_stdout {
                eprintln!("  text:   skipped ({e})");
            }
            None
        }
    };

    let kfx = bokai::export::pdf_to_kfx(&doc, &meta, cover_jpeg, text_pages)
        .map_err(|e| format!("write KFX: {e}"))?;

    if to_stdout {
        use std::io::Write;
        std::io::stdout()
            .write_all(&kfx)
            .map_err(|e| format!("Write failed: {e}"))?;
    } else {
        std::fs::write(output.unwrap(), &kfx)
            .map_err(|e| format!("Failed to write output: {e}"))?;
    }
    if !quiet && !to_stdout {
        eprintln!("Done ({} bytes).", kfx.len());
    }
    Ok(())
}

/// KFX → PDF: extract the verbatim embedded PDF from a PDF-backed container KFX.
fn convert_kfx_to_pdf(input: &str, output: &str, quiet: bool) -> Result<(), String> {
    let kfx = std::fs::read(input).map_err(|e| format!("Failed to read input: {e}"))?;
    let pdf = bokai::formats::kfx::pdf_container::kfx_extract_pdf(&kfx)
        .map_err(|e| format!("PDF extraction failed: {e}"))?;
    std::fs::write(output, &pdf).map_err(|e| format!("Failed to write output: {e}"))?;
    if !quiet {
        eprintln!("Extracted embedded PDF: {} bytes", pdf.len());
    }
    Ok(())
}

/// Title-case a string only if it is "shouting" (has letters and no lowercase),
/// matching what Amazon does to an ALL-CAPS `/Info` title.
#[cfg(feature = "pdf")]
fn title_case_if_shouting(s: &str) -> String {
    let has_lower = s.chars().any(|c| c.is_lowercase());
    let has_alpha = s.chars().any(|c| c.is_alphabetic());
    if has_lower || !has_alpha {
        return s.to_string();
    }
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(feature = "pdf")]
fn pdf_file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string()
}

// =========================================================================
// Aozora dispatch (called from `convert` when input is .zip → .epub)
// =========================================================================

#[cfg(feature = "aozora")]
fn aozora_dispatch(
    input: &str,
    output: Option<&str>,
    to_stdout: bool,
    quiet: bool,
    validate: bool,
) -> Result<Option<()>, String> {
    use std::io::Read;

    let file = std::fs::File::open(input).map_err(|e| format!("Failed to open input: {e}"))?;
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return Ok(None),
    };

    let mut txt_buf: Option<Vec<u8>> = None;
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("zip entry: {e}"))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("read zip: {e}"))?;
        if lower.ends_with(".txt") {
            txt_buf = Some(buf);
        } else if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
        {
            let basename = std::path::Path::new(&name)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(name);
            images.push((basename, buf));
        }
    }

    let Some(txt) = txt_buf else {
        return Ok(None);
    };
    let text = bokai::formats::aozora::parser_txt::decode_bytes(&txt);
    // Sniff: must look like an Aozora source (底本：/［＃ markers).
    if !text.contains("底本") && !text.contains("［＃") {
        return Ok(None);
    }
    let doc = bokai::formats::aozora::parse_txt(&text);
    let cover = bokai::formats::aozora::render_cover_jpeg(&doc.title, &doc.author)
        .map_err(|e| format!("cover render: {e}"))?;
    let epub_bytes = bokai::formats::aozora::build_epub(bokai::formats::aozora::EpubInput {
        document: &doc,
        images: &images,
        cover_jpeg: &cover,
    })
    .map_err(|e| format!("build epub: {e}"))?;

    if to_stdout {
        use std::io::Write;
        std::io::stdout()
            .write_all(&epub_bytes)
            .map_err(|e| format!("Write failed: {e}"))?;
    } else {
        std::fs::write(output.unwrap(), &epub_bytes)
            .map_err(|e| format!("Failed to write output: {e}"))?;
    }
    report_epub_validation(&epub_bytes, validate, quiet);
    if !quiet && !to_stdout {
        eprintln!("Done.");
    }
    Ok(Some(()))
}
