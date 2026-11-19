//! `dream web install [FILE.zip | DIR]`, `dream web status`, and `dream
//! web remove`: Dreamer Web, the Chromium component that web panes need,
//! which is installed apart from the app (see the `web-component` crate).
//!
//! An install unpacks the component next to where it goes, checks it (its
//! manifest names this Dreamer and its CEF release, every file is there,
//! the framework and each helper app pass `codesign --verify`, and a
//! helper loads the framework from where it sits), and only then swaps it
//! in. Without a file it downloads this version's archive from the
//! release, and its checksum beside it.

use std::path::{Path, PathBuf};
use std::process::Command;

use web_component::{
    FRAMEWORK, HELPERS, NAME, State, TOP, VERSION, archive_name, check, helper_app,
    helper_executable, installed_dir, release_url,
};

pub const USAGE: &str =
    "usage: dream web install [FILE.zip | DIR] | dream web status | dream web remove";

/// Whether the component is installed and is this Dreamer's.
pub fn status() -> Result<(), String> {
    let dir = installed_dir();
    let state = check(&dir);
    match state.problem(&dir) {
        None => {
            if let State::Ready(m) = state {
                println!(
                    "{NAME} {} (Chromium {}) in {}",
                    m.version,
                    m.chromium(),
                    dir.display()
                );
            }
            Ok(())
        }
        Some(problem) => Err(problem),
    }
}

/// Remove the installed component. Web panes say how to add it again.
pub fn remove() -> Result<(), String> {
    let dir = installed_dir();
    if !dir.exists() {
        println!("{NAME} is not installed");
        return Ok(());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    println!("{NAME} removed from {}", dir.display());
    Ok(())
}

/// Install the component from `from` (an archive or an unpacked
/// component directory), or from this version's release.
pub fn install(from: Option<&str>) -> Result<(), String> {
    let target = web_component::TARGET
        .ok_or_else(|| format!("{NAME} is built for macOS on Apple Silicon only so far"))?;
    let dir = installed_dir();
    let parent = dir
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", dir.display()))?
        .to_path_buf();
    // The data directory is private, as the daemon requires it to be.
    platform::private_dir(&parent)?;
    // Beside the destination, so the swap is a rename.
    let staging = parent.join(format!(".web-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let result = (|| {
        let unpacked = staging.join(TOP);
        match from {
            Some(path) if Path::new(path).is_dir() => {
                run(
                    Command::new("ditto").arg(path).arg(&unpacked),
                    "copying the component",
                )?;
            }
            Some(path) => {
                let zip = Path::new(path);
                verify_sum(zip, None)?;
                unzip(zip, &staging)?;
            }
            None => {
                let name = archive_name(VERSION, target);
                let url = release_url(VERSION, target);
                let zip = staging.join(&name);
                println!("{NAME} {VERSION}: downloading {url}");
                run(
                    curl(&url).arg("-o").arg(&zip),
                    &format!("downloading {url}"),
                )?;
                let sum = run(
                    &mut curl(&format!("{url}.sha256")),
                    &format!("downloading {url}.sha256"),
                )?;
                verify_sum(&zip, Some(&sum))?;
                unzip(&zip, &staging)?;
            }
        }
        verify(&unpacked)?;
        swap(&unpacked, &dir, &parent)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let manifest = result?;
    println!(
        "{NAME} {} (Chromium {}) installed in {}",
        manifest.version,
        manifest.chromium(),
        dir.display()
    );
    println!("Web panes opened from now on use it.");
    Ok(())
}

fn curl(url: &str) -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--retry", "2", "--connect-timeout", "20"])
        .arg(url);
    c
}

/// Run `cmd`; its standard output, or why it failed.
fn run(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn unzip(zip: &Path, into: &Path) -> Result<(), String> {
    run(
        Command::new("ditto").args(["-x", "-k"]).arg(zip).arg(into),
        &format!("unpacking {}", zip.display()),
    )?;
    if into.join(TOP).is_dir() {
        Ok(())
    } else {
        Err(format!(
            "{} is not a {NAME} archive (it has no {TOP}/)",
            zip.display()
        ))
    }
}

/// Check `zip` against `sum` (a `.sha256` file's text), or against the
/// `.sha256` file beside it if there is one.
fn verify_sum(zip: &Path, sum: Option<&str>) -> Result<(), String> {
    let beside = PathBuf::from(format!("{}.sha256", zip.display()));
    let text = match sum {
        Some(s) => s.to_owned(),
        None => match std::fs::read_to_string(&beside) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        },
    };
    let want = text.split_whitespace().next().unwrap_or_default();
    let got = sha256_file(zip)?;
    if want.eq_ignore_ascii_case(&got) {
        Ok(())
    } else {
        Err(format!(
            "{}: SHA-256 {got}, but its checksum says {want}",
            zip.display()
        ))
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>())
}

/// The unpacked component is this Dreamer's, complete, signed, and its
/// helper loads its framework.
fn verify(dir: &Path) -> Result<web_component::Manifest, String> {
    let state = check(dir);
    let manifest = match state {
        State::Ready(m) => m,
        State::Other(m) => {
            return Err(format!(
                "this is {NAME} for Dreamer {} (CEF {}), and this is Dreamer {VERSION}: \
                 install {}",
                m.version,
                m.cef,
                archive_name(VERSION, web_component::TARGET.unwrap_or_default())
            ));
        }
        State::Missing => return Err(format!("{}: not found", dir.display())),
        State::Broken(why) => return Err(format!("not a usable {NAME}: {why}")),
    };
    let mut signed = vec![dir.join(FRAMEWORK)];
    signed.extend(
        HELPERS
            .iter()
            .map(|(suffix, _)| dir.join(helper_app(suffix))),
    );
    for path in signed {
        run(
            Command::new("codesign")
                .args(["--verify", "--strict"])
                .arg(&path),
            &format!("codesign --verify {}", path.display()),
        )?;
    }
    // With no process type, CEF's entry point returns at once (-1): this
    // checks only that the helper finds and loads the framework.
    let helper = helper_executable(dir, "");
    let out = Command::new(&helper)
        .output()
        .map_err(|e| format!("{}: {e}", helper.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(255) || stderr.contains("dreamer-web-helper:") {
        return Err(format!(
            "the helper did not load the framework ({}): {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(manifest)
}

/// Put `new` at `dir`, moving any older component away first.
fn swap(new: &Path, dir: &Path, parent: &Path) -> Result<web_component::Manifest, String> {
    let manifest = match check(new) {
        State::Ready(m) => m,
        _ => return Err(format!("{}: changed while installing", new.display())),
    };
    let old = parent.join(format!(".web-old-{}", std::process::id()));
    if dir.exists() {
        std::fs::rename(dir, &old).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if let Err(e) = std::fs::rename(new, dir) {
        if old.exists() {
            let _ = std::fs::rename(&old, dir);
        }
        return Err(format!("{}: {e}", dir.display()));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(manifest)
}
