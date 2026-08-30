//! `sidle-cli` — the library without the window.

mod cmd;
mod ctx;
mod progress;
mod select;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::ctx::Ctx;
use crate::select::Select;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sidle-cli: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[derive(Parser)]
#[command(
    name = "sidle-cli",
    about = "sidle's library, from a script",
    version,
    max_term_width = 100
)]
struct Cli {
    /// Work on the library under DIR, in place of the configured root.
    /// A copy of a library is a library: `--root` points a sweep at one.
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<std::path::PathBuf>,

    /// Report as JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What the library holds, and what needs doing.
    Status,
    /// List books.
    List {
        #[command(flatten)]
        select: Select,
        /// Print one column: `id`, `sha`, `title`, `path`,
        /// `kfx`, `epub`, `pdf`, `asin`. Ideal for feeding another command.
        #[arg(long, value_name = "FIELD")]
        field: Option<String>,
    },
    /// Everything stored about the selected books.
    Show {
        #[command(flatten)]
        select: Select,
    },
    /// Convert the selected books — the sweep after a bokai change.
    Convert(cmd::convert::ConvertArgs),
    /// Conversion job state, per book.
    Jobs {
        #[command(flatten)]
        select: Select,
    },
    /// Add files to the library, converting each as it lands.
    Import(cmd::library::ImportArgs),
    /// Edit metadata on the selected books.
    Set(Box<cmd::library::SetArgs>),
    /// Give a book its catalogue ASIN — the key colour covers are fetched with.
    Asin {
        #[command(flatten)]
        select: Select,
        /// The 10-character Amazon id.
        asin: String,
    },
    /// Record the format a book arrived in, for rows imported before the
    /// library kept it.
    SourceFormat {
        #[command(flatten)]
        select: Select,
        /// `azw3`, `mobi`, `epub`, `kfx`, `kfx-zip`, `pdf`, `aozora`.
        format: String,
    },
    /// Give every KFX an identity of its own, so a copy of a store-bought book
    /// stops sharing the catalogue item's ASIN.
    Rekey {
        /// Without this, the plan is printed and nothing is written.
        #[arg(long)]
        apply: bool,
    },
    /// Render romaji for a piece of text, the way the metadata editor does.
    Romanize {
        text: String,
        #[arg(long, default_value = "ja")]
        language: String,
    },
    /// Covers: re-fetch from the catalogue, or set one from a file.
    Cover(cmd::library::CoverArgs),
    /// Write books out as files someone else can read.
    Export(cmd::library::ExportArgs),
    /// Remove books from the library, files and all.
    Remove {
        #[command(flatten)]
        select: Select,
        /// Without this, the plan is printed and nothing is deleted.
        #[arg(long)]
        apply: bool,
    },
    /// Reclaim the disk space removed books freed (`VACUUM`).
    Compact,
    /// Split an omnibus into its volumes.
    Split(cmd::library::SplitArgs),
    /// Highlights, notes and bookmarks.
    Annotations(cmd::annotations::AnnotationsArgs),
    /// Judge — and rebuild — tables of contents.
    Toc(cmd::toc::TocArgs),
    /// The connected Kindle.
    #[command(subcommand)]
    Device(cmd::device::DeviceCmd),
    /// The apps that install to a Kindle.
    #[command(subcommand)]
    Apps(cmd::apps::AppsCmd),
    /// Handwritten notebooks.
    #[command(subcommand)]
    Notebook(cmd::notebook::NotebookCmd),
    /// Time read, as the Kindle's own logs recorded it.
    #[command(subcommand)]
    ReadingLog(cmd::reading_log::ReadingLogCmd),
    /// Files backed up off the device: screenshots, picker logs.
    #[command(subcommand)]
    Misc(cmd::misc::MiscCmd),
    /// The library as a whole: where it lives, backups, merges.
    #[command(subcommand)]
    Library(cmd::manage::LibraryCmd),
    /// The LAN server the Kindle pulls from.
    #[command(subcommand)]
    Server(cmd::server::ServerCmd),
}

fn run(cli: Cli) -> Result<()> {
    // The one command that runs without a library: it makes one.
    if matches!(cli.command, Command::Library(cmd::manage::LibraryCmd::Init)) {
        return cmd::manage::init(cli.root);
    }
    // `inspect` reads a directory on this machine and opens no library. A
    // checkout on a machine that has never run sidle is where it is called.
    if let Command::Apps(cmd::apps::AppsCmd::Inspect { path, files }) = &cli.command {
        return cmd::apps::inspect(cli.json, path, *files);
    }
    let ctx = Ctx::open(cli.root, cli.json)?;
    match cli.command {
        Command::Status => cmd::library::status(&ctx),
        Command::List { select, field } => cmd::library::list(&ctx, &select, field.as_deref()),
        Command::Show { select } => cmd::library::show(&ctx, &select),
        Command::Convert(args) => cmd::convert::run(&ctx, args),
        Command::Jobs { select } => cmd::convert::jobs(&ctx, &select),
        Command::Import(args) => cmd::library::import(&ctx, args),
        Command::Set(args) => cmd::library::set(&ctx, *args),
        Command::Asin { select, asin } => cmd::library::asin(&ctx, &select, &asin),
        Command::SourceFormat { select, format } => {
            cmd::library::source_format(&ctx, &select, &format)
        }
        Command::Rekey { apply } => cmd::library::rekey(&ctx, apply),
        Command::Romanize { text, language } => {
            println!(
                "{}",
                sidle_core::library::romaji::romanize_field(&text, None, &language)
            );
            Ok(())
        }
        Command::Cover(args) => cmd::library::cover(&ctx, args),
        Command::Export(args) => cmd::library::export(&ctx, args),
        Command::Remove { select, apply } => cmd::library::remove(&ctx, &select, apply),
        Command::Compact => cmd::library::compact(&ctx),
        Command::Split(args) => cmd::library::split(&ctx, args),
        Command::Annotations(args) => cmd::annotations::run(&ctx, args),
        Command::Toc(args) => cmd::toc::run(&ctx, args),
        Command::Device(sub) => cmd::device::run(&ctx, sub),
        Command::Apps(sub) => cmd::apps::run(&ctx, sub),
        Command::Notebook(sub) => cmd::notebook::run(&ctx, sub),
        Command::ReadingLog(sub) => cmd::reading_log::run(&ctx, sub),
        Command::Misc(sub) => cmd::misc::run(&ctx, sub),
        Command::Library(sub) => cmd::manage::run(&ctx, sub),
        Command::Server(sub) => cmd::server::run(&ctx, sub),
    }
}
