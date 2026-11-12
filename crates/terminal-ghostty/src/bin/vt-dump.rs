//! Headless harness: replay a VT byte stream into an engine and print the
//! resulting frame as deterministic text.
//!
//! usage: vt-dump [--cols N] [--rows N] [--styles] [--features] [FILE|-]

use std::io::Read as _;
use std::process::ExitCode;

use terminal_ghostty::core::GridSize;
use terminal_ghostty::{Engine, EngineConfig, features};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vt-dump: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut size = GridSize { cols: 80, rows: 24 };
    let mut styles = false;
    let mut input: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cols" => size.cols = number(args.next())?,
            "--rows" => size.rows = number(args.next())?,
            "--styles" => styles = true,
            "--features" => {
                let f = features();
                println!("{f:#?}");
                let missing = f.missing_required();
                if !missing.is_empty() {
                    return Err(format!("missing required features: {}", missing.join(", ")));
                }
                return Ok(());
            }
            other if input.is_none() => input = Some(other.to_owned()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let bytes = match input.as_deref() {
        None | Some("-") => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            buf
        }
        Some(path) => std::fs::read(path).map_err(|e| format!("{path}: {e}"))?,
    };

    let mut engine = Engine::new(EngineConfig::new(size)).map_err(|e| e.to_string())?;
    let fx = engine.feed(&bytes).map_err(|e| e.to_string())?;
    let frame = engine.frame().map_err(|e| e.to_string())?;
    print!(
        "{}",
        if styles {
            frame.style_dump()
        } else {
            frame.text_dump()
        }
    );
    if !fx.is_empty() {
        println!("effects {fx:?}");
    }
    Ok(())
}

fn number(v: Option<String>) -> Result<u16, String> {
    v.ok_or("missing number")?
        .parse()
        .map_err(|e| format!("bad number: {e}"))
}
