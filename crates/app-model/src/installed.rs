//! Installed app packages: `<config dir>/apps/<name>/app.toml`.
//! Their contributions are read from here. A package that does not
//! validate, or whose id another package already has, is refused with the
//! reason; a keybinding that is taken is dropped from its command, with the
//! reason. The rest still load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::manifest::Manifest;

#[derive(Debug, Clone)]
pub struct Package {
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Default)]
pub struct Installed {
    pub packages: Vec<Package>,
    /// What was refused, and why.
    pub problems: Vec<String>,
}

/// `<config dir>/apps`.
pub fn apps_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("apps")
}

/// The manifests under `dir`, by directory name, with when each changed:
/// cheap enough to poll for changes.
pub fn stamps(dir: &Path) -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path().join("app.toml"))
        .filter(|p| p.is_file())
        .map(|p| {
            let t = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            (p, t)
        })
        .collect();
    out.sort();
    out
}

/// A problem on one line: a parser's multi-line report becomes its first
/// line (where) and its last (what).
fn one_line(e: &str) -> String {
    let lines: Vec<&str> = e.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match lines.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [first, .., last] => format!("{first}: {last}"),
    }
}

/// A keybinding with its modifiers in one order (`alt-cmd-r` and
/// `cmd-alt-r` are the same binding).
pub fn normal_binding(k: &str) -> String {
    let mut parts: Vec<&str> = k.split('-').collect();
    let key = parts.pop().unwrap_or_default();
    parts.sort_unstable();
    parts.push(key);
    parts.join("-")
}

/// Read every package under `dir`. `taken` are keybindings the host keeps
/// for itself.
pub fn scan(dir: &Path, taken: &[&str]) -> Installed {
    let mut out = Installed::default();
    let mut ids: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut keys: BTreeMap<String, String> = taken
        .iter()
        .map(|k| (normal_binding(k), "the desktop".to_owned()))
        .collect();
    for (path, _) in stamps(dir) {
        let shown = path.display().to_string();
        let mut manifest = match crate::run::load(&shown) {
            Ok(m) => m,
            Err(e) => {
                out.problems.push(one_line(&e));
                continue;
            }
        };
        if let Some(first) = ids.get(&manifest.id) {
            out.problems.push(format!(
                "{shown}: package `{}` is already installed from {}",
                manifest.id,
                first.display()
            ));
            continue;
        }
        ids.insert(manifest.id.clone(), path.clone());
        let id = manifest.id.clone();
        for c in &mut manifest.contributes.commands {
            let Some(k) = c.key.clone() else { continue };
            let name = format!("{id}.{}", c.id);
            if let Some(owner) = keys.get(&normal_binding(&k)) {
                out.problems.push(format!(
                    "{name}: `{k}` is {owner}'s; the command stays in the palette without it"
                ));
                c.key = None;
            } else {
                keys.insert(normal_binding(&k), name);
            }
        }
        out.packages.push(Package {
            manifest_path: path,
            manifest,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(id: &str, key: &str) -> String {
        format!(
            r#"schema_version = 1
id = "{id}"
version = "1"
title = "T"
[data.s]
method = "tmux.topology"
path = "windows"
[view]
type = "list"
source = "s"
[[contributes.commands]]
id = "open"
title = "Open {id}"
opens = "panel"
key = "{key}"
"#
        )
    }

    #[test]
    fn clashes_are_refused_with_a_reason_and_the_rest_load() {
        let dir = std::env::temp_dir().join(format!("app-model-installed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let put = |name: &str, text: &str| {
            std::fs::create_dir_all(dir.join(name)).expect("dir");
            std::fs::write(dir.join(name).join("app.toml"), text).expect("write");
        };
        put("a", &package("one.app", "cmd-alt-o"));
        // The same id again: refused.
        put("b", &package("one.app", "cmd-alt-p"));
        // Its key is the first package's: loaded without the key.
        put("c", &package("two.app", "cmd-alt-o"));
        // The desktop's own key, with its modifiers in another order.
        put("d", &package("three.app", "shift-cmd-p"));
        put("e", "schema_version = 1\n");
        std::fs::create_dir_all(dir.join("not-a-package")).expect("dir");

        let found = scan(&dir, &["cmd-shift-p"]);
        let _ = std::fs::remove_dir_all(&dir);
        let ids: Vec<&str> = found
            .packages
            .iter()
            .map(|p| p.manifest.id.as_str())
            .collect();
        assert_eq!(ids, ["one.app", "two.app", "three.app"]);
        let key = |i: usize| {
            found.packages[i].manifest.contributes.commands[0]
                .key
                .clone()
        };
        assert_eq!(key(0).as_deref(), Some("cmd-alt-o"));
        assert_eq!((key(1), key(2)), (None, None));
        let p = found.problems.join("\n");
        assert!(p.contains("package `one.app` is already installed"), "{p}");
        assert!(
            p.contains("two.app.open: `cmd-alt-o` is one.app.open's"),
            "{p}"
        );
        assert!(
            p.contains("three.app.open: `shift-cmd-p` is the desktop's"),
            "{p}"
        );
        assert!(p.contains("e/app.toml"), "{p}");
        // Each on one line, even a parser's multi-line report.
        assert!(found.problems.iter().all(|x| !x.contains('\n')), "{p}");
        assert_eq!(found.problems.len(), 4, "{p}");
    }
}
