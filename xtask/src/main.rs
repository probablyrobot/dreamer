//! Repository tasks: `cargo xtask <command>`.

mod bindings;
mod deps;
mod doctor;
mod gate;
mod hooks;
mod manifest;
mod policy;
mod util;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  doctor                       check the local toolchain and native build prerequisites
  bindings [--check]           regenerate (or check) the libghostty-vt Rust bindings
  deps prepare|path [--target <triple>]
                               fetch, verify, and build pinned native dependencies
  hooks install|status         use the versioned git hooks in .githooks/
  policy                       check repository rules (workflow runners, pinned actions)
  gate [mac|linux] [--staged|--full]
                               run the local verification gate (pre-commit: --staged)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("doctor") => doctor::run(&args[1..]),
        Some("bindings") => bindings::run(&args[1..]),
        Some("deps") => deps::run(&args[1..]),
        Some("hooks") => hooks::run(&args[1..]),
        Some("gate") => gate::run(&args[1..]),
        Some("policy") => policy::run(&args[1..]),
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
