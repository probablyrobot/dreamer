//! `cargo xtask policy`: repository rules that are cheap to check mechanically.
//!
//! - No workflow may run on a hosted macOS runner (macOS is verified locally).
//! - Every third-party action is pinned to a full commit SHA.

use crate::util::repo_root;

pub fn run(args: &[String]) -> Result<(), String> {
    if let Some(arg) = args.first() {
        return Err(format!("policy takes no arguments, got `{arg}`"));
    }
    let dir = repo_root().join(".github").join("workflows");
    let mut violations = Vec::new();
    let mut checked = 0;
    if dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yml" | "yaml")))
            .collect();
        entries.sort();
        for path in entries {
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path
                .strip_prefix(repo_root())
                .unwrap_or(&path)
                .display()
                .to_string();
            violations.extend(check_workflow(&name, &text));
            checked += 1;
        }
    }
    if violations.is_empty() {
        println!("policy: ok ({checked} workflow file(s))");
        Ok(())
    } else {
        for v in &violations {
            eprintln!("policy: {v}");
        }
        Err(format!("{} policy violation(s)", violations.len()))
    }
}

fn check_workflow(name: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = strip_comment(raw);
        let lineno = i + 1;
        if line.to_ascii_lowercase().contains("macos-") {
            out.push(format!(
                "{name}:{lineno}: hosted macOS runners are not used; macOS is verified by the local gate"
            ));
        }
        if let Some(target) = uses_target(line)
            && !is_pinned(target)
        {
            out.push(format!(
                "{name}:{lineno}: action `{target}` must be pinned to a full commit SHA"
            ));
        }
    }
    out
}

/// Drop a YAML comment. Good enough for workflow files: `#` only starts a
/// comment at the line start or after whitespace.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

fn uses_target(line: &str) -> Option<&str> {
    let trimmed = line.trim_start().trim_start_matches("- ").trim_start();
    let value = trimmed.strip_prefix("uses:")?.trim();
    Some(value.trim_matches(|c| c == '"' || c == '\''))
}

fn is_pinned(target: &str) -> bool {
    if target.starts_with("./") || target.starts_with("docker://") {
        return true;
    }
    match target.rsplit_once('@') {
        Some((_, rev)) => rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "3d3c42e5aac5ba805825da76410c181273ba90b1";

    #[test]
    fn macos_runners_are_rejected_but_comments_are_not() {
        let ok = "# macOS is verified locally, never on macos-15\nruns-on: ubuntu-24.04\n";
        assert!(check_workflow("w", ok).is_empty());
        let bad = "    runs-on: macos-15\n";
        assert_eq!(check_workflow("w", bad).len(), 1);
        let matrix = "        os: [ubuntu-24.04, macos-latest]\n";
        assert_eq!(check_workflow("w", matrix).len(), 1);
    }

    #[test]
    fn actions_must_be_pinned_to_a_sha() {
        let pinned = format!("      - uses: actions/checkout@{SHA} # v7\n");
        assert!(check_workflow("w", &pinned).is_empty());
        assert_eq!(
            check_workflow("w", "      - uses: actions/checkout@v7\n").len(),
            1
        );
        assert_eq!(check_workflow("w", "        uses: owner/repo\n").len(), 1);
        assert!(check_workflow("w", "      - uses: ./.github/actions/local\n").is_empty());
    }
}
