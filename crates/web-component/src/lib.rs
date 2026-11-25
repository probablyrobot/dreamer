//! Dreamer Web: Chromium for web panes, as a component installed apart
//! from the app, so an install that only wants terminals stays small.
//!
//! On macOS the component is a directory holding CEF's framework, the five
//! helper apps Chromium starts its renderer, GPU, and utility processes
//! from (each signed), and [`MANIFEST`], which names the Dreamer and CEF
//! versions it was built for. On Linux (x86-64) it holds `libcef.so`,
//! Chromium's resources (`.pak` files, ICU data, the V8 snapshot, the
//! locales, SwiftShader), the one helper executable, and the manifest.
//! Either way a helper loads the library from beside itself, so the
//! directory works wherever it is; Dreamer installs it as `web/` in its
//! data directory ([`installed_dir`]), and the desktop loads the library
//! from there at run time: neither `dreamer` nor the helper links it.
//! Packed, it is [`archive_name`] (a zip on macOS, a gzipped tarball on
//! Linux) whose one top directory is [`TOP`].
//!
//! The packager (`cargo xtask package`), the installer ([`install`],
//! which `dream web install` and the desktop share), and the desktop's
//! engine all read the layout from here.
//!
//! A copy already on the system can be used in place of a download
//! ([`system_dirs`]): `web/` is then a link to it, made only after the copy
//! passed the same checks as an install.

use std::path::{Path, PathBuf};

pub mod install;

/// The component's name, as people see it.
pub const NAME: &str = "Dreamer Web";

/// The Dreamer version this build is, which an installed component must
/// have been built for: its helpers are this build's own.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The CEF release the desktop is built against (`deps/baseline.toml`,
/// and the `cef` crate's own version string, which the engine checks this
/// against when it is compiled).
pub const CEF_VERSION: &str = "156.0.30+g6028375+chromium-156.0.8078.96";

/// The platform the component is built for on this host, if there is one.
pub const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else {
    None
};

/// The directory an archive of the component unpacks to.
pub const TOP: &str = "dreamer-web";

/// The component's description of itself, in its directory.
pub const MANIFEST: &str = "component.json";

/// CEF's framework bundle, in the component's directory.
pub const FRAMEWORK: &str = "Chromium Embedded Framework.framework";

/// The framework's library, in the component's directory.
pub const FRAMEWORK_BINARY: &str =
    "Chromium Embedded Framework.framework/Chromium Embedded Framework";

/// The library the desktop and the helpers load, in the component's
/// directory: the framework's on macOS, `libcef.so` on Linux.
pub const LIBRARY: &str = if cfg!(target_os = "macos") {
    FRAMEWORK_BINARY
} else {
    "libcef.so"
};

/// The helper executable as cargo builds it; on macOS each helper app holds
/// a copy, on Linux it is in the component's directory as it is.
pub const HELPER_EXE: &str = "dreamer-web-helper";

/// Linux: what the component holds besides its manifest, from the CEF
/// distribution (all but the helper). Chromium finds its resources, ICU
/// data, and V8 snapshot where it is told (the component's directory) and
/// SwiftShader beside `libcef.so`, its software GPU when there is no other.
pub const LINUX_FILES: [&str; 11] = [
    "libcef.so",
    HELPER_EXE,
    "icudtl.dat",
    "v8_context_snapshot.bin",
    "resources.pak",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "locales/en-US.pak",
    "libvk_swiftshader.so",
    "libvulkan.so.1",
    "vk_swiftshader_icd.json",
];

/// The helper apps, by the suffix of their name (`dreamer Helper
/// (GPU).app`) and the end of their bundle identifier. Chromium finds the
/// others beside the first one, by these suffixes.
pub const HELPERS: [(&str, &str); 5] = [
    ("", "helper"),
    (" (GPU)", "helper.gpu"),
    (" (Renderer)", "helper.renderer"),
    (" (Plugin)", "helper.plugin"),
    (" (Alerts)", "helper.alerts"),
];

/// How to add the component, for messages.
pub const HOW_TO_ADD: &str = "Add it from the web pane, with \"Add Dreamer Web\" in the palette, or with `dream web install`.";

/// About how much the component's archive is to download, and how much it
/// takes installed, in megabytes (decimal), for what people are asked.
pub const DOWNLOAD_MB: u32 = if cfg!(target_os = "macos") { 150 } else { 160 };
pub const INSTALLED_MB: u32 = if cfg!(target_os = "macos") { 334 } else { 378 };

