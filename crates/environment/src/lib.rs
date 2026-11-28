//! Dreamer's environment: the configuration directory, versioned with
//! the user's own git.
//!
//! The environment is everything a person builds in Dreamer that lives in
//! the configuration directory: settings, themes, workspaces, apps,
//! extensions. [`Environment`] turns that directory into a git repository
//! (or adopts the one already there), reports what changed by kind, and
//! saves (commits) it once it validates. Private and machine-specific
//! files stay out through the ignore list of
//! [`workspace_config::env::IGNORED`].
//!
//! Dreamer drives the user's `git` and never stores, reads, or asks for a
//! git or GitHub credential. The user may use git in the directory at any
//! time; every operation here works from whatever state git reports.

mod git;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

pub use git::NO_GIT;

/// The trailer that marks a save Dreamer made on its own.
const TRAILER: &str = "Dreamer-Save";
use git::{Git, canonical};
use workspace_config::env::{ENV_FILE, IGNORED};

/// What a changed file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Settings,
    Themes,
    Workspaces,
    Apps,
    Extensions,
    Environment,
    Other,
}

impl Kind {
    /// The kind of a path relative to the configuration directory.
    pub fn of(path: &str) -> Kind {
        let first = path.split('/').next().unwrap_or(path);
        match first {
            "dreamer.yaml" => Kind::Settings,
            "themes" => Kind::Themes,
            "workspaces" => Kind::Workspaces,
            "apps" => Kind::Apps,
            "extensions.yaml" => Kind::Extensions,
            ENV_FILE | ".gitignore" => Kind::Environment,
            _ => Kind::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Settings => "settings",
            Kind::Themes => "themes",
            Kind::Workspaces => "workspaces",
            Kind::Apps => "apps",
            Kind::Extensions => "extensions",
            Kind::Environment => "environment",
            Kind::Other => "other",
        }
    }
}

/// How a file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    Added,
    Modified,
    Deleted,
}

impl What {
    pub fn sign(self) -> char {
        match self {
            What::Added => '+',
            What::Modified => '~',
            What::Deleted => '-',
        }
    }
}

/// One changed file, relative to the configuration directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub path: String,
    pub kind: Kind,
    pub what: What,
}

/// Why a command cannot work on the repository as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// The directory is not a repository yet.
    NotInitialized,
    /// The directory sits inside another repository (a dotfiles
    /// repository, for example) and has none of its own.
    InsideAnother(PathBuf),
    /// No branch is checked out.
    Detached,
    /// A merge, rebase, cherry-pick, revert or bisect is under way.
    Operation(&'static str),
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocked::NotInitialized => {
                write!(
                    f,
                    "this configuration directory is not an environment yet: run `dream env init`"
                )
            }
            Blocked::InsideAnother(top) => write!(
                f,
                "this configuration directory is inside the git repository {}, so Dreamer does not version it on its own; \
                 run `dream env init --nested` to give it a repository of its own anyway",
                top.display()
            ),
            Blocked::Detached => write!(
                f,
                "no branch is checked out in the environment (detached HEAD): check one out with `git switch`, \
                 then try again; nothing was changed"
            ),
            Blocked::Operation(op) => write!(
                f,
                "a git {op} is under way in the environment: finish or abort it with git, then try again; \
                 nothing was changed"
            ),
        }
    }
}

/// A problem that stops a save: a file that does not validate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub file: PathBuf,
    pub message: String,
}

/// What `status` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// The checked-out branch; `None` when detached.
    pub branch: Option<String>,
    /// The last save, when there is one.
    pub head: Option<Save>,
    /// Changes since the last save.
    pub changes: Vec<Change>,
    /// Files git tracks although the ignore list keeps them out (they
    /// were added before, or by hand): they would be pushed.
    pub tracked_but_ignored: Vec<String>,
    /// Why saving would not work now, if it would not.
    pub blocked: Option<String>,
}

/// One save (commit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Save {
    pub revision: String,
    pub short: String,
    /// Commit time, RFC 3339.
    pub date: String,
    pub message: String,
    /// Made by Dreamer for a change it made itself (autosave).
    pub automatic: bool,
}

