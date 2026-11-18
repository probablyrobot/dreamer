//! Where an agent builds apps, and the `dream app` steps it runs there,
//! for `dream mcp`'s app tools.
//!
//! An agent names package directories inside one root: `DREAMER_APPS_ROOT`
//! when set, else the working directory of the server (the harness's).
//! A path that leaves the root, through `..` or a symbolic link, is
//! refused, so the tools never read or write anywhere else. Installing
//! copies into Dreamer's apps directory, as `dream app install` does.

use std::path::{Component, Path, PathBuf};

/// The root package directories must be inside: canonical.
pub fn root() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("DREAMER_APPS_ROOT").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir().map_err(|e| format!("the working directory: {e}"))?,
    };
    dir.canonicalize()
        .map_err(|e| format!("the apps directory {}: {e}", dir.display()))
}

/// `dir` (relative to `root`, or absolute) as a path inside `root`,
/// following the symbolic links of the part that exists; or why not.
pub fn resolve(root: &Path, dir: &str) -> Result<PathBuf, String> {
    if dir.trim().is_empty() {
        return Err("give a directory".into());
    }
    let p = Path::new(dir);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "`{dir}`: `..` is not allowed; name a directory inside {}",
            root.display()
        ));
    }
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    // The longest part that exists, with its links resolved, then the rest.
    let mut existing = joined.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(n) => rest.push(n.to_owned()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut real = existing
        .canonicalize()
        .map_err(|e| format!("{}: {e}", existing.display()))?;
    for n in rest.into_iter().rev() {
        real.push(n);
    }
    if real.starts_with(root) {
        Ok(real)
    } else {
        Err(format!(
            "`{dir}` is outside the apps directory {}; the app tools work only there",
            root.display()
        ))
    }
}

/// What `dream ARGS` printed, and whether it succeeded.
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run this program with `args`, off the async thread.
pub async fn dream(args: Vec<String>) -> Result<Ran, String> {
    tokio::task::spawn_blocking(move || {
        let me = std::env::current_exe().map_err(|e| format!("this program: {e}"))?;
        let out = std::process::Command::new(&me)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", me.display()))?;
        Ok(Ran {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Standard base64, padded: how MCP carries an image.
pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(A[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    }

    #[test]
    fn a_directory_outside_the_root_is_refused() {
        let base = std::env::temp_dir().join(format!("drm-appdir-{}", std::process::id()));
        let root = base.join("root");
        std::fs::create_dir_all(root.join("inside")).expect("dir");
        std::fs::create_dir_all(base.join("outside")).expect("dir");
        let root = root.canonicalize().expect("root");
        std::os::unix::fs::symlink(base.join("outside"), root.join("link")).expect("link");

        assert_eq!(resolve(&root, "inside").expect("in"), root.join("inside"));
        assert_eq!(
            resolve(&root, "new/app").expect("new"),
            root.join("new/app")
        );
        let abs = root.join("inside").display().to_string();
        assert!(resolve(&root, &abs).is_ok());
        for bad in [
            "../outside",
            "inside/../../outside",
            "link",
            "link/app",
            "/etc",
            "",
        ] {
            assert!(resolve(&root, bad).is_err(), "{bad} was allowed");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