/// The variable that says where to download the component from instead
/// of the release ([`download_url`]).
pub const URL_ENV: &str = "DREAMER_WEB_URL";

/// For tests: the variable that replaces where copies already on the
/// system are looked for ([`system_dirs`]) with its own list of
/// directories (`:`-separated; relative entries are ignored), so a check
/// can offer a copy of its own and never reads a real system location.
pub const SYSTEM_DIRS_ENV: &str = "DREAMER_WEB_SYSTEM_DIRS";

/// The name of a helper app's executable: `dreamer Helper (GPU)`.
pub fn helper_name(suffix: &str) -> String {
    format!("dreamer Helper{suffix}")
}

/// A helper app's directory name: `dreamer Helper (GPU).app`.
pub fn helper_app(suffix: &str) -> String {
    format!("{}.app", helper_name(suffix))
}

/// The helper CEF is given to start its processes from, in component
/// `dir`: the first helper app's executable on macOS (Chromium finds the
/// others beside it), the helper itself on Linux.
pub fn helper(dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        helper_executable(dir, "")
    } else {
        dir.join(HELPER_EXE)
    }
}

/// The files a complete component on this platform holds besides its
/// manifest, relative to its directory.
pub fn required_files() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        let mut files = vec![PathBuf::from(FRAMEWORK_BINARY)];
        files.extend(
            HELPERS
                .iter()
                .map(|(suffix, _)| helper_executable(Path::new(""), suffix)),
        );
        files
    } else {
        LINUX_FILES.iter().map(PathBuf::from).collect()
    }
}

/// The macOS executable of the helper app with `suffix` in component `dir`.
pub fn helper_executable(dir: &Path, suffix: &str) -> PathBuf {
    let name = helper_name(suffix);
    dir.join(helper_app(suffix))
        .join("Contents/MacOS")
        .join(name)
}

/// Where Dreamer installs the component: `web/` in its data directory
/// (`DREAMER_DATA_DIR`, else `$XDG_DATA_HOME/dreamer`, else
/// `~/.local/share/dreamer`). It may be a link to a copy used in place
/// ([`linked_to`]).
pub fn installed_dir() -> PathBuf {
    platform::data_dir().join("web")
}

/// Where `dir` links to, when it is a link to a copy used in place rather
/// than a component installed there.
pub fn linked_to(dir: &Path) -> Option<PathBuf> {
    std::fs::symlink_metadata(dir)
        .ok()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| std::fs::read_link(dir).ok())
}

/// Where a copy of the component may already be on this machine, to use in
/// place of a download, in the order they are tried. Each is used only if
/// it is exactly this build's component and passes an install's checks.
///
/// - The system-wide location, for every user of the machine: on macOS
///   `/Library/Application Support/Dreamer/web` (an administrator puts it
///   there); on Linux a distribution's `dreamer-web` package
///   (`/usr/lib/dreamer-web`), or CEF of exactly the pinned release laid
///   out as the component (`/usr/lib/cef`, `/opt/cef`).
/// - Another Dreamer data directory of this user's: the default one
///   (`$XDG_DATA_HOME/dreamer/web`, `~/.local/share/dreamer/web`) when
///   `DREAMER_DATA_DIR` points elsewhere.
///
/// A system Chrome or Chromium browser is not CEF, and is never used.
///
/// [`SYSTEM_DIRS_ENV`], when set, replaces all of these
/// ([`system_dirs_override`]).
pub fn system_dirs() -> Vec<PathBuf> {
    if let Some(dirs) = system_dirs_override() {
        return dirs;
    }
    let mut dirs: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Library/Application Support/Dreamer/web")]
    } else {
        ["/usr/lib/dreamer-web", "/usr/lib/cef", "/opt/cef"]
            .into_iter()
            .map(PathBuf::from)
            .collect()
    };
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share/dreamer/web"));
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("dreamer/web"));
    let here = installed_dir();
    for d in [xdg, home].into_iter().flatten() {
        if d != here && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// The directories [`SYSTEM_DIRS_ENV`] names, when it is set.
pub fn system_dirs_override() -> Option<Vec<PathBuf>> {
    dirs_from(std::env::var_os(SYSTEM_DIRS_ENV).as_deref())
}

/// The absolute directories in a `:`-separated list, if there is a list.
fn dirs_from(list: Option<&std::ffi::OsStr>) -> Option<Vec<PathBuf>> {
    list.map(|l| {
        std::env::split_paths(l)
            .filter(|p| p.is_absolute())
            .collect()
    })
}

/// The archive of the component for Dreamer `version` on `target`: a zip
/// for macOS, a gzipped tarball for Linux (as the platform's base package
/// is).
pub fn archive_name(version: &str, target: &str) -> String {
    format!(
        "dreamer-web-{version}-{target}.{}",
        archive_extension(target)
    )
}

/// The extension of the component's archive for `target`.
pub fn archive_extension(target: &str) -> &'static str {
    if target.contains("-apple-") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Whether `name` (a path or an address) names an archive of the
/// component rather than a directory.
pub fn is_archive(name: &str) -> bool {
    name.ends_with(".zip") || name.ends_with(".tar.gz")
}

/// Where a release publishes that archive (its checksum is beside it, with
/// `.sha256` added).
pub fn release_url(version: &str, target: &str) -> String {
    format!(
        "{}/releases/download/v{version}/{}",
        env!("CARGO_PKG_REPOSITORY"),
        archive_name(version, target)
    )
}

/// The archive's address given `base`: `base` itself when it names an
/// archive ([`is_archive`]), else this version's archive in the directory
/// `base` names.
pub fn archive_url(base: &str, target: &str) -> String {
    let base = base.trim();
    if is_archive(base) {
        base.to_owned()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            archive_name(VERSION, target)
        )
    }
}

