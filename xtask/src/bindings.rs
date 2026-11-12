//! `cargo xtask bindings [--check]`: regenerate or verify the checked-in
//! `ghostty-sys` bindings against the prepared, pinned headers.

use std::process::Command;

use crate::deps::{ghostty_prefix, host_triple};
use crate::manifest::Manifest;
use crate::util::repo_root;

pub fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: cargo xtask bindings [--check]".into()),
    };
    let manifest = Manifest::load()?;
    let prefix = ghostty_prefix(&manifest, host_triple()?)?;
    let include = prefix.join("include");
    if !include.join("ghostty").join("vt.h").is_file() {
        return Err(format!(
            "headers not found under {}; run `cargo xtask deps prepare` first",
            include.display()
        ));
    }
    let root = repo_root();
    let out = root.join("crates/ghostty-sys/src/bindings.rs");
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args([
        "run",
        "--quiet",
        "--locked",
        "--package",
        "ghostty-bindgen",
        "--",
    ])
    .arg("--include")
    .arg(&include)
    .arg("--out")
    .arg(&out)
    .current_dir(&root);
    if check {
        cmd.arg("--check");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("could not run ghostty-bindgen: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("ghostty-bindgen failed".into())
    }
}
