//! Regenerate (or `--check`) every artifact derived from the API schema
//! (and, for TypeScript, the `ui` builders in `ui.yaml`),
//! and enforce the compatibility lock. Run through `cargo xtask api`.

mod compat;
mod other;
mod rust;
mod schema;
mod ts;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SCHEMA: &str = "crates/api/schema/api.yaml";
const LOCK: &str = "crates/api/schema/compat.lock";
const UI: &str = "crates/api/schema/ui.yaml";

fn main() -> ExitCode {
    let check = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--check") => true,
        Some(_) => {
            eprintln!("usage: api-gen [--check]");
            return ExitCode::from(2);
        }
    };
    match run(check) {
        Ok(msg) => {
            println!("api: {msg}");
            ExitCode::SUCCESS
        }
        Err(problems) => {
            for p in problems {
                eprintln!("api: {p}");
            }
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Format generated Rust the way `cargo fmt` would, so the two agree.
fn rustfmt(code: &str) -> Result<String, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("rustfmt: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("rustfmt: no stdin")?
        .write_all(code.as_bytes())
        .map_err(|e| format!("rustfmt: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("rustfmt: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rustfmt rejected the generated code: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("rustfmt: {e}"))
}

fn run(check: bool) -> Result<String, Vec<String>> {
    let root = root();
    let text =
        std::fs::read_to_string(root.join(SCHEMA)).map_err(|e| vec![format!("{SCHEMA}: {e}")])?;
    let schema: schema::Schema =
        serde_norway::from_str(&text).map_err(|e| vec![format!("{SCHEMA}: {e}")])?;
    let ui_text = std::fs::read_to_string(root.join(UI)).map_err(|e| vec![format!("{UI}: {e}")])?;
    let ui: ts::Ui = serde_norway::from_str(&ui_text).map_err(|e| vec![format!("{UI}: {e}")])?;
    let mut problems = schema.validate();
    problems.extend(ui.validate(&schema));
    if !problems.is_empty() {
        return Err(problems);
    }

    let contract = compat::Contract::of(&schema);
    let lock_path = root.join(LOCK);
    let lock_text = std::fs::read_to_string(&lock_path).ok();
    if let Some(old) = &lock_text {
        let old = compat::Contract::parse(old).map_err(|e| vec![e])?;
        let problems = compat::check(&old, &contract);
        if !problems.is_empty() {
            return Err(problems);
        }
    }

    let outputs: [(&str, String); 7] = [
        (
            "crates/api/src/generated.rs",
            rustfmt(&rust::generate(&schema)).map_err(|e| vec![e])?,
        ),
        ("crates/api/generated/openrpc.json", other::openrpc(&schema)),
        ("crates/api/generated/dreamer.proto", other::proto(&schema)),
        ("crates/api/generated/dreamer.luau", other::luau(&schema)),
        ("crates/api/generated/dreamer.d.ts", ts::dts(&schema, &ui)),
        (
            "crates/api/generated/app-types.json",
            ts::data(&schema, &ui),
        ),
        (LOCK, contract.render()),
    ];
    let mut stale = Vec::new();
    for (path, content) in &outputs {
        let full = root.join(path);
        if std::fs::read_to_string(&full).ok().as_deref() == Some(content.as_str()) {
            continue;
        }
        if check {
            stale.push(format!("{path} is out of date; run `cargo xtask api`"));
        } else {
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir).map_err(|e| vec![e.to_string()])?;
            }
            std::fs::write(&full, content).map_err(|e| vec![format!("{path}: {e}")])?;
            stale.push((*path).to_owned());
        }
    }
    let v = schema.version;
    match (check, stale.is_empty()) {
        (true, true) => Ok(format!("{}.{} up to date and compatible", v.major, v.minor)),
        (true, false) => Err(stale),
        (false, true) => Ok(format!("{}.{} unchanged", v.major, v.minor)),
        (false, false) => Ok(format!(
            "{}.{} wrote {}",
            v.major,
            v.minor,
            stale.join(", ")
        )),
    }
}