/// Where to download this build's component from: [`URL_ENV`] if it is
/// set, else `setting` (`web.url` in `dreamer.yaml`), else the release
/// ([`release_url`]). Either may name the archive or a directory holding
/// it ([`archive_url`]).
pub fn download_url(setting: Option<&str>, target: &str) -> String {
    let env = std::env::var(URL_ENV).ok();
    match env
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .or(setting.filter(|u| !u.trim().is_empty()))
    {
        Some(base) => archive_url(base, target),
        None => release_url(VERSION, target),
    }
}

/// What [`MANIFEST`] says: the Dreamer version whose helpers the component
/// holds, the CEF release of its framework, and its platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub cef: String,
    pub target: String,
}

impl Manifest {
    /// The manifest of a component built with this build, on a platform
    /// that has one.
    pub fn this_build() -> Option<Manifest> {
        Some(Manifest {
            version: VERSION.to_owned(),
            cef: CEF_VERSION.to_owned(),
            target: TARGET?.to_owned(),
        })
    }

    pub fn to_json(&self) -> String {
        let v = serde_json::json!({
            "component": TOP,
            "version": self.version,
            "cef": self.cef,
            "target": self.target,
        });
        serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
    }

    pub fn parse(text: &str) -> Result<Manifest, String> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("{MANIFEST}: {e}"))?;
        if v["component"].as_str() != Some(TOP) {
            return Err(format!("{MANIFEST}: not a {NAME} component"));
        }
        let field = |k: &str| {
            v[k].as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{MANIFEST}: no `{k}`"))
        };
        Ok(Manifest {
            version: field("version")?,
            cef: field("cef")?,
            target: field("target")?,
        })
    }

    /// Chromium's version, from the CEF version string
    /// (`...+chromium-156.0.8078.96`).
    pub fn chromium(&self) -> &str {
        self.cef
            .split_once("chromium-")
            .map_or(self.cef.as_str(), |(_, c)| c)
    }
}

/// What a directory holds, as the component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The component, built for this Dreamer, with every file in place.
    Ready(Manifest),
    /// Nothing there.
    Missing,
    /// A component built for another Dreamer or CEF release.
    Other(Manifest),
    /// Something there, but not a usable component, and why.
    Broken(String),
}

/// Whether `dir` holds the component this build needs.
pub fn check(dir: &Path) -> State {
    check_for(dir, Manifest::this_build().as_ref())
}

/// Whether `dir` holds a complete component with manifest `want` (`None`:
/// no component can be this build's, on a platform without one).
pub fn check_for(dir: &Path, want: Option<&Manifest>) -> State {
    let text = match std::fs::read_to_string(dir.join(MANIFEST)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !dir.exists() => {
            return State::Missing;
        }
        Err(e) => return State::Broken(format!("{}: {e}", dir.join(MANIFEST).display())),
    };
    let found = match Manifest::parse(&text) {
        Ok(m) => m,
        Err(e) => return State::Broken(e),
    };
    if want != Some(&found) {
        return State::Other(found);
    }
    for file in required_files() {
        if !dir.join(&file).is_file() {
            return State::Broken(format!("{} is missing", file.display()));
        }
    }
    State::Ready(found)
}

