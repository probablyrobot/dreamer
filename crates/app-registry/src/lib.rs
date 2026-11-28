//! The local app registry: every version of every app installed on this
//! machine, kept by its package's SHA-256 in the data directory.
//!
//! ```text
//! <data>/registry/
//!   index/<namespace>/<name>.json    one JSON object a line, one line a version
//!   packages/<sha256>.dreamerapp     the package itself
//! ```
//!
//! A line of the index: `version`, `sha256`, `dreamer` (the Dreamer
//! versions it needs, or `null`), `capabilities` (the grant it asks for),
//! `yanked`, `source` (`local`; `registry+URL` and `git+URL#rev` are
//! reserved), and `added` (UTC, RFC 3339). Unknown keys are kept as they
//! are read, so an index a later Dreamer wrote still reads. It is the
//! form a remote registry would serve as static files.
//!
//! A version names one package: the same id and version with other
//! contents is refused. Files are written beside their place and renamed
//! into it, and one writer at a time holds [`Registry::lock`].

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The registry's folder in the data directory.
pub const DIR: &str = "registry";

/// Where an app with no namespace in its id is indexed (an app installed
/// before ids had one).
pub const LOCAL_NAMESPACE: &str = "local";

/// The `source` of a version added on this machine.
pub const SOURCE_LOCAL: &str = "local";

/// One version of an app, as its index line says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Version {
    pub version: String,
    pub sha256: String,
    #[serde(default)]
    pub dreamer: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub yanked: bool,
    pub source: String,
    pub added: String,
    /// Keys this Dreamer does not know, kept as read.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Version {
    /// A version added here now.
    pub fn local(
        version: &str,
        sha256: &str,
        dreamer: Option<String>,
        capabilities: Vec<String>,
    ) -> Version {
        Version {
            version: version.to_owned(),
            sha256: sha256.to_owned(),
            dreamer,
            capabilities,
            yanked: false,
            source: SOURCE_LOCAL.to_owned(),
            added: now_utc(),
            other: serde_json::Map::new(),
        }
    }

    /// Whether Dreamer `dreamer` runs it: its requirement matches the
    /// version, or the release a pre-release leads to.
    pub fn runs_on(&self, dreamer: &str) -> bool {
        let Some(req) = &self.dreamer else {
            return true;
        };
        let (Ok(req), Ok(v)) = (
            semver::VersionReq::parse(req),
            semver::Version::parse(dreamer),
        ) else {
            return false;
        };
        req.matches(&v) || req.matches(&semver::Version::new(v.major, v.minor, v.patch))
    }

    fn semver(&self) -> Option<semver::Version> {
        semver::Version::parse(&self.version).ok()
    }
}

/// What [`Registry::add`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// A version the registry did not have.
    New,
    /// The registry had this version with these contents already.
    Known,
}

/// The registry in one directory (usually `<data>/registry`).
#[derive(Debug, Clone)]
pub struct Registry {
    root: PathBuf,
}

/// Held while writing; another writer waits for it.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] File);