/// What `init` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Initialized {
    /// A repository was there and was adopted, history unchanged.
    pub adopted: bool,
    /// The save `init` made; `None` when there was nothing to do.
    pub saved: Option<Save>,
    /// Lines added to `.gitignore`.
    pub ignored: Vec<String>,
    /// `env.toml` was written.
    pub wrote_env: bool,
}

/// Options for [`Environment::init`].
#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    /// The environment's name in `env.toml`; the login name by default.
    pub name: Option<String>,
    /// Make a repository even inside another one.
    pub nested: bool,
}

/// What `save` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Saved {
    /// Nothing changed since the last save.
    Nothing,
    Saved {
        save: Save,
        changes: Vec<Change>,
    },
}

/// What `restore` did, or would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Restored {
    /// The revision restored.
    pub revision: String,
    /// The save made first of what was there, when anything was unsaved:
    /// restoring it undoes the restore.
    pub saved_before: Option<Save>,
    /// The save the restore made; `None` when the files already matched
    /// (or with `dry_run`).
    pub restored: Option<Save>,
    /// How the files change: what the revision adds, changes or removes.
    pub changes: Vec<Change>,
}

/// The configuration directory as an environment.
#[derive(Debug, Clone)]
pub struct Environment {
    git: Git,
}

impl Environment {
    pub fn new(dir: impl Into<PathBuf>) -> Environment {
        Environment {
            git: Git {
                dir: dir.into(),
                env: Vec::new(),
            },
        }
    }

    /// Set these variables on every git command (tests use this to keep
    /// git away from the user's configuration).
    pub fn with_git_env(mut self, vars: Vec<(OsString, OsString)>) -> Environment {
        self.git.env = vars;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.git.dir
    }

    /// Whether the directory has a repository of its own.
    fn own_repository(&self) -> Result<bool, String> {
        let top = self.git.ask(&["rev-parse", "--show-toplevel"])?;
        Ok(top.is_some_and(|t| canonical(Path::new(t.trim())) == canonical(&self.git.dir)))
    }

    /// Why the repository cannot be worked on now, if it cannot.
    pub fn blocked(&self) -> Result<Option<Blocked>, String> {
        if !self.git.dir.is_dir() {
            return Ok(Some(Blocked::NotInitialized));
        }
        if !self.own_repository()? {
            return Ok(Some(
                match self.git.ask(&["rev-parse", "--show-toplevel"])? {
                    Some(top) => Blocked::InsideAnother(PathBuf::from(top.trim())),
                    None => Blocked::NotInitialized,
                },
            ));
        }
        if self.git.ask(&["symbolic-ref", "-q", "HEAD"])?.is_none() {
            return Ok(Some(Blocked::Detached));
        }
        let git_dir = self
            .git
            .run(&["rev-parse", "--absolute-git-dir"])
            .map(|s| PathBuf::from(s.trim()))?;
        for (file, op) in [
            ("MERGE_HEAD", "merge"),
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
            ("BISECT_LOG", "bisect"),
        ] {
            if git_dir.join(file).exists() {
                return Ok(Some(Blocked::Operation(op)));
            }
        }
        Ok(None)
    }

    /// Whether this is an environment: its own repository and `env.toml`.
    pub fn initialized(&self) -> Result<bool, String> {
        Ok(self.git.dir.join(ENV_FILE).is_file() && self.own_repository()?)
    }

