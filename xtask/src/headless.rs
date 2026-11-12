//! `cargo xtask headless [--cargo-args]`: the packages that make up the
//! headless build (server, CLI, pane-app runtime, and their libraries).
//!
//! These must build without GPUI, a display server, or font libraries. The
//! list is the single source for CI, the gate, and the boundary check.

/// Packages built and tested for every headless target, including ARM64
/// Linux. Add each new UI-free crate here.
pub const PACKAGES: &[&str] = &[
    "dreamer-server",
    "dreamer-cli",
    "dreamer-termapp",
    "ghostty-sys",
    "tmux-protocol",
];

pub fn run(args: &[String]) -> Result<(), String> {
    match args {
        [] => {
            for p in PACKAGES {
                println!("{p}");
            }
            Ok(())
        }
        [flag] if flag == "--cargo-args" => {
            let parts: Vec<String> = PACKAGES.iter().map(|p| format!("-p {p}")).collect();
            println!("{}", parts.join(" "));
            Ok(())
        }
        _ => Err("usage: cargo xtask headless [--cargo-args]".into()),
    }
}
