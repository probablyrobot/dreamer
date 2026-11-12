//! Repository tasks: `cargo xtask <command>`.

mod api;
mod bindings;
mod deps;
mod desktop;
mod doctor;
mod gate;
mod headless;
mod hooks;
mod manifest;
mod package;
mod policy;
mod render;
mod util;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  doctor                       check the local toolchain and native build prerequisites
  api [--check]                regenerate (or check) API artifacts from crates/api/schema
  bindings [--check]           regenerate (or check) the libghostty-vt Rust bindings
  deps prepare|path [--target <triple>]
                               fetch, verify, and build pinned native dependencies
  headless [--cargo-args]      list the packages of the headless build
  hooks install|status         use the versioned git hooks in .githooks/
  package --platform macos|arch|current [--debug] [--source-only]
                               build, ad hoc sign, and zip the macOS app bundle
  policy                       check repository rules (workflow runners, pinned actions)
  gate linux-desktop [--allow-missing wayland|x11]
                               real-display smoke on a Linux desktop (second machine)
  gate [mac|linux] [--staged|--full]
                               run the local verification gate (pre-commit: --staged)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("doctor") => doctor::run(&args[1..]),
        Some("api") => api::run(&args[1..]),
        Some("bindings") => bindings::run(&args[1..]),
        Some("deps") => deps::run(&args[1..]),
        Some("headless") => headless::run(&args[1..]),
        Some("hooks") => hooks::run(&args[1..]),
        Some("gate") if args.get(1).map(String::as_str) == Some("linux-desktop") => {
            desktop::run(&args[2..])
        }
        Some("gate") => gate::run(&args[1..]),
        Some("policy") => policy::run(&args[1..]),
        Some("package") => package::run(&args[1..]),
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
