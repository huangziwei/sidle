//! Writes a request script for a book the renderer holds.
//!
//! Usage: `sidle-render-script <profile> <uri> <pages> [setting=value]...`
//!
//! Settings are `font=<index>`, `margins=narrow|normal|wide`,
//! `spacing=narrow|normal|wide`, `bold=<index>`, `columns=<1|2>`,
//! `vertical=<0|1>` and `language=<tag>`; anything unnamed stays at
//! [`Settings::default_for`].

use std::error::Error;
use std::path::PathBuf;

use sidle_render::probe::Probe;
use sidle_render::settings::{Panel, Settings, Stop};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(profile), Some(uri), Some(pages)) = (
        args.next().map(PathBuf::from),
        args.next(),
        args.next().and_then(|n| n.parse::<usize>().ok()),
    ) else {
        eprintln!("usage: sidle-render-script <profile> <uri> <pages> [setting=value]...");
        std::process::exit(2);
    };

    let panel = Panel::read(&profile)?;
    let mut settings = Settings::default_for(&panel);
    let mut language = String::from("en");
    let mut vertical = false;

    for argument in args {
        let Some((key, value)) = argument.split_once('=') else {
            return Err(format!("not a setting: {argument}").into());
        };
        match key {
            "font" => settings.font_size = value.parse()?,
            "bold" => settings.boldness = value.parse()?,
            "columns" => settings.columns = value.parse()?,
            "margins" => settings.margins = stop(value)?,
            "spacing" => settings.line_spacing = stop(value)?,
            "vertical" => vertical = value != "0",
            "language" => language = value.to_string(),
            _ => return Err(format!("no such setting: {key}").into()),
        }
    }

    let mut probe = Probe::new("held", &[]).in_language(language);
    if vertical {
        probe = probe.vertical();
    }
    print!("{}", probe.script(&uri, &panel, &settings, pages));
    Ok(())
}

fn stop(value: &str) -> Result<Stop, String> {
    match value {
        "narrow" => Ok(Stop::Narrow),
        "normal" => Ok(Stop::Normal),
        "wide" => Ok(Stop::Wide),
        other => Err(format!("no such stop: {other}")),
    }
}
