//! boko - Fast ebook converter

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;

use boko::{Book, Chapter, ChapterId, Format, NodeId, Role, ToCss, TocEntry, extract_section_tree};

#[derive(Parser)]
#[command(name = "boko")]
#[command(version, about = "Fast ebook converter", long_about = None)]
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

        /// Input format (epub, azw3, mobi, txt). Required when reading from stdin.
        #[arg(short = 'f', long = "from")]
        from_format: Option<String>,

        /// Output format (md, txt, epub, azw3). Inferred from output extension if not specified.
        #[arg(short = 't', long = "to")]
        to_format: Option<String>,

        /// Suppress output messages
        #[arg(short, long)]
        quiet: bool,

        /// `.kfx-zip` → `.kfx` merge strategy. `fast` (default) passes
        /// entity bodies through verbatim — fast and calibre-accepted.
        /// `mechanical` is a faithful port of calibre's pipeline, kept as
        /// the correctness reference.
        #[arg(long = "mode", default_value = "fast")]
        merge_mode: String,
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
    Validate {
        /// Which conversion direction to interpret. `epub-to-kfx` (default)
        /// treats the EPUB as ground truth; `kfx-to-epub` treats the KFX as
        /// ground truth.
        #[arg(long = "direction", default_value = "epub-to-kfx", global = true)]
        direction: String,

        #[command(subcommand)]
        check: ValidateCheck,
    },
}

fn parse_direction(s: &str) -> Result<boko::validate::Direction, String> {
    match s {
        "epub-to-kfx" | "epub2kfx" | "e2k" => Ok(boko::validate::Direction::EpubToKfx),
        "kfx-to-epub" | "kfx2epub" | "k2e" => Ok(boko::validate::Direction::KfxToEpub),
        other => Err(format!(
            "--direction must be 'epub-to-kfx' or 'kfx-to-epub', got '{other}'"
        )),
    }
}

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

    /// Report which CSS properties used by the EPUB boko-kai's parser drops
    Style {
        epub: String,
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
        } => convert(
            &input,
            output.as_deref(),
            from_format.as_deref(),
            to_format.as_deref(),
            quiet,
            &merge_mode,
        ),
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
        Command::Validate { direction, check } => match parse_direction(&direction) {
            Err(e) => Err(e),
            Ok(dir) => match check {
                ValidateCheck::Ruby { epub, kfx, details } => {
                    validate_ruby(&epub, &kfx, details, dir)
                }
                ValidateCheck::Text { epub, kfx, details } => {
                    validate_text(&epub, &kfx, details, dir)
                }
                ValidateCheck::Style { epub, details } => validate_style(&epub, details),
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
                ValidateCheck::All { epub, kfx, details } => {
                    validate_all(&epub, &kfx, details, dir)
                }
            },
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// JSON output structures
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
    #[serde(skip_serializing_if = "Option::is_none")]
    author_sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    collection: Option<CollectionInfoJson>,
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

fn validate_ruby(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::ruby::validate(&epub_bytes, &kfx_bytes)?;
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

fn validate_text(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::text::validate(&epub_bytes, &kfx_bytes)?;
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

fn validate_style(epub_path: &str, details: usize) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let report = boko::validate::style::validate(&epub_bytes)?;
    report.print_summary();
    if details > 0 {
        report.print_details(details);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!("{} CSS declarations dropped", report.dropped))
    }
}

fn validate_tags(epub_path: &str, details: usize) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let report = boko::validate::tags::validate(&epub_bytes)?;
    report.print_summary();
    if details > 0 {
        report.print_details(details);
    }
    if report.is_clean() {
        Ok(())
    } else {
        let fallback = report
            .by_bucket
            .get(&boko::validate::tags::Bucket::Fallback)
            .copied()
            .unwrap_or(0);
        Err(format!("{} elements with no role_map entry", fallback))
    }
}

