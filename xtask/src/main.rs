//! Repository tasks: `cargo xtask <command>`.

mod acceptance;
mod api;
mod bench;
mod bindings;
mod build_report;
mod color_schemes;
mod deps;
mod desktop;
mod doctor;
mod fresh;
mod gate;
mod headless;
mod hooks;
mod icons;
mod manifest;
mod package;
mod plugin;
mod policy;
mod preview;
mod private_git;
mod release;
mod render;
mod scenes;
mod util;
mod web;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  doctor                       check the local toolchain and native build prerequisites
  api [--check]                regenerate (or check) API artifacts from crates/api/schema
  bench headless|busy|extensions|memory|streams|tiers [OPTIONS] [--debug] [--linux]
                               the headless benchmark workloads, release build, results in target/bench
  bench remote [OPTIONS] [--container [--netem SPEC]]
                               key to echo through a connected host, split into link and host
  bench gui [OPTIONS]          the desktop measuring itself (needs an awake, unlocked display;
                               exits 5, not run, without one)
  bench report FILE...         result files as the Markdown docs/performance.md records
  build-report [--ci-run ID]   the release build report from target/package, the gate, and CI
  release vX.Y.Z [--sign ID] [--notarize PROFILE] [--resume]
                               build both lanes' artifacts, tag, and fill a draft GitHub release with the report
  bindings [--check]           regenerate (or check) the libghostty-vt Rust bindings
  deps prepare|path [--target <triple>]
                               fetch, verify, and build pinned native dependencies
  deps web                     fetch and verify the web engine's (CEF, cmake, Ninja; macOS)
  web bundle [--release] [--install]
                               the desktop with web panes and the Dreamer Web component beside it (macOS)
  web smoke [--packaged]       draw web panes in that bundle, or in target/package's app and component
  headless [--cargo-args]      list the packages of the headless build
  hooks install|status         use the versioned git hooks in .githooks/
  package --platform macos|arch|linux|deb|rpm|server|current [--debug] [--source-only] [--nodeps] [--target T]
          [--sign ID [--notarize PROFILE]]
                               build a package (macOS: zips of the app and of Dreamer Web, with signing records; ad hoc unless signed with Developer ID)
  package --platform linux --container [--arch arm64|amd64]
                               the Linux archives built in an ubuntu:24.04 container
  package --fresh [--arch arm64|amd64]
                               run the Linux archives on fresh ubuntu:24.04 containers
  package --fresh --platform macos
                               add Dreamer Web from its zip with the unpacked app's dream, away from the tree
  icons [--check]              draw (or check) the application icon files
  plugin [--check]             copy (or check) the shipped skills and version into plugins/dreamer
  policy                       check repository rules (workflow runners, pinned actions)
  schemes [--check]            generate the terminal color scheme table from the vendored files
  remote-acceptance            section 18.2 remote cases through a private, unprivileged sshd
  scenes [NAME]                open the design review's reference scenes in both skins, to capture
  screenshot [--scene NAME] [--skin dark|light] [--settings YAML]
                               capture Dreamer's own window in a reference scene (macOS; Linux under Hyprland)
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
        Some("bench") => bench::run(&args[1..]),
        Some("bindings") => bindings::run(&args[1..]),
        Some("deps") => deps::run(&args[1..]),
        Some("web") => web::run(&args[1..]),
        Some("headless") => headless::run(&args[1..]),
        Some("hooks") => hooks::run(&args[1..]),
        Some("gate") if args.get(1).map(String::as_str) == Some("linux-desktop") => {
            desktop::run(&args[2..])
        }
        Some("gate") => gate::run(&args[1..]),
        Some("icons") => icons::run(&args[1..]),
        Some("plugin") => plugin::run(&args[1..]),
        Some("policy") => policy::run(&args[1..]),
        Some("scenes") => scenes::run(&args[1..]),
        Some("screenshot") => scenes::screenshot(&args[1..]),
        Some("schemes") => color_schemes::run(&args[1..]),
        Some("remote-acceptance") => acceptance::run(&args[1..]),
        Some("package") => package::run(&args[1..]),
        Some("build-report") => build_report::run(&args[1..]),
        Some("release") => release::run(&args[1..]),
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
