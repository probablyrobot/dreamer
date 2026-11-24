//! `cargo xtask build-report [--ci-run ID]`: the build report
//! from what this checkout built and checked, for a release.
//!
//! It writes `target/package/build-report.json` and `build-report.md`:
//!
//! - the source (commit, tree, whether the work tree is clean) and the
//!   pinned toolchains and native dependencies;
//! - every artifact under `target/package` (size, SHA-256 checked against
//!   its `.sha256` file, target, and for the macOS zips, the base app's
//!   and the Dreamer Web component's, their signing state);
//! - the runtime evidence:
//!   - the macOS gate report for this exact tree;
//!   - the fresh-container checks (`cargo xtask package --fresh`), and the
//!     Dreamer Web zip added away from the tree on the Mac (`cargo xtask
//!     package --fresh --platform macos`), matched to the artifacts by
//!     checksum;
//!   - with `--ci-run`, that GitHub Actions run's jobs;
//! - everything not run.
//!
//! Nothing here is assumed: an artifact without a matching check is listed
//! as not checked, and a tree the macOS gate has not passed marks macOS
//! runtime verification incomplete.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::util::repo_root;

fn git(root: &Path, args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn first_line(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| {
            let text = if o.stdout.is_empty() {
                o.stderr
            } else {
                o.stdout
            };
            String::from_utf8_lossy(&text)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .unwrap_or_default()
}

/// `key = "value"` from a section of a small TOML file.
fn toml_value(text: &str, section: &str, key: &str) -> String {
    let mut inside = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            inside = l == format!("[{section}]");
            continue;
        }
        if inside
            && let Some((k, v)) = l.split_once('=')
            && k.trim() == key
        {
            return v.trim().trim_matches('"').to_owned();
        }
    }
    String::new()
}

fn target_of(name: &str) -> &'static str {
    for t in [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ] {
        if name.contains(t) {
            return t;
        }
    }
    "unknown"
}