    /// Make the directory an environment, or adopt the repository already
    /// there without rewriting its history. Running it again changes
    /// nothing. `version` is this Dreamer's, for `env.toml`.
    pub fn init(&self, opts: &InitOptions, version: &str) -> Result<Initialized, String> {
        std::fs::create_dir_all(&self.git.dir)
            .map_err(|e| format!("{}: {e}", self.git.dir.display()))?;
        let adopted = self.own_repository()?;
        if !adopted {
            if let Some(Blocked::InsideAnother(top)) = self.blocked()?
                && !opts.nested
            {
                return Err(Blocked::InsideAnother(top).to_string());
            }
            self.git.run(&["init", "-q"])?;
            self.git.run(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        } else if let Some(b) = self.blocked()? {
            return Err(b.to_string());
        }

        // The ignore list: missing lines appended, nothing removed.
        let gitignore = self.git.dir.join(".gitignore");
        let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
        let have: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();
        let missing: Vec<String> = IGNORED
            .iter()
            .filter(|l| !have.contains(**l))
            .map(|l| (*l).to_owned())
            .collect();
        if !missing.is_empty() {
            let mut text = existing.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if text.is_empty() {
                text = workspace_config::env::gitignore();
            } else {
                text.push_str(
                    "# Dreamer: never versioned (private, specific to this machine, or rebuilt).\n",
                );
                for l in &missing {
                    text.push_str(l);
                    text.push('\n');
                }
            }
            write_file(&gitignore, &text)?;
        }

        let env_path = self.git.dir.join(ENV_FILE);
        let wrote_env = !env_path.exists();
        if wrote_env {
            let name = opts.name.clone().unwrap_or_else(default_name);
            let text = workspace_config::env::starter(&name, version);
            workspace_config::env::parse_env(&text, Some(&env_path)).map_err(|e| e.to_string())?;
            write_file(&env_path, &text)?;
        } else {
            workspace_config::env::check(&self.git.dir, version)?;
        }

        // A new repository saves everything there; an adopted one only
        // the environment's own files, leaving the user's work as it is.
        let message = if adopted {
            self.git.run(&["add", "--", ".gitignore", ENV_FILE])?;
            "Version this directory as a Dreamer environment"
        } else {
            self.git.run(&["add", "-A"])?;
            "Start the environment"
        };
        let staged = self.git.ask(&["diff", "--cached", "--quiet"])?.is_none();
        let saved = if staged {
            self.git.run(&["commit", "-q", "-m", message])?;
            self.head()?
        } else {
            None
        };
        Ok(Initialized {
            adopted,
            saved,
            ignored: missing,
            wrote_env,
        })
    }

    /// The last save.
    pub fn head(&self) -> Result<Option<Save>, String> {
        Ok(self.log(1)?.into_iter().next())
    }

    /// Changes since the last save, and the repository's state.
    pub fn status(&self) -> Result<Status, String> {
        if let Some(b @ (Blocked::NotInitialized | Blocked::InsideAnother(_))) = self.blocked()? {
            return Err(b.to_string());
        }
        let branch = self
            .git
            .ask(&["symbolic-ref", "-q", "--short", "HEAD"])?
            .map(|s| s.trim().to_owned());
        let tracked_but_ignored = self
            .git
            .run(&[
                "ls-files",
                "-z",
                "--cached",
                "--ignored",
                "--exclude-standard",
            ])?
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Status {
            branch,
            head: self.head()?,
            changes: self.worktree_changes()?,
            tracked_but_ignored,
            blocked: self.blocked()?.map(|b| b.to_string()),
        })
    }

