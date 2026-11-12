//! Repository tasks: `cargo xtask <command>`.

mod doctor;
mod util;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  doctor    check the local toolchain and native build prerequisites
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("doctor") => doctor::run(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}
