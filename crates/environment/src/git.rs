//! Running the user's own `git` on the environment.
//!
//! Always an argument list, never a shell. The user's configuration,
//! identity, hooks and credentials apply, as they would if the user ran
//! git in the directory: the environment is theirs to push. Variables
//! that would point git at another repository are removed, and git never
//! prompts (there is no terminal to answer on).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Variables that would make git act on a repository other than the one
/// in the directory it is run in.
const REDIRECTS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

#[derive(Debug, Clone)]
pub(crate) struct Git {
    pub(crate) dir: PathBuf,
    /// Set on every git command (tests isolate git from the user's
    /// configuration with these).
    pub(crate) env: Vec<(OsString, OsString)>,
}

/// What a missing git looks like to the user.
pub const NO_GIT: &str = "git is not installed, and Dreamer versions the environment with it: \
     install git (on macOS `xcode-select --install`; on Linux your package manager's `git`)";

impl Git {
    pub(crate) fn command(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(&self.dir);
        c.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null());
        for v in REDIRECTS {
            c.env_remove(v);
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    pub(crate) fn output(&self, args: &[&str]) -> Result<Output, String> {
        self.command().args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                NO_GIT.to_owned()
            } else {
                format!("git: {e}")
            }
        })
    }

    /// Run git; its standard output, or its standard error as the reason.
    pub(crate) fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.output(args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(failure(args, &out))
        }
    }

    /// Run git, `None` when it fails (for questions whose "no" is a
    /// failure, such as whether a revision exists).
    pub(crate) fn ask(&self, args: &[&str]) -> Result<Option<String>, String> {
        let out = self.output(args)?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
    }
}

/// `git <verb>: <stderr>`, with a hint for the failures a user can fix.
pub(crate) fn failure(args: &[&str], out: &Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    let verb = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .copied()
        .unwrap_or("");
    let hint = if err.contains("Author identity unknown")
        || err.contains("Please tell me who you are")
        || err.contains("empty ident name")
    {
        "\ngit has no name and email to save with: set them with \
         `git config --global user.name \"Your Name\"` and \
         `git config --global user.email you@example.com`"
    } else {
        ""
    };
    format!("git {verb}: {err}{hint}")
}

/// The canonical form of a path, or the path itself when it cannot be
/// resolved.
pub(crate) fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}