    /// Changes in the working tree against the last save, untracked
    /// files included.
    fn worktree_changes(&self) -> Result<Vec<Change>, String> {
        let out = self.git.run(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])?;
        let mut changes: Vec<Change> = out
            .split('\0')
            .filter(|e| e.len() > 3)
            .map(|e| {
                let (xy, path) = e.split_at(3);
                let xy = xy.as_bytes();
                let what = if xy[0] == b'?' || xy[0] == b'A' {
                    What::Added
                } else if xy[0] == b'D' || xy[1] == b'D' {
                    What::Deleted
                } else {
                    What::Modified
                };
                Change {
                    path: path.to_owned(),
                    kind: Kind::of(path),
                    what,
                }
            })
            .collect();
        changes.sort_by(|a, b| (a.kind, &a.path).cmp(&(b.kind, &b.path)));
        Ok(changes)
    }

    /// Save what changed, with `message` or one made from the changes.
    /// `problems` are what the caller's checks found (see [`validate`]);
    /// with any, nothing is saved unless `force`.
    pub fn save(
        &self,
        message: Option<&str>,
        problems: &[Problem],
        force: bool,
    ) -> Result<Saved, String> {
        if let Some(b) = self.blocked()? {
            return Err(b.to_string());
        }
        let changes = self.worktree_changes()?;
        if changes.is_empty() {
            return Ok(Saved::Nothing);
        }
        if !problems.is_empty() && !force {
            let mut e = String::from("not saved: the environment does not validate");
            for p in problems {
                e.push_str(&format!("\n  {}: {}", p.file.display(), p.message));
            }
            e.push_str("\nfix these, or save anyway with --force");
            return Err(e);
        }
        let message = message
            .map(str::to_owned)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| describe(&changes));
        self.git.run(&["add", "-A"])?;
        self.git.run(&["commit", "-q", "-m", &message])?;
        let save = self
            .head()?
            .ok_or("git committed, but no commit is there")?;
        Ok(Saved::Saved { save, changes })
    }

    /// Saves, newest first, at most `n`.
    pub fn log(&self, n: usize) -> Result<Vec<Save>, String> {
        if self
            .git
            .ask(&["rev-parse", "--verify", "-q", "HEAD"])?
            .is_none()
        {
            return Ok(Vec::new());
        }
        let out = self.git.run(&[
            "log",
            &format!("-n{n}"),
            "-z",
            &format!(
                "--format=%H%x1f%h%x1f%cI%x1f%s%x1f%(trailers:key={TRAILER},valueonly,separator=)"
            ),
        ])?;
        Ok(out
            .split('\0')
            .filter(|r| !r.is_empty())
            .filter_map(|r| {
                let mut f = r.trim_start_matches('\n').split('\x1f');
                Some(Save {
                    revision: f.next()?.to_owned(),
                    short: f.next()?.to_owned(),
                    date: f.next()?.to_owned(),
                    message: f.next()?.to_owned(),
                    automatic: f.next().is_some_and(|t| t.trim() == "automatic"),
                })
            })
            .collect())
    }

    /// What changed between `rev` (the last save by default) and the
    /// files as they are now, untracked files included.
    pub fn diff(&self, rev: Option<&str>) -> Result<Vec<Change>, String> {
        let Some(rev) = self.resolve(rev)? else {
            // Nothing saved yet: everything there is new.
            return self.worktree_changes();
        };
        let out = self
            .git
            .run(&["diff", "--name-status", "-z", "--no-renames", &rev, "--"])?;
        let mut parts = out.split('\0').filter(|s| !s.is_empty());
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
            let what = match status.as_bytes().first() {
                Some(b'A') => What::Added,
                Some(b'D') => What::Deleted,
                _ => What::Modified,
            };
            changes.push(Change {
                path: path.to_owned(),
                kind: Kind::of(path),
                what,
            });
        }
        for c in self.worktree_changes()? {
            if c.what == What::Added && !changes.iter().any(|d| d.path == c.path) {
                changes.push(c);
            }
        }
        changes.sort_by(|a, b| (a.kind, &a.path).cmp(&(b.kind, &b.path)));
        Ok(changes)
    }

    /// The text of [`Environment::diff`]: git's patch, then each untracked
    /// file in full.
    pub fn patch(&self, rev: Option<&str>) -> Result<String, String> {
        let mut out = match self.resolve(rev)? {
            Some(rev) => self.git.run(&["diff", &rev, "--"])?,
            None => String::new(),
        };
        let untracked = self
            .git
            .run(&["ls-files", "-z", "--others", "--exclude-standard"])?;
        for path in untracked.split('\0').filter(|s| !s.is_empty()) {
            // Exit status 1 means "they differ", which they always do.
            let o = self
                .git
                .output(&["diff", "--no-index", "--", "/dev/null", path])?;
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
        Ok(out)
    }

    /// Save the files at `paths` (relative to the configuration directory)
    /// after Dreamer itself changed them, with `message`. Only those files
    /// are saved: other changes, the user's own, stay as they are. Does
    /// nothing when the directory is not an environment, when
    /// `env.autosave` is off, or when nothing at `paths` changed. The
    /// caller's own work must not fail because of this: it reports the
    /// error and carries on.
    pub fn autosave(&self, paths: &[&str], message: &str) -> Result<Option<Save>, String> {
        if paths.is_empty() || !self.initialized()? || !autosave_on(&self.git.dir) {
            return Ok(None);
        }
        if let Some(b) = self.blocked()? {
            return Err(b.to_string());
        }
        let mut add = vec!["add", "-A", "--"];
        add.extend_from_slice(paths);
        self.git.run(&add)?;
        let mut staged = vec!["diff", "--cached", "--quiet", "--"];
        staged.extend_from_slice(paths);
        if self.git.ask(&staged)?.is_some() {
            return Ok(None);
        }
        let trailer = format!("{TRAILER}: automatic");
        let mut commit = vec!["commit", "-q", "-m", message, "-m", &trailer, "--"];
        commit.extend_from_slice(paths);
        self.git.run(&commit)?;
        self.head()
    }

    /// Bring the versioned files back to save `rev`, as a new save on top:
    /// history is never rewritten. `rev` is first unpacked into a scratch
    /// directory beside the configuration directory and checked there with
    /// `check` (see [`validate`]); a revision with problems changes
    /// nothing. What was there is saved first, so restoring that save
    /// undoes this. Ignored files (`local/`, `hosts.yaml`, `memory/`) are
    /// never touched. With `dry_run`, only report what would change.
    pub fn restore(
        &self,
        rev: &str,
        check: &dyn Fn(&Path) -> Vec<Problem>,
        dry_run: bool,
    ) -> Result<Restored, String> {
        if let Some(b) = self.blocked()? {
            return Err(b.to_string());
        }
        let commit = self
            .resolve(Some(rev))?
            .ok_or_else(|| format!("{rev:?} is not a save in this environment"))?;
        let short = self
            .git
            .run(&["rev-parse", "--short", &commit])?
            .trim()
            .to_owned();

        // Files the revision holds that the ignore list now keeps out:
        // restoring would overwrite what is private to this machine.
        let in_rev = self
            .git
            .run(&["ls-tree", "-r", "-z", "--name-only", &commit])?;
        let clashing: Vec<&str> = in_rev
            .split('\0')
            .filter(|p| !p.is_empty())
            .filter(|p| {
                self.git.dir.join(p).exists()
                    && self
                        .git
                        .ask(&["check-ignore", "-q", "--no-index", "--", p])
                        .ok()
                        .flatten()
                        .is_some()
            })
            .collect();
        if !clashing.is_empty() {
            return Err(format!(
                "{short} holds files that are now kept out of the environment ({}); \
                 restoring would overwrite this machine's copies, so nothing was changed. \
                 Take what you need from it with `git -C {} show {short}:PATH`",
                clashing.join(", "),
                self.git.dir.display()
            ));
        }

        // Check the revision where it cannot disturb anything.
        let scratch = scratch_dir(&self.git.dir);
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
        let problems = self.unpack(&commit, &scratch).map(|()| check(&scratch));
        let _ = std::fs::remove_dir_all(&scratch);
        let problems = problems?;
        if !problems.is_empty() {
            let mut e = format!("{short} does not validate, so nothing was changed:");
            for p in problems {
                let file = p.file.strip_prefix(&scratch).unwrap_or(&p.file);
                e.push_str(&format!("\n  {}: {}", file.display(), p.message));
            }
            return Err(e);
        }

        // From what is there now to the revision.
        let changes: Vec<Change> = self
            .diff(Some(&commit))?
            .into_iter()
            .map(|mut c| {
                c.what = match c.what {
                    What::Added => What::Deleted,
                    What::Deleted => What::Added,
                    w => w,
                };
                c
            })
            .collect();
        if dry_run {
            return Ok(Restored {
                revision: short,
                saved_before: None,
                restored: None,
                changes,
            });
        }

        let saved_before = match self.save(Some(&format!("Before restoring {short}")), &[], true)? {
            Saved::Saved { save, .. } => Some(save),
            Saved::Nothing => None,
        };
        // Index and files to the revision: tracked files it lacks are
        // removed, untracked and ignored files are left alone.
        self.git.run(&["read-tree", "--reset", "-u", &commit])?;
        let subject = self
            .git
            .run(&["log", "-1", "--format=%s", &commit])?
            .trim()
            .to_owned();
        let staged = self.git.ask(&["diff", "--cached", "--quiet"])?.is_none();
        let restored = if staged {
            self.git.run(&[
                "commit",
                "-q",
                "-m",
                &format!("Restore {short} ({subject})"),
            ])?;
            self.head()?
        } else {
            None
        };
        Ok(Restored {
            revision: short,
            saved_before,
            restored,
            changes,
        })
    }

    /// Write the files of `commit` into `dir`.
    fn unpack(&self, commit: &str, dir: &Path) -> Result<(), String> {
        let archive = self.git.output(&["archive", "--format=tar", commit])?;
        if !archive.status.success() {
            return Err(git::failure(&["archive"], &archive));
        }
        let mut tar = std::process::Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(dir)
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("tar: {e}"))?;
        if let Some(mut stdin) = tar.stdin.take() {
            use std::io::Write as _;
            stdin
                .write_all(&archive.stdout)
                .map_err(|e| format!("tar: {e}"))?;
        }
        let out = tar.wait_with_output().map_err(|e| format!("tar: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "tar: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// `rev` (or `HEAD`) as a commit, `None` when nothing is saved yet;
    /// an error for a revision that does not exist.
    fn resolve(&self, rev: Option<&str>) -> Result<Option<String>, String> {
        match rev {
            None => Ok(self
                .git
                .ask(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])?
                .map(|s| s.trim().to_owned())),
            Some(r) => self
                .git
                .ask(&["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")])?
                .map(|s| Some(s.trim().to_owned()))
                .ok_or_else(|| {
                    format!("{r:?} is not a save in this environment (see `dream env log`)")
                }),
        }
    }
}