impl Registry {
    pub fn new(root: impl Into<PathBuf>) -> Registry {
        Registry { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Wait until no one else writes to this registry, then hold it until
    /// the lock is dropped.
    pub fn lock(&self) -> Result<Lock, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| fail(&self.root, e))?;
        let path = self.root.join(".lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| fail(&path, e))?;
        file.lock().map_err(|e| fail(&path, e))?;
        Ok(Lock(file))
    }

    /// `index/<namespace>/<name>.json` for `id`.
    pub fn index_path(&self, id: &str) -> Result<PathBuf, String> {
        let (namespace, name) = id.split_once('.').unwrap_or((LOCAL_NAMESPACE, id));
        for part in [namespace, name] {
            let ok = !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
            if !ok {
                return Err(format!("`{id}` is not an app id the registry can hold"));
            }
        }
        Ok(self
            .root
            .join("index")
            .join(namespace)
            .join(format!("{name}.json")))
    }

    /// `packages/<sha256>.dreamerapp`.
    pub fn package_path(&self, sha256: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(format!("{sha256}.{}", app_package::EXTENSION))
    }

    /// The versions of `id`, in the order they were added; none when the
    /// registry has never seen it.
    pub fn versions(&self, id: &str) -> Result<Vec<Version>, String> {
        let path = self.index_path(id)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(fail(&path, e)),
        };
        text.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(n, l)| {
                serde_json::from_str(l).map_err(|e| format!("{}:{}: {e}", path.display(), n + 1))
            })
            .collect()
    }

    /// The package with hash `sha256`, checked against it.
    pub fn package(&self, sha256: &str) -> Result<Vec<u8>, String> {
        let path = self.package_path(sha256);
        let bytes = std::fs::read(&path).map_err(|e| fail(&path, e))?;
        let got = app_package::sha256_hex(&bytes);
        if got != sha256 {
            return Err(format!(
                "{}: the file's sha256 is {got}, not the {sha256} it is stored as",
                path.display()
            ));
        }
        Ok(bytes)
    }

    /// Add `version` of `id`, whose package is `bytes`. The same version
    /// with the same package is [`Added::Known`]; with another package it
    /// is refused, naming both hashes.
    pub fn add(
        &self,
        _lock: &Lock,
        id: &str,
        version: Version,
        bytes: &[u8],
    ) -> Result<Added, String> {
        let sha = app_package::sha256_hex(bytes);
        if sha != version.sha256 {
            return Err(format!(
                "{id} {}: the package's sha256 is {sha}, not {}",
                version.version, version.sha256
            ));
        }
        semver::Version::parse(&version.version)
            .map_err(|e| format!("{id}: `{}` is not a semantic version: {e}", version.version))?;
        let mut versions = self.versions(id)?;
        if let Some(had) = versions.iter().find(|v| v.version == version.version) {
            if had.sha256 == sha {
                // A package removed by hand comes back.
                if !self.package_path(&sha).is_file() {
                    write_atomic(&self.package_path(&sha), bytes)?;
                }
                return Ok(Added::Known);
            }
            return Err(format!(
                "{id} {} is already in the registry with other contents:\n  \
                 recorded  sha256 {}\n  this one  sha256 {sha}\n\
                 A version names one set of files: raise `version` in app.toml, \
                 or install with --dev while you work on it",
                version.version, had.sha256
            ));
        }
        let package = self.package_path(&sha);
        if !package.is_file() {
            write_atomic(&package, bytes)?;
        }
        versions.push(version);
        let mut text = String::new();
        for v in &versions {
            text.push_str(&serde_json::to_string(v).map_err(|e| e.to_string())?);
            text.push('\n');
        }
        write_atomic(&self.index_path(id)?, text.as_bytes())?;
        Ok(Added::New)
    }

    /// The version to record work in progress on `base` as: the dev
    /// version already holding package `sha256`, else the next
    /// `<base>-dev.N`.
    pub fn dev_version(&self, id: &str, base: &str, sha256: &str) -> Result<String, String> {
        let mut v = semver::Version::parse(base)
            .map_err(|e| format!("{id}: `{base}` is not a semantic version: {e}"))?;
        v.build = semver::BuildMetadata::EMPTY;
        let prefix = if v.pre.is_empty() {
            format!("{v}-dev.")
        } else {
            format!("{v}.dev.")
        };
        let mut last = 0u64;
        for had in self.versions(id)? {
            let Some(n) = had
                .version
                .strip_prefix(&prefix)
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            if had.sha256 == sha256 {
                return Ok(had.version);
            }
            last = last.max(n);
        }
        Ok(format!("{prefix}{}", last + 1))
    }

    /// The version of `id` to install: `wanted` exactly, or else the
    /// highest one that is not yanked and that Dreamer `dreamer` runs.
    pub fn choose(&self, id: &str, wanted: Option<&str>, dreamer: &str) -> Result<Version, String> {
        let versions = self.versions(id)?;
        if versions.is_empty() {
            return Err(format!("{id} is not in the registry"));
        }
        if let Some(w) = wanted {
            let parsed = semver::Version::parse(w).ok();
            return versions
                .into_iter()
                .find(|v| v.version == w || (parsed.is_some() && v.semver() == parsed))
                .ok_or_else(|| format!("{id} {w} is not in the registry"));
        }
        let mut usable: Vec<Version> = versions
            .iter()
            .filter(|v| !v.yanked && v.runs_on(dreamer))
            .cloned()
            .collect();
        usable.sort_by_key(Version::semver);
        usable.pop().ok_or_else(|| {
            let list: Vec<String> = versions
                .iter()
                .map(|v| {
                    let why = if v.yanked {
                        "yanked".to_owned()
                    } else {
                        format!("needs Dreamer {}", v.dreamer.as_deref().unwrap_or("?"))
                    };
                    format!("{} ({why})", v.version)
                })
                .collect();
            format!(
                "no version of {id} this Dreamer ({dreamer}) can install: {}",
                list.join(", ")
            )
        })
    }
}