impl State {
    /// Why web pages cannot be drawn with what is in `dir`, and how to fix
    /// it; `None` when the component is ready.
    pub fn problem(&self, dir: &Path) -> Option<String> {
        let dir = dir.display();
        match self {
            State::Ready(_) => None,
            State::Missing => Some(format!(
                "Web pages need {NAME} (Chromium), which is installed apart from Dreamer. \
                 {HOW_TO_ADD}"
            )),
            State::Other(m) => Some(format!(
                "{NAME} in {dir} is for Dreamer {} (CEF {}), not for this Dreamer {VERSION} \
                 (CEF {CEF_VERSION}). {HOW_TO_ADD}",
                m.version, m.cef
            )),
            State::Broken(why) => Some(format!(
                "{NAME} in {dir} is not usable ({why}). {HOW_TO_ADD}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("web-component-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A component directory with every file the check looks for.
    pub(crate) fn lay_out(dir: &Path, manifest: &Manifest) {
        for file in required_files() {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, b"").expect("file");
        }
        std::fs::write(dir.join(MANIFEST), manifest.to_json()).expect("write");
    }

    #[test]
    fn only_a_complete_component_of_this_build_is_ready() {
        let Some(this) = Manifest::this_build() else {
            return;
        };
        let dir = temp("ready");
        assert_eq!(check(&dir), State::Missing);
        assert!(
            State::Missing
                .problem(&dir)
                .expect("a problem")
                .contains("Add Dreamer Web")
        );
        lay_out(&dir, &this);
        assert_eq!(check(&dir), State::Ready(this.clone()));
        assert_eq!(check(&dir).problem(&dir), None);

        // Helpers of another Dreamer are not this build's.
        let older = Manifest {
            version: "0.0.1".into(),
            ..this.clone()
        };
        std::fs::write(dir.join(MANIFEST), older.to_json()).expect("write");
        assert_eq!(check(&dir), State::Other(older));

        std::fs::write(dir.join(MANIFEST), this.to_json()).expect("write");
        let helper = helper(&dir);
        std::fs::remove_file(&helper).expect("remove");
        let name = helper
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(check(&dir), State::Broken(why) if why.contains(&name)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_test_list_of_system_copies_keeps_only_absolute_directories() {
        use std::ffi::OsStr;
        assert_eq!(dirs_from(None), None);
        assert_eq!(dirs_from(Some(OsStr::new(""))), Some(Vec::new()));
        assert_eq!(
            dirs_from(Some(OsStr::new("/tmp/a/web:relative/web::/tmp/b"))),
            Some(vec![PathBuf::from("/tmp/a/web"), PathBuf::from("/tmp/b")])
        );
        // Set, it is the whole list: none of the real locations.
        if let Some(only) = system_dirs_override() {
            assert_eq!(system_dirs(), only);
        }
    }

    #[test]
    fn the_manifest_reads_back_and_names_chromium() {
        let m = Manifest {
            version: "1.2.3".into(),
            cef: CEF_VERSION.into(),
            target: "aarch64-apple-darwin".into(),
        };
        assert_eq!(Manifest::parse(&m.to_json()), Ok(m.clone()));
        assert_eq!(m.chromium(), "156.0.8078.96");
        assert!(Manifest::parse("{\"component\":\"other\"}").is_err());
    }

    #[test]
    fn a_download_address_names_the_archive_or_its_directory() {
        let t = "aarch64-apple-darwin";
        let zip = archive_name(VERSION, t);
        assert_eq!(
            archive_url("https://example.com/web/", t),
            format!("https://example.com/web/{zip}")
        );
        assert_eq!(
            archive_url("https://example.com/x.zip", t),
            "https://example.com/x.zip"
        );
        assert_eq!(
            archive_url("file:///tmp/d", t),
            format!("file:///tmp/d/{zip}")
        );
        // Linux's is a tarball.
        let linux = "x86_64-unknown-linux-gnu";
        let tarball = archive_name(VERSION, linux);
        assert!(tarball.ends_with(".tar.gz"), "{tarball}");
        assert_eq!(
            archive_url(&format!("https://example.com/{tarball}"), linux),
            format!("https://example.com/{tarball}")
        );
        assert_eq!(
            archive_url("https://example.com/web", linux),
            format!("https://example.com/web/{tarball}")
        );
        // Without the variable: the setting, then the release.
        if std::env::var_os(URL_ENV).is_none() {
            assert_eq!(download_url(None, t), release_url(VERSION, t));
            assert_eq!(download_url(Some(" "), t), release_url(VERSION, t));
            assert_eq!(download_url(Some("https://h/a.zip"), t), "https://h/a.zip");
        }
    }
}