/// A message for a save, from what changed: "settings; 2 workspaces".
pub fn describe(changes: &[Change]) -> String {
    let mut parts = Vec::new();
    let named = |kind: Kind, singular: &str, plural: &str, parts: &mut Vec<String>| {
        let mut names: Vec<String> = changes
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| {
                let mut segs = c.path.split('/');
                segs.next();
                let first = segs.next().unwrap_or(&c.path);
                // Apps are folders; themes and workspaces are files.
                if kind == Kind::Apps {
                    first.to_owned()
                } else {
                    Path::new(first)
                        .file_stem()
                        .map_or_else(|| first.to_owned(), |s| s.to_string_lossy().into_owned())
                }
            })
            .collect();
        names.sort();
        names.dedup();
        match names.len() {
            0 => {}
            1 => parts.push(format!("{singular} {}", names[0])),
            n => parts.push(format!("{n} {plural}")),
        }
    };
    if changes.iter().any(|c| c.kind == Kind::Settings) {
        parts.push("settings".to_owned());
    }
    named(Kind::Themes, "theme", "themes", &mut parts);
    named(Kind::Workspaces, "workspace", "workspaces", &mut parts);
    named(Kind::Apps, "app", "apps", &mut parts);
    if changes.iter().any(|c| c.kind == Kind::Extensions) {
        parts.push("extensions".to_owned());
    }
    if changes.iter().any(|c| c.kind == Kind::Environment) {
        parts.push("environment".to_owned());
    }
    let other: Vec<&Change> = changes.iter().filter(|c| c.kind == Kind::Other).collect();
    match other.len() {
        0 => {}
        1 => parts.push(other[0].path.clone()),
        n => parts.push(format!("{n} other files")),
    }
    if parts.is_empty() {
        "Save".to_owned()
    } else {
        parts.join("; ")
    }
}

