//! Dreamer Web: Chromium for web panes, as a component installed apart
//! from the app, so an install that only wants terminals stays small.
//!
//! On macOS the component is a directory holding CEF's framework, the five
//! helper apps Chromium starts its renderer, GPU, and utility processes
//! from (each signed), and [`MANIFEST`], which names the Dreamer and CEF
//! versions it was built for. A helper loads the framework from beside
//! itself, so the directory works wherever it is; Dreamer installs it as
//! `web/` in its data directory ([`installed_dir`]). Packed, it is
//! [`archive_name`], a zip whose one top directory is [`TOP`].
//!
//! The packager (`cargo xtask package`), the installer (`dream web
//! install`), and the desktop's engine all read the layout from here. The
//! Linux component (an optional `dreamer-web` archive) is not built yet:
//! [`TARGET`] is `None` there.

use std::path::{Path, PathBuf};

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

/// The helper executable as cargo builds it; each helper app holds a copy.
pub const HELPER_EXE: &str = "dreamer-web-helper";

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
pub const HOW_TO_ADD: &str = "Add it with `dream web install`.";

/// The name of a helper app's executable: `dreamer Helper (GPU)`.
pub fn helper_name(suffix: &str) -> String {
    format!("dreamer Helper{suffix}")
}

/// A helper app's directory name: `dreamer Helper (GPU).app`.
pub fn helper_app(suffix: &str) -> String {
    format!("{}.app", helper_name(suffix))
}

/// The executable of the helper app with `suffix` in component `dir`.
pub fn helper_executable(dir: &Path, suffix: &str) -> PathBuf {
    let name = helper_name(suffix);
    dir.join(helper_app(suffix))
        .join("Contents/MacOS")
        .join(name)
}

/// Where Dreamer installs the component: `web/` in its data directory
/// (`DREAMER_DATA_DIR`, else `$XDG_DATA_HOME/dreamer`, else
/// `~/.local/share/dreamer`).
pub fn installed_dir() -> PathBuf {
    platform::data_dir().join("web")
}

/// The archive of the component for Dreamer `version` on `target`.
pub fn archive_name(version: &str, target: &str) -> String {
    format!("dreamer-web-{version}-{target}.zip")
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
    if Manifest::this_build().as_ref() != Some(&found) {
        return State::Other(found);
    }
    if !dir.join(FRAMEWORK_BINARY).is_file() {
        return State::Broken(format!("{FRAMEWORK_BINARY} is missing"));
    }
    for (suffix, _) in HELPERS {
        if !helper_executable(dir, suffix).is_file() {
            return State::Broken(format!("{} is missing", helper_app(suffix)));
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
                 (CEF {CEF_VERSION}). Install this Dreamer's with `dream web install`.",
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
    fn lay_out(dir: &Path, manifest: &Manifest) {
        std::fs::create_dir_all(dir.join(FRAMEWORK)).expect("framework dir");
        std::fs::write(dir.join(FRAMEWORK_BINARY), b"").expect("framework");
        for (suffix, _) in HELPERS {
            let exe = helper_executable(dir, suffix);
            std::fs::create_dir_all(exe.parent().expect("parent")).expect("helper dir");
            std::fs::write(exe, b"").expect("helper");
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
                .contains("dream web install")
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
        std::fs::remove_file(helper_executable(&dir, " (GPU)")).expect("write");
        assert!(matches!(check(&dir), State::Broken(why) if why.contains("(GPU)")));
        let _ = std::fs::remove_dir_all(&dir);
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
}
