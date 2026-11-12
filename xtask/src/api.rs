//! `cargo xtask api [--check]`: regenerate (or verify) everything derived
//! from `crates/api/schema/api.yaml` and enforce its compatibility lock.

use std::process::Command;

use crate::util::repo_root;

pub fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: cargo xtask api [--check]".into()),
    };
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(["run", "--quiet", "--locked", "--package", "api-gen", "--"])
        .current_dir(repo_root());
    if check {
        cmd.arg("--check");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("cargo run api-gen: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("API artifacts are stale or the schema change is incompatible (see above)".into())
    }
}
