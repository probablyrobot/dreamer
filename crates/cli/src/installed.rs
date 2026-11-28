//! Installed apps and the local registry.
//!
//! Each installed app is its package, `<config>/packages/<name>.dreamerapp`
//! (versioned with the environment), and a folder unpacked from it,
//! `<config>/apps/<name>/` (not versioned: [`sync`] rebuilds it). Every
//! version installed is also kept in the local registry in the data
//! directory, so any of them can be installed again by id and version.

use std::path::{Path, PathBuf};

use app_registry::{Added, Registry, Version};

use crate::build::{apps, valid_name};

/// The local registry, `<data>/registry`.
pub(crate) fn registry() -> Registry {
    Registry::new(platform::data_dir().join(app_registry::DIR))
}

fn config() -> Result<PathBuf, String> {
    workspace_config::config_dir()
        .ok_or_else(|| "cannot determine the configuration directory (HOME unset)".into())
}

/// `packages/<name>.dreamerapp`, relative to the configuration directory.
pub(crate) fn package_rel(name: &str) -> String {
    format!("packages/{name}.{}", app_package::EXTENSION)
}

fn fail(p: &Path, e: std::io::Error) -> String {
    format!("{}: {e}", p.display())
}

/// What a manifest adds to its registry entry: the Dreamer it needs and
/// the grant it asks for.
fn entry_of(manifest: &str) -> (Option<String>, Vec<String>) {
    let table = manifest.parse::<toml::Table>().unwrap_or_default();
    let dreamer = table
        .get("package")
        .and_then(|p| p.get("dreamer"))
        .and_then(|d| d.as_str())
        .map(str::to_owned);
    let capabilities = table
        .get("capabilities")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_owned))
        .collect();
    (dreamer, capabilities)
}

/// Record version `version` of `id`, whose package is `bytes`, in the
/// registry. Says so, and refuses the same version with other contents.
pub(crate) fn record(id: &str, version: &str, bytes: &[u8], manifest: &str) -> Result<(), String> {
    let reg = registry();
    let lock = reg.lock()?;
    let sha = app_package::sha256_hex(bytes);
    let (dreamer, capabilities) = entry_of(manifest);
    match reg.add(
        &lock,
        id,
        Version::local(version, &sha, dreamer, capabilities),
        bytes,
    )? {
        Added::New => println!("recorded {id} {version} in the registry (sha256 {sha})"),
        Added::Known => println!("{id} {version} is in the registry already (sha256 {sha})"),
    }
    Ok(())
}

/// The version to record a folder's package as: the manifest's, or with
/// `dev` the next `<version>-dev.N` (the same one again for the same
/// files).
pub(crate) fn version_for(
    id: &str,
    manifest_version: &str,
    sha256: &str,
    dev: bool,
) -> Result<String, String> {
    if dev {
        registry().dev_version(id, manifest_version, sha256)
    } else {
        Ok(manifest_version.to_owned())
    }
}

/// Unpack `bytes` into `apps/<name>/` and build it, replacing what is
/// there. With `keep_last`, what it replaces becomes `apps/.last-good/`;
/// otherwise a folder that differs from its package is moved to
/// `apps/.edited-<name>/`.
fn place(name: &str, bytes: &[u8], keep_last: bool) -> Result<PathBuf, String> {
    valid_name(name)?;
    let apps = apps()?;
    std::fs::create_dir_all(&apps).map_err(|e| fail(&apps, e))?;
    let dest = apps.join(name);
    let staging = apps.join(format!(".staging-{name}"));
    let _ = std::fs::remove_dir_all(&staging);
    let built = app_package::unpack(bytes, &staging)
        .and_then(|_| crate::strip::build(&staging.join(app_package::MANIFEST)));
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if dest.exists() {
        let aside = if keep_last {
            let last = apps.join(".last-good");
            std::fs::create_dir_all(&last).map_err(|e| fail(&last, e))?;
            last.join(name)
        } else {
            apps.join(format!(".edited-{name}"))
        };
        let _ = std::fs::remove_dir_all(&aside);
        std::fs::rename(&dest, &aside).map_err(|e| fail(&dest, e))?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| fail(&staging, e))?;
    Ok(dest)
}

/// Install package `bytes` (version `version` of `id`) as `name`: its
/// package goes to `packages/`, its folder to `apps/`, and the
/// environment saves the package.
pub(crate) fn deploy(name: &str, id: &str, version: &str, bytes: &[u8]) -> Result<(), String> {
    let dest = place(name, bytes, true)?;
    let rel = package_rel(name);
    app_registry::write_atomic(&config()?.join(&rel), bytes)?;
    println!("installed {id} {version} as {name} ({})", dest.display());
    crate::env::autosave(&[&rel], &format!("Install app {name} {version}"));
    Ok(())
}

/// After `dream app rollback`: the package follows the folder put back.
pub(crate) fn repackage(name: &str) -> Result<(), String> {
    let packed = app_package::pack(&apps()?.join(name))?;
    let rel = package_rel(name);
    app_registry::write_atomic(&config()?.join(&rel), &packed.bytes)?;
    crate::env::autosave(&[&rel], &format!("Roll back app {name}"));
    Ok(())
}

/// How an installed folder stands against its package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Unpacked from its package, unchanged.
    Same,
    /// Changed by hand since it was unpacked.
    Edited,
    /// The package is there; the folder is not.
    Missing,
    /// A folder with no package (installed by an earlier Dreamer, not
    /// migrated yet).
    Unrecorded,
}

impl Standing {
    pub(crate) fn note(self) -> &'static str {
        match self {
            Standing::Same => "",
            Standing::Edited => {
                "  (edited: differs from its package; `dream app sync` puts the package back)"
            }
            Standing::Missing => "  (not unpacked: `dream app sync` unpacks it)",
            Standing::Unrecorded => "  (not recorded yet: `dream app sync` records it)",
        }
    }
}

