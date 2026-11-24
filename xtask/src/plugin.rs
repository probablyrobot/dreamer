//! `cargo xtask plugin [--check]`: keep the agent plugin in `plugins/dreamer`
//! in step with what `dream` ships.
//!
//! The plugin's skills are copies of the shared skills in
//! `crates/providers/pi-profile/skills` (the ones `dream skills install`
//! writes), because a plugin is installed as its own directory and cannot
//! point outside it. Its two manifests carry the workspace version. This
//! command writes both; with `--check` it fails if either has drifted.

use std::path::Path;

use crate::util::repo_root;

/// Skills that are about pi's own profile, not for other agents. Mirrors
/// `providers::pi_profile::PI_ONLY_SKILLS`.
const PI_ONLY: &[&str] = &["dreamer-pi-profile"];

const MANIFESTS: &[&str] = &[
    "plugins/dreamer/.claude-plugin/plugin.json",
    "plugins/dreamer/plugin.json",
];

pub fn run(args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [a] if a == "--check" => true,
        _ => return Err("usage: cargo xtask plugin [--check]".into()),
    };
    let root = repo_root();
    let mut stale = Vec::new();

    let source = root.join("crates/providers/pi-profile/skills");
    let dest = root.join("plugins/dreamer/skills");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&source).map_err(|e| format!("{}: {e}", source.display()))? {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        let name = name.to_string_lossy().into_owned();
        if !PI_ONLY.contains(&name.as_str()) {
            names.push(name);
        }
    }
    names.sort();
    for name in &names {
        let from = source.join(name).join("SKILL.md");
        let to = dest.join(name).join("SKILL.md");
        let text =
            std::fs::read_to_string(&from).map_err(|e| format!("{}: {e}", from.display()))?;
        sync(&to, &text, check, &mut stale)?;
    }
    // A skill removed from the shipped set must leave the plugin too.
    if let Ok(entries) = std::fs::read_dir(&dest) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !names.contains(&name) {
                if check {
                    stale.push(format!("plugins/dreamer/skills/{name} (no longer shipped)"));
                } else {
                    std::fs::remove_dir_all(entry.path()).map_err(|e| e.to_string())?;
                }
            }
        }
    }

    let version = crate::package::version(&root)?;
    for manifest in MANIFESTS {
        let path = root.join(manifest);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{manifest}: {e}"))?;
        let mut json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{manifest}: {e}"))?;
        if json["version"] != version.as_str() {
            json["version"] = version.clone().into();
            let mut out = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
            out.push('\n');
            sync(&path, &out, check, &mut stale)?;
        }
    }

    if stale.is_empty() {
        println!("plugin: {} skills, version {version}", names.len());
        return Ok(());
    }
    Err(format!(
        "the agent plugin is out of step with what dream ships:\n  {}\nrun `cargo xtask plugin`",
        stale.join("\n  ")
    ))
}

/// Make `path` hold `text`; under `check`, record it as stale instead.
fn sync(path: &Path, text: &str, check: bool, stale: &mut Vec<String>) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|have| have == text) {
        return Ok(());
    }
    if check {
        stale.push(path.display().to_string());
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}
