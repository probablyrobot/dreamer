use std::path::PathBuf;
use std::process::Command;

/// Repository root (the workspace directory containing `xtask/`).
pub fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().map(PathBuf::from).unwrap_or(manifest)
}

/// Run a program and return trimmed stdout on success, or `None` if it could
/// not be started or exited unsuccessfully.
pub fn capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Locate an executable on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Extract the first `major.minor[.patch]` version number found in `text`.
/// A trailing letter suffix (tmux uses versions like `3.8a`) is ignored.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let rest = &text[i..];
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(rest.len());
            let mut parts = rest[..end].split('.').filter(|p| !p.is_empty());
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                let c = parts.next().unwrap_or("0");
                if let (Ok(a), Ok(b), Ok(c)) = (a.parse(), b.parse(), c.parse()) {
                    return Some((a, b, c));
                }
            }
            i += end.max(1);
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_common_version_strings() {
        assert_eq!(parse_version("tmux 3.8"), Some((3, 8, 0)));
        assert_eq!(parse_version("tmux 3.8a"), Some((3, 8, 0)));
        assert_eq!(parse_version("tmux next-3.9"), Some((3, 9, 0)));
        assert_eq!(parse_version("0.14.0"), Some((0, 14, 0)));
        assert_eq!(
            parse_version("rustc 1.99.1 (58cf8f9c3 2026-10-13)"),
            Some((1, 99, 1))
        );
        assert_eq!(
            parse_version("Xcode 27.2\nBuild version 27B52"),
            Some((27, 2, 0))
        );
        assert_eq!(parse_version("no digits here"), None);
        assert_eq!(parse_version("build 7"), None);
    }
}