/// Check what Dreamer itself reads from the environment in `dir`: every
/// file `dream config check` checks, every workspace, and
/// `extensions.yaml`. Apps are checked by their own runtime, so the
/// caller adds theirs.
pub fn validate(dir: &Path, version: &str) -> Vec<Problem> {
    let mut problems: Vec<Problem> = workspace_config::check::check(dir)
        .into_iter()
        .filter_map(|c| {
            c.problem.map(|message| Problem {
                file: c.file,
                message,
            })
        })
        .collect();
    // `check` reads env.toml with the version it was built with; this is
    // the caller's.
    if let Err(message) = workspace_config::env::check(dir, version) {
        let file = workspace_config::env::env_path(dir);
        if !problems.iter().any(|p| p.file == file) {
            problems.push(Problem { file, message });
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("workspaces")) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml"))
            .collect();
        files.sort();
        for f in files {
            if let Err(errs) = workspace_config::load(&f) {
                problems.push(Problem {
                    message: errs
                        .iter()
                        .map(|e| {
                            // The file is the problem's own; say only where in it.
                            let mut e = e.clone();
                            e.file = None;
                            match (e.line, e.column) {
                                (Some(l), Some(c)) => format!("{l}:{c}: {e}"),
                                (Some(l), None) => format!("{l}: {e}"),
                                _ => e.to_string(),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
                    file: f,
                });
            }
        }
    }
    let ext = dir.join("extensions.yaml");
    if let Ok(text) = std::fs::read_to_string(&ext)
        && let Err(e) = workspace_config::extensions::parse_extensions(&text, Some(&ext))
    {
        problems.push(Problem {
            file: ext,
            message: e.to_string(),
        });
    }
    problems
}

/// `env.autosave` in `dreamer.yaml`; on when the file is absent or does
/// not read (its own problems are reported elsewhere).
fn autosave_on(dir: &Path) -> bool {
    let path = dir.join("dreamer.yaml");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| workspace_config::settings::parse_settings(&t, Some(&path)).ok())
        .is_none_or(|s| s.env.autosave)
}

/// A directory beside the configuration directory (same file system,
/// never inside the repository) to check a revision in.
fn scratch_dir(config: &Path) -> PathBuf {
    let name = config
        .file_name()
        .map_or_else(|| "dreamer".into(), |n| n.to_string_lossy().into_owned());
    config.with_file_name(format!(".{name}.restore-{}", std::process::id()))
}

/// A name for a new environment: the login name, cleaned to what
/// `env.toml` accepts.
fn default_name() -> String {
    let login = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let clean: String = login
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean.is_empty() {
        "dreamer".to_owned()
    } else {
        clean
    }
}

/// Write a file through a temporary beside it, so a reader never sees half.
fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("dreamer-tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, what: What) -> Change {
        Change {
            path: path.to_owned(),
            kind: Kind::of(path),
            what,
        }
    }

    #[test]
    fn files_are_grouped_by_what_they_configure() {
        assert_eq!(Kind::of("dreamer.yaml"), Kind::Settings);
        assert_eq!(Kind::of("themes/nord.toml"), Kind::Themes);
        assert_eq!(Kind::of("workspaces/dev.yaml"), Kind::Workspaces);
        assert_eq!(Kind::of("apps/local.panes/app.ts"), Kind::Apps);
        assert_eq!(Kind::of("extensions.yaml"), Kind::Extensions);
        assert_eq!(Kind::of("env.toml"), Kind::Environment);
        assert_eq!(Kind::of("notes.txt"), Kind::Other);
    }

    #[test]
    fn a_save_message_names_what_changed() {
        let one = [
            change("dreamer.yaml", What::Modified),
            change("workspaces/dev.yaml", What::Added),
            change("apps/local.panes/app.toml", What::Modified),
            change("apps/local.panes/app.ts", What::Modified),
        ];
        assert_eq!(describe(&one), "settings; workspace dev; app local.panes");
        let many = [
            change("themes/a.toml", What::Added),
            change("themes/b.toml", What::Deleted),
            change("x.txt", What::Added),
            change("y.txt", What::Added),
        ];
        assert_eq!(describe(&many), "2 themes; 2 other files");
    }
}