/// The installed names: folders under `apps/` with a manifest, and
/// packages under `packages/`, sorted, each with its standing.
pub(crate) fn installed(config: &Path) -> Vec<(String, Standing)> {
    let apps = config.join("apps");
    let packages = config.join("packages");
    let mut names: Vec<String> = std::fs::read_dir(&apps)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && apps.join(n).join(app_package::MANIFEST).is_file())
        .collect();
    let suffix = format!(".{}", app_package::EXTENSION);
    names.extend(
        std::fs::read_dir(&packages)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_suffix(&suffix)
                    .map(str::to_owned)
            })
            .filter(|n| valid_name(n).is_ok()),
    );
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|n| {
            let standing = standing(&apps.join(&n), &packages.join(format!("{n}{suffix}")));
            (n, standing)
        })
        .collect()
}

fn standing(folder: &Path, package: &Path) -> Standing {
    let Ok(bytes) = std::fs::read(package) else {
        return Standing::Unrecorded;
    };
    if !folder.join(app_package::MANIFEST).is_file() {
        return Standing::Missing;
    }
    match app_package::pack(folder) {
        Ok(p) if p.sha256 == app_package::sha256_hex(&bytes) => Standing::Same,
        _ => Standing::Edited,
    }
}

/// The names whose folder differs from its package, for `dream env
/// status`.
pub(crate) fn differing(config: &Path) -> Vec<String> {
    installed(config)
        .into_iter()
        .filter(|(_, s)| matches!(s, Standing::Edited | Standing::Missing))
        .map(|(n, _)| n)
        .collect()
}

/// Record every folder under `apps/` that has no package yet (installed
/// by an earlier Dreamer): packed, added to the registry at its manifest
/// version (`0.0.0-migrated` without a usable one), its package written
/// to `packages/`. Then the environment, if there is one, versions
/// `packages/` and stops versioning `apps/`. What it recorded.
pub(crate) fn migrate() -> Result<Vec<String>, String> {
    let config = config()?;
    let mut done = Vec::new();
    let mut problems = Vec::new();
    for (name, standing) in installed(&config) {
        if standing != Standing::Unrecorded {
            continue;
        }
        let folder = config.join("apps").join(&name);
        let result = (|| {
            let packed = app_package::pack(&folder)?;
            let manifest = std::fs::read_to_string(folder.join(app_package::MANIFEST))
                .map_err(|e| fail(&folder, e))?;
            let table = manifest.parse::<toml::Table>().unwrap_or_default();
            let id = table
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or(&name)
                .to_owned();
            let version = table
                .get("version")
                .and_then(|v| v.as_str())
                .filter(|v| semver_ok(v))
                .unwrap_or("0.0.0-migrated")
                .to_owned();
            // A version recorded with other files keeps its package here;
            // the registry is told nothing it would refuse.
            if let Err(e) = record(&id, &version, &packed.bytes, &manifest) {
                eprintln!("dream: {name}: not recorded in the registry: {e}");
            }
            app_registry::write_atomic(&config.join(package_rel(&name)), &packed.bytes)
        })();
        match result {
            Ok(()) => done.push(name),
            Err(e) => problems.push(format!("{name}: {e}")),
        }
    }
    catch_up(&config, &done);
    if problems.is_empty() {
        Ok(done)
    } else {
        Err(format!(
            "these apps could not be recorded:\n  {}",
            problems.join("\n  ")
        ))
    }
}

fn semver_ok(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    core.split('.').count() == 3
        && core
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// An environment saved before `apps/` was ignored: version `packages/`
/// instead, in one automatic save.
fn catch_up(config: &Path, recorded: &[String]) {
    if !config.join(".git").exists() {
        return;
    }
    let ignore = std::fs::read_to_string(config.join(".gitignore")).unwrap_or_default();
    if ignore.lines().any(|l| l.trim() == "/apps/") && recorded.is_empty() {
        return;
    }
    let env = environment::Environment::new(config);
    match env.catch_up(
        &["apps"],
        &["packages"],
        "Version app packages; installed apps are unpacked from them",
    ) {
        Ok(Some(save)) => println!("environment saved: {} {}", save.short, save.message),
        Ok(None) => {}
        Err(e) => eprintln!("dream: the environment was not saved: {e}"),
    }
}

/// `dream app sync [--quiet]`: record what an earlier Dreamer installed,
/// then unpack every package whose folder is missing or differs from it.
/// A folder with hand edits is moved to `apps/.edited-<name>/` first.
pub fn sync(args: &[String]) -> Result<(), String> {
    let quiet = match args {
        [] => false,
        [q] if q == "--quiet" => true,
        _ => return Err(crate::build::USAGE.into()),
    };
    let say = |s: String| {
        if !quiet {
            println!("{s}");
        }
    };
    let mut problems = Vec::new();
    match migrate() {
        Ok(done) => {
            for n in done {
                say(format!("recorded {n}, installed by an earlier Dreamer"));
            }
        }
        Err(e) => problems.push(e),
    }
    let config = config()?;
    for (name, standing) in installed(&config) {
        if !matches!(standing, Standing::Edited | Standing::Missing) {
            continue;
        }
        let package = config.join(package_rel(&name));
        let placed = std::fs::read(&package)
            .map_err(|e| fail(&package, e))
            .and_then(|bytes| place(&name, &bytes, false));
        match (placed, standing) {
            (Ok(_), Standing::Edited) => say(format!(
                "unpacked {name} again; the edited folder is apps/.edited-{name}"
            )),
            (Ok(_), _) => say(format!("unpacked {name}")),
            (Err(e), _) => problems.push(format!("{name}: {e}")),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}