fn validate_links(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::links::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
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

fn validate_images(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::images::validate(&epub_bytes, &kfx_bytes)?;
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

fn validate_nav(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::nav::validate(&epub_bytes, &kfx_bytes)?;
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

fn validate_metadata(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::metadata::validate(&epub_bytes, &kfx_bytes)?;
    report.print_summary(dir);
    if details > 0 {
        report.print_details(details, dir);
    }
    if report.is_clean() {
        Ok(())
    } else {
        Err(format!("{} metadata field(s) mismatched", report.diffs.len()))
    }
}

fn validate_writing_mode(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    let report = boko::validate::writing_mode::validate(&epub_bytes, &kfx_bytes)?;
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

fn validate_all(
    epub_path: &str,
    kfx_path: &str,
    details: usize,
    dir: boko::validate::Direction,
) -> Result<(), String> {
    let epub_bytes = std::fs::read(epub_path).map_err(|e| format!("{}: {}", epub_path, e))?;
    let kfx_bytes = std::fs::read(kfx_path).map_err(|e| format!("{}: {}", kfx_path, e))?;

    println!("=== Direction: {} → {} ===", dir.source_label(), dir.target_label());
    let mut all_clean = true;

    println!("=== Ruby ===");
    let ruby = boko::validate::ruby::validate(&epub_bytes, &kfx_bytes)?;
    ruby.print_summary(dir);
    if details > 0 {
        ruby.print_details(details, dir);
    }
    if !ruby.is_clean() {
        all_clean = false;
    }

    println!("\n=== Text ===");
    let text = boko::validate::text::validate(&epub_bytes, &kfx_bytes)?;
    text.print_summary(dir);
    if details > 0 {
        text.print_details(details, dir);
    }
    if !text.is_clean_for(dir) {
        all_clean = false;
    }

    println!("\n=== Style ===");
    let style = boko::validate::style::validate(&epub_bytes)?;
    style.print_summary();
    if details > 0 {
        style.print_details(details);
    }
    if !style.is_clean() {
        all_clean = false;
    }

    println!("\n=== Tags ===");
    let tags = boko::validate::tags::validate(&epub_bytes)?;
    tags.print_summary();
    if details > 0 {
        tags.print_details(details);
    }
    if !tags.is_clean() {
        all_clean = false;
    }

    println!("\n=== Links ===");
    let links = boko::validate::links::validate(&epub_bytes, &kfx_bytes)?;
    links.print_summary(dir);
    if details > 0 {
        links.print_details(details, dir);
    }
    if !links.is_clean() {
        all_clean = false;
    }

    println!("\n=== Images ===");
    let images = boko::validate::images::validate(&epub_bytes, &kfx_bytes)?;
    images.print_summary(dir);
    if details > 0 {
        images.print_details(details);
    }
    if !images.is_clean() {
        all_clean = false;
    }

    println!("\n=== Nav ===");
    let nav = boko::validate::nav::validate(&epub_bytes, &kfx_bytes)?;
    nav.print_summary(dir);
    if details > 0 {
        nav.print_details(details, dir);
    }
    if !nav.is_clean() {
        all_clean = false;
    }

    println!("\n=== Metadata ===");
    let metadata = boko::validate::metadata::validate(&epub_bytes, &kfx_bytes)?;
    metadata.print_summary(dir);
    if details > 0 {
        metadata.print_details(details, dir);
    }
    if !metadata.is_clean() {
        all_clean = false;
    }

    println!("\n=== Writing mode ===");
    let wm = boko::validate::writing_mode::validate(&epub_bytes, &kfx_bytes)?;
    wm.print_summary(dir);
    if !wm.is_clean() {
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
    // External URLs: denominator is the source side's external count. KFX
    // doesn't track external URL counts separately; use EPUB count as a proxy
    // in both directions (the assumption is that EPUB anchors mirror KFX uri
    // anchors 1:1 when preserved).
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
            author_sort: meta.author_sort.clone(),
            collection: meta.collection.as_ref().map(|c| CollectionInfoJson {
                name: c.name.clone(),
                collection_type: c.collection_type.clone(),
                position: c.position,
            }),
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
    if let Some(ref publisher) = meta.publisher {
        println!("Publisher: {publisher}");
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
        if desc.len() > 200 {
            println!("Description: {}...", &desc[..200]);
        } else {
            println!("Description: {desc}");
        }
    }
    if let Some(ref modified) = meta.modified_date {
        println!("Modified: {modified}");
    }
    if let Some(ref title_sort) = meta.title_sort {
        println!("Title Sort: {title_sort}");
    }
    if let Some(ref author_sort) = meta.author_sort {
        println!("Author Sort: {author_sort}");
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

    // Spine (chapters)
    println!("\nSpine ({} chapters):", book.spine().len());
    for entry in book.spine() {
        let source = book.source_id(entry.id).unwrap_or("?");
        println!(
            "  [{}] {} ({} bytes)",
            entry.id.0, source, entry.size_estimate
        );
    }

    // Table of contents
    println!("\nTable of Contents ({} entries):", book.toc().len());
    print_toc_human(book.toc(), 1);

    // Landmarks
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

    // Assets
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

fn convert(
    input: &str,
    output: Option<&str>,
    from_format: Option<&str>,
    to_format: Option<&str>,
    quiet: bool,
    merge_mode: &str,
) -> Result<(), String> {
    // Check if reading from stdin
    let from_stdin = input == "-";

    // Determine input format
    let input_format = if let Some(fmt) = from_format {
        Some(parse_format(fmt)?)
    } else if from_stdin {
        // Default to EPUB for stdin since that's most common
        return Err(
            "Input format required when reading from stdin. Use -f (epub|azw3|mobi)".to_string(),
        );
    } else {
        Format::from_path(input)
    };

    // Validate input format
    if let Some(fmt) = input_format
        && !fmt.can_import()
    {
        return Err(format!("{:?} cannot be used as input format", fmt));
    }

    // Determine output format
    let output_format = if let Some(fmt) = to_format {
        parse_format(fmt)?
    } else if let Some(out) = output {
        if out == "-" {
            // Explicit stdout, default to markdown
            Format::Markdown
        } else {
            Format::from_path(out).ok_or_else(|| {
                format!(
                    "Unknown output format: {}. Supported: .epub, .azw3, .txt, .md",
                    out
                )
            })?
        }
    } else {
        // No output specified, default to markdown on stdout
        Format::Markdown
    };

    if output_format == Format::Mobi {
        return Err("MOBI output is not supported; use .azw3 instead".to_string());
    }

    // Check if writing to stdout
    let to_stdout = output.is_none() || output == Some("-");

    if !quiet && !to_stdout {
        let input_name = if from_stdin { "stdin" } else { input };
        eprintln!(
            "Converting {} -> {}",
            input_name,
            output.unwrap_or("stdout")
        );
    }

    // Fast path: .kfx-zip -> .kfx merges fragments without touching the IR
    // pipeline. This avoids storyline/section resolution (and the
    // `document_regions` blocker) entirely. See `kfx::merge` for the design.
    if !from_stdin
        && output_format == Format::Kfx
        && std::path::Path::new(input)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx-zip"))
    {
        let mode = match merge_mode {
            "fast" => boko::kfx::merge::MergeMode::Fast,
            "mechanical" | "" => boko::kfx::merge::MergeMode::Mechanical,
            other => return Err(format!("--mode must be 'mechanical' or 'fast', got '{other}'")),
        };
        let bytes = boko::kfx::merge::merge_kfx_zip_with_mode(
            std::path::Path::new(input),
            mode,
        )
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

    // Open the book (from file or stdin)
    let mut book = if from_stdin {
        use std::io::Read;
        let mut data = Vec::new();
        std::io::stdin()
            .read_to_end(&mut data)
            .map_err(|e| format!("Failed to read stdin: {e}"))?;
        Book::from_bytes(&data, input_format.unwrap())
            .map_err(|e| format!("Failed to parse input: {e}"))?
    } else {
        let fmt = input_format.or_else(|| Format::from_path(input));
        if let Some(fmt) = fmt {
            Book::open_format(input, fmt).map_err(|e| format!("Failed to open input: {e}"))?
        } else {
            Book::open(input).map_err(|e| format!("Failed to open input: {e}"))?
        }
    };

    if to_stdout {
        // Write to stdout
        let mut stdout = std::io::stdout();
        let mut cursor = std::io::Cursor::new(Vec::new());
        book.export(output_format, &mut cursor)
            .map_err(|e| format!("Conversion failed: {e}"))?;
        use std::io::Write;
        stdout
            .write_all(cursor.get_ref())
            .map_err(|e| format!("Write failed: {e}"))?;
    } else {
        let output_path = output.unwrap();
        let mut file = std::fs::File::create(output_path)
            .map_err(|e| format!("Failed to create output: {e}"))?;
        book.export(output_format, &mut file)
            .map_err(|e| format!("Conversion failed: {e}"))?;
    }

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

// JSON output structures for dump command
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

    // If styles_only, just dump the style pool from the first chapter
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

    // Collect chapters to dump
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
        let content = chapter.text(node.text);
        Some(truncate_text(content, 100))
    } else {
        None
    };

    let style_id = if !opts.no_styles && node.style.0 != 0 {
        Some(node.style.0)
    } else {
        None
    };

    // Collect children
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

    // If styles_only, just dump the style pool
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

    // Collect chapters to dump
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
    // Check depth limit
    if let Some(max_depth) = opts.depth
        && depth > max_depth
    {
        return;
    }

    let node = chapter.node(id).unwrap();
    let indent = "  ".repeat(depth);

    // Build the node display line
    let mut line = format!("{}{}", indent, role_to_string(node.role));

    // Add style if not hidden and not default
    if !opts.no_styles && node.style.0 != 0 {
        // Always show style ID
        line.push_str(&format!(" [s{}]", node.style.0));

        if opts.styles {
            // Also expand styles to show CSS properties
            if let Some(style) = chapter.styles.get(node.style) {
                let css = style.to_css_string();
                if !css.is_empty() {
                    line.push_str(&format!(" {{ {} }}", css.trim()));
                }
            }
        }
    }

    // Add semantic attributes
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

    // Add text content for text nodes
    if !opts.structure && node.role == Role::Text && !node.text.is_empty() {
        let text = chapter.text(node.text);
        line.push_str(&format!(": \"{}\"", truncate_text(text, 60)));
    }

    println!("{line}");

    // Recurse into children
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

fn truncate_text(text: &str, max_chars: usize) -> String {
    // Normalize whitespace
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");

    // Count characters (not bytes) to handle multi-byte UTF-8 correctly
    let char_count = normalized.chars().count();
    if char_count <= max_chars {
        normalized
    } else {
        let truncated: String = normalized.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}