fn artifacts(out: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for dir in [out.to_path_buf(), out.join("linux"), out.join("server")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if [".zip", ".tar.gz", ".deb", ".rpm"]
                .iter()
                .any(|ext| n.ends_with(ext))
            {
                found.push(p);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

pub fn run(args: &[String]) -> Result<(), String> {
    let mut ci_run: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ci-run" => ci_run = Some(it.next().ok_or("--ci-run needs a run id")?.clone()),
            other => return Err(format!("unknown build-report argument `{other}`")),
        }
    }
    let root = repo_root();
    let out = root.join("target/package");
    let commit = git(&root, &["rev-parse", "HEAD"]);
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
    let dirty = !git(&root, &["status", "--porcelain", "--untracked-files=no"]).is_empty();
    let baseline = std::fs::read_to_string(root.join("deps/baseline.toml")).unwrap_or_default();
    let channel = toml_value(
        &std::fs::read_to_string(root.join("rust-toolchain.toml")).unwrap_or_default(),
        "toolchain",
        "channel",
    );
    let mut not_run: Vec<String> = Vec::new();

    // Fresh-container checks, by checksum.
    // (`macos`: the Dreamer Web zip added away from the tree on this Mac.)
    let fresh: Vec<Value> = ["arm64", "amd64", "macos"]
        .iter()
        .filter_map(|a| read_json(&out.join(format!("fresh-{a}.json"))))
        .collect();
    let fresh_for = |sha: &str| -> Option<String> {
        fresh.iter().find_map(|f| {
            f["checked"].as_array()?.iter().find_map(|c| {
                (c["sha256"].as_str() == Some(sha)).then(|| {
                    format!(
                        "{} ({} {})",
                        c["check"].as_str().unwrap_or_default(),
                        f["image"].as_str().unwrap_or_default(),
                        f["arch"].as_str().unwrap_or_default()
                    )
                })
            })
        })
    };

    let version = crate::package::version(&root)?;
    let mut list = Vec::new();
    for p in artifacts(&out)? {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Another version's leftovers. (The Linux tarball's name has no
        // version: install.sh fetches it by a stable name.)
        let versioned = name
            .strip_prefix("dreamer-server-")
            .or_else(|| name.strip_prefix("dreamer-web-"))
            .or_else(|| name.strip_prefix("dreamer-"))
            .and_then(|rest| rest.split('-').next())
            .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()));
        if versioned.is_some_and(|v| v != version) {
            continue;
        }
        let sha = crate::util::sha256_file(&p)?;
        let recorded = std::fs::read_to_string(p.with_file_name(format!("{name}.sha256")))
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_owned));
        let sum_file = match recorded.as_deref() {
            Some(r) if r == sha => "matches",
            Some(_) => "DIFFERS",
            None => "missing",
        };
        let target = target_of(&name);
        let record = read_json(&p.with_file_name(format!("{name}.json")));
        let mut checked = fresh_for(&sha);
        if name.ends_with(".zip") {
            if record.as_ref().and_then(|r| r["commit"].as_str()) != Some(commit.as_str()) {
                not_run.push(format!(
                    "{name}: its record is for another commit; rebuild it"
                ));
            }
            if record.as_ref().map(|r| &r["signing"]["notarized"]) != Some(&Value::Bool(true)) {
                not_run.push(format!(
                    "{name}: not Developer ID signed and notarized (`cargo xtask package --sign ID --notarize PROFILE`)"
                ));
            }
            if checked.is_none() {
                checked = Some("the macOS gate's package stage (see the gate report)".into());
            }
        }
        if checked.is_none() {
            not_run.push(format!("{name}: not run on a fresh machine"));
        }
        list.push(json!({
            "artifact": name,
            "path": p.strip_prefix(&root).unwrap_or(&p).display().to_string(),
            "target": target,
            "bytes": std::fs::metadata(&p).map_or(0, |m| m.len()),
            "sha256": sha,
            "sha256_file": sum_file,
            "signing": record.as_ref().map_or(Value::Null, |r| r["signing"].clone()),
            "runtime_check": checked,
        }));
    }
    if list.is_empty() {
        return Err("no artifacts in target/package; build them first".into());
    }

    // The macOS gate for this exact tree.
    let gate = read_json(&root.join(format!("target/gate/mac/{tree}-full.json")));
    let gate_summary = match &gate {
        Some(g) => {
            for s in g["stages"].as_array().into_iter().flatten() {
                if s["result"] == "not_run" {
                    not_run.push(format!(
                        "macOS gate stage `{}` ({})",
                        s["name"].as_str().unwrap_or_default(),
                        s["command"].as_str().unwrap_or_default()
                    ));
                }
            }
            json!({
                "tree": tree,
                "result": g["result"],
                "unix_time": g["unix_time"],
                "stages": g["stages"],
            })
        }
        None => {
            not_run.push(format!(
                "macOS runtime verification incomplete: no full gate report for tree {tree}"
            ));
            Value::Null
        }
    };
    if dirty {
        not_run.push(
            "the work tree has uncommitted changes: artifacts may not match the commit".into(),
        );
    }

    // The Linux lane.
    let ci = match &ci_run {
        Some(id) => {
            let out = Command::new("gh")
                .args([
                    "run",
                    "view",
                    id,
                    "--json",
                    "conclusion,headSha,url,jobs,workflowName",
                ])
                .current_dir(&root)
                .output()
                .map_err(|e| format!("gh run view: {e}"))?;
            let v: Value = serde_json::from_slice(&out.stdout)
                .map_err(|e| format!("gh run view {id}: {e}"))?;
            if v["headSha"].as_str() != Some(commit.as_str()) {
                not_run.push(format!("CI run {id} is for another commit"));
            }
            json!({
                "run": id,
                "workflow": v["workflowName"],
                "url": v["url"],
                "conclusion": v["conclusion"],
                "jobs": v["jobs"].as_array().into_iter().flatten()
                    .map(|j| json!({ "name": j["name"], "conclusion": j["conclusion"] }))
                    .collect::<Vec<_>>(),
            })
        }
        None => {
            not_run.push("Linux CI lane not attached (pass --ci-run ID)".into());
            Value::Null
        }
    };
    not_run.extend([
        "the desktop on Wayland and on real Linux GPUs (the fresh check uses X11 with software Vulkan)".to_owned(),
    ]);

    let report = json!({
        "source": {
            "commit": commit,
            "tree": tree,
            "clean": !dirty,
            "version": version,
        },
        "toolchains": {
            "rust": channel,
            "rustc": first_line("rustc", &["-V"]),
            "zig": toml_value(&baseline, "zig", "version"),
            "macos_sdk": if cfg!(target_os = "macos") { first_line("xcrun", &["--show-sdk-version"]) } else { String::new() },
        },
        "dependencies": {
            "libghostty-vt": format!(
                "{} ({})",
                toml_value(&baseline, "ghostty", "version"),
                toml_value(&baseline, "ghostty", "commit")
            ),
            "tmux_baseline": toml_value(&baseline, "tmux", "version"),
            // In the Dreamer Web component only, never in the base packages.
            "cef": format!(
                "{} (Chromium {}), in the Dreamer Web component",
                toml_value(&baseline, "cef", "version"),
                toml_value(&baseline, "cef", "chromium")
            ),
            "gpui": "pinned in Cargo.toml and Cargo.lock (see docs/dependency-baseline.md)",
        },
        "compatibility": {
            "macos": "Apple Silicon, macOS 15.0 or later",
            "linux": "glibc 2.39 or later (built on Ubuntu 24.04), x86-64 and ARM64",
        },
        "artifacts": list,
        "macos_gate": gate_summary,
        "fresh_checks": fresh,
        "ci": ci,
        "not_run": not_run,
    });
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    std::fs::write(
        out.join("build-report.json"),
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(out.join("build-report.md"), markdown(&report)).map_err(|e| e.to_string())?;
    println!("build-report: {}", out.join("build-report.md").display());
    println!("build-report: {}", out.join("build-report.json").display());
    Ok(())
}