fn fail(p: &Path, e: std::io::Error) -> String {
    format!("{}: {e}", p.display())
}

/// Write `bytes` beside `path`, then rename it into place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no folder", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| fail(dir, e))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| fail(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(path, e)
    })
}

/// Now, in UTC, as RFC 3339 to the second.
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339(secs)
}

fn rfc3339(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("dreamer-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn package(text: &str) -> (Vec<u8>, String) {
        let bytes = app_package::write(&[app_package::Entry {
            path: "app.toml".into(),
            executable: false,
            data: text.as_bytes().to_vec(),
        }])
        .expect("package");
        let sha = app_package::sha256_hex(&bytes);
        (bytes, sha)
    }

    #[test]
    fn a_version_names_one_package() {
        let dir = temp("one");
        let r = Registry::new(&dir);
        let lock = r.lock().expect("lock");
        let (a, sha_a) = package("a");
        let (b, sha_b) = package("b");
        let v = |version: &str, sha: &str| Version::local(version, sha, None, vec![]);
        assert_eq!(
            r.add(&lock, "me.app", v("0.1.0", &sha_a), &a),
            Ok(Added::New)
        );
        assert_eq!(
            r.add(&lock, "me.app", v("0.1.0", &sha_a), &a),
            Ok(Added::Known)
        );
        let refused = r
            .add(&lock, "me.app", v("0.1.0", &sha_b), &b)
            .expect_err("refused");
        assert!(
            refused.contains(&sha_a) && refused.contains(&sha_b) && refused.contains("--dev"),
            "{refused}"
        );
        assert_eq!(r.package(&sha_a).expect("stored"), a);
        let index = std::fs::read_to_string(dir.join("index/me/app.json")).expect("index");
        assert_eq!(index.lines().count(), 1, "{index}");
        let line: serde_json::Value = serde_json::from_str(index.trim()).expect("json");
        for key in [
            "version",
            "sha256",
            "dreamer",
            "capabilities",
            "yanked",
            "source",
            "added",
        ] {
            assert!(line.get(key).is_some(), "{key}: {line}");
        }
        assert!(r.index_path("../x").is_err() && r.index_path("me./x").is_err());
        assert_eq!(
            r.index_path("old").expect("old"),
            dir.join("index/local/old.json")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dev_versions_rise_and_the_best_runnable_version_is_chosen() {
        let dir = temp("dev");
        let r = Registry::new(&dir);
        let lock = r.lock().expect("lock");
        let add = |text: &str, version: Option<&str>, dreamer: Option<&str>, yanked: bool| {
            let (bytes, sha) = package(text);
            let version = version.map_or_else(
                || r.dev_version("me.app", "0.2.0", &sha).expect("dev"),
                str::to_owned,
            );
            let mut v = Version::local(&version, &sha, dreamer.map(str::to_owned), vec![]);
            v.yanked = yanked;
            r.add(&lock, "me.app", v, &bytes).expect("add");
            version
        };
        assert_eq!(add("one", None, None, false), "0.2.0-dev.1");
        assert_eq!(add("two", None, None, false), "0.2.0-dev.2");
        assert_eq!(
            add("one", None, None, false),
            "0.2.0-dev.1",
            "same files, same version"
        );
        add("old", Some("0.1.0"), None, false);
        add("future", Some("9.0.0"), Some(">=99"), false);
        add("bad", Some("0.3.0"), None, true);
        assert_eq!(
            r.choose("me.app", None, "0.15.0").expect("best").version,
            "0.2.0-dev.2"
        );
        assert_eq!(
            r.choose("me.app", Some("0.1.0"), "0.15.0")
                .expect("named")
                .version,
            "0.1.0"
        );
        assert!(r.choose("me.app", Some("0.4.0"), "0.15.0").is_err());
        assert!(r.choose("other.app", None, "0.15.0").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn times_are_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_833_045), "2026-11-28T02:30:45Z");
    }
}
