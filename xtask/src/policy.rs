//! `cargo xtask policy`: repository rules that are cheap to check mechanically.
//!
//! - No workflow may run on a hosted macOS runner (macOS is verified locally).
//! - Every third-party action is pinned to a full commit SHA.
//! - Product code opens no network sockets: remote hosts are reached only
//!   through system OpenSSH, and the daemon listens only on owner-only Unix
//!   sockets. Test code (a crate's `tests/`, `#[cfg(test)]` modules) and
//!   xtask may, for example to run a local sshd. One file is the exception
//!   ([`LOOPBACK_ONLY`]): forwards to a remote host's loopback ports, which
//!   travel over the host's SSH connection. It may use TCP, and only on the
//!   loopback: each of its binds and connects names `LOCALHOST`.
//! - The HTTP client (`ureq`) is used in one product file only
//!   ([`FETCH_ONLY`]): a script app's `fetch`, which checks every request
//!   and redirect against the hosts the app's manifest lists, under its
//!   `net.fetch` capability. That file opens no sockets of its own.

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
    let sources = rust_sources(&repo_root().join("crates"))?;
    for path in &sources {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path
            .strip_prefix(repo_root())
            .unwrap_or(path)
            .display()
            .to_string();
        violations.extend(check_source(&name, &text));
    }
    if violations.is_empty() {
        println!(
            "policy: ok ({checked} workflow file(s), {} source file(s))",
            sources.len()
        );
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

/// Network APIs product code must not use: SSH is the only way between
/// hosts.
const NETWORK: [&str; 7] = [
    "TcpListener",
    "TcpStream",
    "UdpSocket",
    "tokio::net::Tcp",
    "tokio::net::Udp",
    "std::net::",
    "ureq::",
];

/// The HTTP client, allowed only in [`FETCH_ONLY`].
const HTTP_CLIENT: &str = "ureq::";

/// The one product file that may use the HTTP client: a script app's
/// `fetch`, to the hosts its manifest lists.
const FETCH_ONLY: &str = "crates/app-script/src/fetch.rs";

/// The one product file that may use TCP (`web.forward`): it listens on
/// this machine's loopback and connects to the host's, nothing else.
const LOOPBACK_ONLY: &str = "crates/server/src/forward.rs";

/// Every `.rs` file under each crate's `src/` (not `tests/`, `benches/`,
/// or `examples/`), sorted.
fn rust_sources(crates: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let p = e.map_err(|e| e.to_string())?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(crates).map_err(|e| format!("{}: {e}", crates.display()))? {
        let src = e.map_err(|e| e.to_string())?.path().join("src");
        if src.is_dir() {
            walk(&src, &mut out)?;
        }
    }
    out.sort();
    Ok(out)
}

/// Network API uses in product code, ignoring comments and everything from
/// a `#[cfg(test)]` line on (test modules sit at a file's end).
fn check_source(name: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        if line.starts_with("#[cfg(test)]") {
            break;
        }
        if line.starts_with("//") {
            continue;
        }
        if name == LOOPBACK_ONLY {
            if (line.contains("::bind(") || line.contains("::connect("))
                && !line.contains("LOCALHOST")
            {
                out.push(format!(
                    "{name}:{}: forwards bind and connect only on the loopback (`Ipv4Addr::LOCALHOST`, `Ipv6Addr::LOCALHOST`)",
                    i + 1
                ));
            }
            if !line.contains("Udp") {
                continue;
            }
        }
        let found = NETWORK
            .iter()
            .find(|n| line.contains(**n) && !(name == FETCH_ONLY && **n == HTTP_CLIENT));
        if let Some(api) = found {
            out.push(format!(
                "{name}:{}: `{api}`: product code opens no network sockets; remote hosts are reached only through SSH",
                i + 1
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

    #[test]
    fn product_code_opens_no_network_sockets() {
        let bad = "use std::net::TcpListener;\nfn f() { let _ = tokio::net::TcpStream::connect(\"x\"); }\n";
        assert_eq!(check_source("f.rs", bad).len(), 2);
        let ok = "use tokio::net::UnixStream;\n// a TcpStream in a comment\n#[cfg(test)]\nmod tests { use std::net::TcpListener; }\n";
        assert!(check_source("f.rs", ok).is_empty());
    }

    #[test]
    fn only_fetch_uses_the_http_client_and_it_opens_no_sockets() {
        let client = "use ureq::http;\nlet a = ureq::Agent::config_builder();\n";
        assert!(check_source(FETCH_ONLY, client).is_empty());
        assert_eq!(check_source("crates/x/src/f.rs", client).len(), 2);
        let socket = "let s = std::net::TcpStream::connect(addr);\n";
        assert_eq!(check_source(FETCH_ONLY, socket).len(), 1);
    }

    #[test]
    fn forwards_use_tcp_only_on_the_loopback() {
        let ok = "use tokio::net::TcpListener;\nlet l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await;\nlet s = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).await;\n";
        assert!(check_source(LOOPBACK_ONLY, ok).is_empty());
        assert_eq!(check_source("crates/x/src/f.rs", ok).len(), 3);
        let wide = "let l = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await;\nlet s = TcpStream::connect(addr).await;\nlet u = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0));\n";
        assert_eq!(check_source(LOOPBACK_ONLY, wide).len(), 3);
    }
}