fn s(v: &Value) -> String {
    match v {
        Value::String(t) => t.clone(),
        Value::Null => "—".into(),
        other => other.to_string(),
    }
}

fn markdown(r: &Value) -> String {
    let mut m = String::new();
    let src = &r["source"];
    m.push_str(&format!(
        "# Dreamer {} build report\n\n- **Commit:** `{}` (tree `{}`){}\n",
        s(&src["version"]),
        s(&src["commit"]),
        s(&src["tree"]),
        if src["clean"] == true {
            ""
        } else {
            ", **with uncommitted changes**"
        }
    ));
    let t = &r["toolchains"];
    let d = &r["dependencies"];
    m.push_str(&format!(
        "- **Toolchains:** Rust {} (`{}`), Zig {}{}\n- **Pinned:** libghostty-vt {}, tmux baseline {}; GPUI {}; CEF {}\n",
        s(&t["rust"]),
        s(&t["rustc"]),
        s(&t["zig"]),
        if s(&t["macos_sdk"]).is_empty() { String::new() } else { format!(", macOS SDK {}", s(&t["macos_sdk"])) },
        s(&d["libghostty-vt"]),
        s(&d["tmux_baseline"]),
        s(&d["gpui"]),
        s(&d["cef"]),
    ));
    m.push_str(&format!(
        "- **Compatibility:** {}; Linux {}\n\n## Artifacts\n\n| Artifact | Target | Size | SHA-256 | Signing | Runtime check |\n| --- | --- | --- | --- | --- | --- |\n",
        s(&r["compatibility"]["macos"]),
        s(&r["compatibility"]["linux"]),
    ));
    for a in r["artifacts"].as_array().into_iter().flatten() {
        let signing = if a["signing"].is_null() {
            "—".to_owned()
        } else {
            format!(
                "{}, team {}, notarized: {}",
                s(&a["signing"]["kind"]),
                s(&a["signing"]["team"]),
                s(&a["signing"]["notarized"])
            )
        };
        m.push_str(&format!(
            "| `{}` | {} | {:.1} MB | `{}` ({} `.sha256`) | {} | {} |\n",
            s(&a["artifact"]),
            s(&a["target"]),
            a["bytes"].as_f64().unwrap_or(0.0) / 1e6,
            s(&a["sha256"]),
            s(&a["sha256_file"]),
            signing,
            a["runtime_check"].as_str().unwrap_or("**not checked**"),
        ));
    }
    m.push_str("\n## macOS gate\n\n");
    let g = &r["macos_gate"];
    if g.is_null() {
        m.push_str(
            "No full gate report for this tree: macOS runtime verification is incomplete.\n",
        );
    } else {
        m.push_str(&format!(
            "Tree `{}`: **{}**.\n\n| Stage | Result |\n| --- | --- |\n",
            s(&g["tree"]),
            s(&g["result"])
        ));
        for st in g["stages"].as_array().into_iter().flatten() {
            m.push_str(&format!("| {} | {} |\n", s(&st["name"]), s(&st["result"])));
        }
    }
    m.push_str("\n## Linux CI\n\n");
    let ci = &r["ci"];
    if ci.is_null() {
        m.push_str("Not attached.\n");
    } else {
        m.push_str(&format!(
            "{} run [{}]({}): **{}**.\n\n",
            s(&ci["workflow"]),
            s(&ci["run"]),
            s(&ci["url"]),
            s(&ci["conclusion"])
        ));
        for j in ci["jobs"].as_array().into_iter().flatten() {
            m.push_str(&format!("- {}: {}\n", s(&j["name"]), s(&j["conclusion"])));
        }
    }
    m.push_str("\n## Not run\n\n");
    for n in r["not_run"].as_array().into_iter().flatten() {
        m.push_str(&format!("- {}\n", s(n)));
    }
    m
}
